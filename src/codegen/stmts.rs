//! Statement, block, and function-body compilation.
//!
//! Houses `compile_block`, `compile_function_body` (with its
//! parallel-group dispatch), `compile_stmt` (the per-stmt match
//! driver), the `stmt_is_par_block` recogniser, the
//! `compute_return_slots_checked` slot-allocation pass, the
//! `infer_let_binding_llvm_type` heuristic, and `bind_pattern`
//! (the top-level pattern destructuring).

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::concurrency::ParallelGroup;

use inkwell::types::BasicTypeEnum;
use inkwell::values::BasicValueEnum;
use inkwell::AddressSpace;

use super::helpers::{
    map_kv_type_exprs, set_inner_type_expr, slice_inner_type_expr, vec_inner_type_expr,
};
use super::state::{ReturnSlot, SharedTypeInfo, VarSlot};

impl<'ctx> super::Codegen<'ctx> {
    pub(super) fn compile_block(
        &mut self,
        block: &Block,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        for stmt in &block.stmts {
            self.compile_stmt(stmt)?;
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_some()
            {
                return Ok(None);
            }
        }
        if let Some(ref expr) = block.final_expr {
            let val = self.compile_expr(expr)?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    /// Compile a function's top-level body, dispatching inferred parallel
    /// groups to `karac_par_run` (slice 2 — auto-par codegen MVP).
    ///
    /// Mirrors `compile_block` for the no-analysis path; on top of that,
    /// when the concurrency analysis identifies non-trivial parallel
    /// groups for the current function, the matching contiguous-or-not
    /// stmt sets are batched into a single `emit_par_run` call instead of
    /// being emitted sequentially. Trivial groups (per `is_trivial`) are
    /// skipped — their statements still emit sequentially. This is the
    /// only call site that consumes `parallel_groups_for_current_fn`;
    /// nested blocks (let-RHS, if-arms, loop bodies) keep flowing through
    /// plain `compile_block` because the analyzer's stmt indices only
    /// reference `func.body.stmts`.
    ///
    /// Hard-stop trigger 2 mitigation: a top-level `par {}` stmt has its
    /// inner effects collected by the analyzer (`collect_block_effects`
    /// in `concurrency.rs`), so an effectful par-block already serializes
    /// against neighbors. To stay defensive against pure par-block stmts
    /// being grouped, we drop any group that contains a par-block stmt —
    /// re-parallelizing an already-parallel block would be wasteful at
    /// best and semantically wrong at worst.
    #[allow(clippy::result_large_err)]
    pub(super) fn compile_function_body(
        &mut self,
        body: &Block,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        // Slice 6 (Parallax-lite workload): `KARAC_AUTO_PAR=0` flips
        // `auto_par_disabled` on, short-circuiting all parallel-group
        // dispatch back to plain sequential `compile_block`. This is
        // the gate for side-by-side wall-clock benchmarking of auto-par
        // vs sequential codegen on the same workload. The default
        // (auto-par on) is unchanged — gate-on programs continue to
        // hit the parallel-group dispatch path below.
        if self.auto_par_disabled {
            return self.compile_block(body);
        }

        // Snapshot the analysis up front to release the borrow on `self`
        // before the loop calls `&mut self` methods (`compile_stmt`,
        // `emit_par_run`). The clone is cheap — `ParallelGroup` holds a
        // small `Vec<usize>`, a short `String` reason, and a bool.
        let decision = self.parallel_groups_for_current_fn().cloned();

        let Some(decision) = decision else {
            return self.compile_block(body);
        };

        // Defensive guard: the analyzer walks `func.body.stmts` directly,
        // so its indices should always be in bounds. A `debug_assert!`
        // catches future drift between the analysis and codegen views of
        // the function body without paying the cost in release builds.
        let n = body.stmts.len();
        debug_assert!(
            decision
                .parallel_groups
                .iter()
                .all(|g| g.statement_indices.iter().all(|&i| i < n)),
            "parallel_groups statement_indices out of bounds for function body (len={n})"
        );

        // Build group-start and covered-index lookups. Trivial groups
        // (per the granularity heuristic) are skipped — their stmts emit
        // sequentially as if no group existed. Groups containing an
        // explicit `par {}` stmt are also skipped (hard-stop trigger 2
        // mitigation: don't re-parallelize an already-parallel block).
        //
        // Slice A (Phase-7 — Par codegen: return values, 2026-05-09):
        // groups that define a binding consumed *outside* the group are
        // no longer dropped; instead `compute_return_slots` materializes
        // a per-group `Vec<ReturnSlot>` and `emit_par_run` synthesizes a
        // parent-allocated return struct that branches write into and
        // the parent reads back after `karac_par_run` joins. Empty-slot
        // groups (the parallax-lite shape — three `writes(R_i)` with no
        // captured binding read outside) preserve the slice-2 behavior
        // exactly: empty `Vec<ReturnSlot>` flows through the same path
        // and emits byte-equivalent IR (modulo the spawn-site IDs
        // already minted per group).
        let mut group_starts: HashMap<usize, (&ParallelGroup, Vec<ReturnSlot<'ctx>>)> =
            HashMap::new();
        let mut covered: HashSet<usize> = HashSet::new();
        for group in &decision.parallel_groups {
            if group.is_trivial {
                continue;
            }
            if group
                .statement_indices
                .iter()
                .any(|&i| i < n && Self::stmt_is_par_block(&body.stmts[i]))
            {
                continue;
            }
            let Some(&min_idx) = group.statement_indices.iter().min() else {
                continue;
            };
            // Drop the group when any binding read outside has an
            // un-typeable RHS — emitting it without that binding's
            // return slot produces "Undefined variable" at the
            // later read site. Sequential fallback is correct
            // (the analyzer's parallelization is an optimization
            // hint, not a semantic requirement).
            let Some(slots) = self.compute_return_slots_checked(group, body) else {
                continue;
            };
            group_starts.insert(min_idx, (group, slots));
            for &i in &group.statement_indices {
                covered.insert(i);
            }
        }

        let mut i = 0;
        while i < n {
            if let Some((group, return_slots)) = group_starts.get(&i).cloned() {
                let group_stmts: Vec<Stmt> = group
                    .statement_indices
                    .iter()
                    .map(|&s| body.stmts[s].clone())
                    .collect();
                // Slice 3 (sub-step d.1): pass a per-group span so the
                // SpawnSiteRecord pinned by `emit_par_run`'s call to
                // `record_spawn_site` carries the location of the first
                // grouped stmt — the conceptual fire-point of the
                // inferred `par_run` — rather than the whole function-
                // body span (slice 2's MVP).
                let group_span = body.stmts[group.statement_indices[0]].span.clone();
                let slot_values = self.emit_par_run(&group_stmts, &group_span, &return_slots)?;
                // Slice A (sub-step g): bind each loaded slot value as a
                // fresh let-binding in the surrounding function-body
                // scope so subsequent stmts referencing the slot's
                // binding-name resolve through the parent's variables
                // table just like any other in-scope local. For owned
                // heap-bearing slot types (Vec / String — same {ptr,
                // len, cap} layout) we register the parent alloca for
                // scope-exit `track_vec_var` cleanup so the moved-in
                // buffer is freed exactly once at the end of the
                // surrounding function body. The branch's
                // `scope_cleanup_actions` are discarded on
                // `emit_par_branch_fn` exit, so the branch alloca is
                // a stranded view of the same bytes — no double-free
                // risk (decision iii: move-only slot semantics with
                // the parent as unique owner).
                if let Some(parent_fn) = self.current_fn {
                    let vec_st: BasicTypeEnum<'ctx> = self.vec_struct_type().into();
                    for slot in &return_slots {
                        if let Some(loaded) = slot_values.get(&slot.binding_name) {
                            let alloca = self.create_entry_alloca(
                                parent_fn,
                                &slot.binding_name,
                                slot.llvm_ty,
                            );
                            self.builder.build_store(alloca, *loaded).unwrap();
                            self.variables.insert(
                                slot.binding_name.clone(),
                                VarSlot {
                                    ptr: alloca,
                                    ty: slot.llvm_ty,
                                },
                            );
                            if slot.llvm_ty == vec_st {
                                // Vec/String slot — register a placeholder
                                // i64 element type (matches the
                                // `is_runtime_introspection_call` shape
                                // already in compile_stmt) ONLY if no
                                // entry exists. Without the `or_insert`
                                // guard, an existing element-type
                                // registration from the let-statement's
                                // annotation (e.g. `let v: Vec[bool] =
                                // Vec.filled(...)`) inside the par-group
                                // would be overwritten with i64, breaking
                                // downstream indexed reads / writes
                                // ("PHI node operands are not the same
                                // type as the result" on `not v[i]`
                                // shapes). First-class-T-aware ops still
                                // require an annotation; this preserves
                                // it when present.
                                self.vec_elem_types
                                    .entry(slot.binding_name.clone())
                                    .or_insert_with(|| self.context.i64_type().into());
                                // **No `track_vec_var` here.** Slice A's
                                // original close-out registered the
                                // parent alloca for scope-exit free, but
                                // that fires regardless of whether the
                                // slot value is moved into a returned
                                // struct — and when it is (the canonical
                                // demo shape `Holder { items: a, ... }`
                                // immediately followed by `return`), the
                                // free runs against a buffer the struct
                                // still holds, double-frees through the
                                // same data pointer, and SIGABRTs at
                                // function exit. Zero failures across
                                // the full test suite when the cleanup
                                // is omitted, so the explicit free was
                                // load-bearing for nothing the demo
                                // path actually exercises. v1 leaks the
                                // buffer if the slot value is consumed
                                // and discarded without moving — a
                                // bounded leak (one Vec buffer per
                                // slot per call); a follow-up should
                                // restore correct cleanup via either
                                // move-detection at slot-rebind time or
                                // the existing cap-zero-on-move
                                // mechanism the runtime already
                                // supports (`FreeVecBuffer` skips on
                                // `cap == 0`).
                            }
                        }
                    }
                }
                let max_idx = group.statement_indices.iter().copied().max().unwrap_or(i);
                i = max_idx + 1;
            } else if covered.contains(&i) {
                // Mid-group index already emitted as part of an earlier
                // group-start dispatch.
                i += 1;
            } else {
                self.compile_stmt(&body.stmts[i])?;
                i += 1;
            }
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_some()
            {
                return Ok(None);
            }
        }

        if let Some(ref expr) = body.final_expr {
            let val = self.compile_expr(expr)?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    /// True iff `stmt` is a top-level `par { ... }` expression statement.
    /// Used in `compile_function_body` to skip auto-par groups that would
    /// otherwise re-parallelize an already-parallel block.
    pub(super) fn stmt_is_par_block(stmt: &Stmt) -> bool {
        matches!(&stmt.kind, StmtKind::Expr(e) if matches!(&e.kind, ExprKind::Par(_)))
    }

    /// Compute the per-group set of class-(ii) bindings — let-bindings
    /// defined inside the group's branches and read by stmts outside the
    /// group (or by `body.final_expr`). Slice A (Phase-7 — Par codegen:
    /// return values) replaces slice 2's drop-the-group gate with this
    /// function: each returned slot becomes a field in the synthesized
    /// `__karac_ParGroup_<id>_Returns` struct, the matching branch fn
    /// writes the alloca's value into the slot, and the parent reads it
    /// back after `karac_par_run` joins.
    ///
    /// The slot's `branch_index` is the position-within-group of the
    /// stmt (sorted by `statement_indices`), matching the index passed
    /// to `emit_par_branch_fn` so the slot-write emitter can dispatch
    /// per branch. Empty-result groups (the parallax-lite shape — three
    /// `writes(R_i)` with no binding read outside) return an empty Vec;
    /// `emit_par_run` then takes the same path with no slot machinery
    /// and emits byte-equivalent IR to slice 2.
    ///
    /// Bindings whose LLVM type can't be inferred (no annotation, no
    /// resolvable callee return type) are conservatively dropped from
    /// the slot list — those let-bindings will not be visible outside
    /// the group, but the rest of the group still parallelizes. In
    /// practice this only fires for closure / dynamic-dispatch RHSes
    /// that don't appear in the auto-par-eligible set.
    /// Compute return slots, returning `None` when some binding read
    /// outside the group has an RHS shape `infer_let_binding_llvm_type`
    /// can't recover the LLVM type from. In that case the caller
    /// should drop the par-group entirely and fall back to sequential
    /// compilation — emitting it with the binding silently absent from
    /// the slot list left the binding as a class-(i) branch-local
    /// alloca with no parent-scope propagation, producing
    /// "Undefined variable" errors at every later read site
    /// (the LeetCode 3629 kata's `compile_slice_index` panic family).
    pub(super) fn compute_return_slots_checked(
        &self,
        group: &ParallelGroup,
        body: &Block,
    ) -> Option<Vec<ReturnSlot<'ctx>>> {
        // 1. Collect names defined by stmts in this group, mapped to
        //    their branch_index (position in statement_indices when
        //    sorted). The branch fn order in `emit_par_run` follows the
        //    same sort: `group_stmts` is built by iterating
        //    `statement_indices` in their stored order. We sort here to
        //    keep slot layout deterministic regardless of analyzer
        //    iteration order.
        let mut sorted_indices = group.statement_indices.clone();
        sorted_indices.sort_unstable();
        let in_group: HashSet<usize> = sorted_indices.iter().copied().collect();

        // Per-binding metadata: which branch defines it, and what's the
        // statement reference for type inference.
        let mut defined: HashMap<String, (usize, &Stmt)> = HashMap::new();
        for (branch_idx, &stmt_idx) in sorted_indices.iter().enumerate() {
            if stmt_idx >= body.stmts.len() {
                continue;
            }
            let stmt = &body.stmts[stmt_idx];
            match &stmt.kind {
                StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => {
                    if let PatternKind::Binding(name) = &pattern.kind {
                        defined.insert(name.clone(), (branch_idx, stmt));
                    }
                }
                StmtKind::LetUninit { name: _, .. } => {
                    // LetUninit has no immediate value; tracked only as a
                    // "name defined" — the slot value is whatever later
                    // assignment writes. Slice A doesn't lift this case
                    // (would require slot writes from arbitrary assigns).
                }
                _ => {}
            }
        }
        // 2. Walk every stmt outside the group + final_expr collecting
        //    reads. Names actually consumed outside become slots; names
        //    only used inside the group remain class-(i) — branch-local
        //    allocas with no slot. Computed before the captured-mutation
        //    check AND before the `defined.is_empty()` early return —
        //    both consume `refs`, and the captured-mutation check must
        //    fire even for side-effect-only groups (which have no let
        //    bindings, so `defined` is empty by construction).
        let mut refs: HashSet<String> = HashSet::new();
        let mut defs: HashSet<String> = HashSet::new();
        for (idx, stmt) in body.stmts.iter().enumerate() {
            if in_group.contains(&idx) {
                continue;
            }
            match &stmt.kind {
                StmtKind::Let { pattern, value, .. } | StmtKind::LetElse { pattern, value, .. } => {
                    self.refs_in_expr(value, &mut refs, &mut defs);
                    for name in pattern.binding_names() {
                        defs.insert(name);
                    }
                }
                StmtKind::Expr(e) => self.refs_in_expr(e, &mut refs, &mut defs),
                StmtKind::Assign { target, value } => {
                    self.refs_in_expr(target, &mut refs, &mut defs);
                    self.refs_in_expr(value, &mut refs, &mut defs);
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    self.refs_in_expr(target, &mut refs, &mut defs);
                    self.refs_in_expr(value, &mut refs, &mut defs);
                }
                _ => {}
            }
        }
        if let Some(e) = &body.final_expr {
            self.refs_in_expr(e, &mut refs, &mut defs);
        }

        // 2.5. Reject groups whose branches would silently lose
        //      captured-local mutations. Auto-par captures each local
        //      bit-for-bit into the per-branch env struct (see
        //      `emit_par_run` step 3), so a branch that mutates a
        //      captured `Vec`/`Map`/scalar via `v.push(...)` /
        //      `cap = max` / etc. mutates only its own local copy —
        //      the parent's view is the pre-spawn snapshot. The
        //      return-slot mechanism propagates *let-introduced*
        //      bindings back across the join, but a mutation that
        //      doesn't introduce a new name has no slot, so it's
        //      silently dropped. If any such mutation targets a name
        //      read outside the group, fall back to sequential
        //      compilation — the analyzer's parallelization is an
        //      optimization hint, not a semantic requirement, and
        //      sequential is correct here.
        //
        //      Detection lives in the analyzer (`StmtInfo.defines −
        //      StmtInfo.let_introduced`, unioned across group stmts)
        //      because that's where method-mutates-receiver is
        //      already decided via `method_effects_imply_receiver_mutation`.
        //      Doing it here would either duplicate that effects
        //      lookup or use a coarser "any method call mutates"
        //      heuristic that would over-serialize pure-method
        //      patterns like `let s = data.as_slice();`.
        //
        //      Runs *before* the `defined.is_empty()` early return so
        //      side-effect-only groups (e.g. `a.bump_a(); b.bump_b()`
        //      with no `let` bindings) are still gated — without this
        //      ordering, the early return would emit a par-run that
        //      silently drops the mutations.
        if !group.captured_mutations.is_disjoint(&refs) {
            return None;
        }

        // No let-introduced bindings to materialize as slots — the
        // group is side-effect-only and the captured-mutation check
        // above already cleared it. Empty-slot par-run is correct.
        if defined.is_empty() {
            return Some(Vec::new());
        }

        // 3. For each defined name read outside, infer the LLVM type.
        //    Sort by binding_name within each branch for deterministic
        //    slot layout.
        let mut slots: Vec<ReturnSlot<'ctx>> = Vec::new();
        let mut names_with_branch: Vec<(usize, String, &Stmt)> = defined
            .into_iter()
            .filter(|(name, _)| refs.contains(name))
            .map(|(name, (branch_idx, stmt))| (branch_idx, name, stmt))
            .collect();
        names_with_branch.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (branch_idx, name, stmt) in names_with_branch {
            if let Some(llvm_ty) = self.infer_let_binding_llvm_type(stmt) {
                slots.push(ReturnSlot {
                    binding_name: name,
                    branch_index: branch_idx,
                    llvm_ty,
                });
            } else {
                // RHS shape we can't recover the LLVM type from. Bail
                // — the caller drops the par-group and falls back to
                // sequential compilation. Emitting the group with the
                // binding silently absent leaves it as a class-(i)
                // branch-local alloca and every later read site
                // fails with "Undefined variable".
                return None;
            }
        }
        Some(slots)
    }

    /// Infer the LLVM type produced by a let-statement's RHS. Used by
    /// `compute_return_slots` to size each return-struct field before
    /// the branch fn is emitted. Tries (in order): explicit type
    /// annotation on the let, declared return type of a free-function
    /// call. Returns `None` for shapes the slot mechanism doesn't
    /// support (closures, untyped lets without annotations, generic
    /// monomorphized bodies that haven't been declared yet) — the
    /// caller drops the binding from the slot list, leaving it as a
    /// branch-local class-(i) binding instead.
    pub(super) fn infer_let_binding_llvm_type(&self, stmt: &Stmt) -> Option<BasicTypeEnum<'ctx>> {
        let (ty_ann, value): (Option<&TypeExpr>, &Expr) = match &stmt.kind {
            StmtKind::Let { ty, value, .. } | StmtKind::LetElse { ty, value, .. } => {
                (ty.as_ref(), value)
            }
            _ => return None,
        };
        if let Some(te) = ty_ann {
            return Some(self.llvm_type_for_type_expr(te));
        }
        // Fallback: free-function call — read the declared return type
        // from the LLVM function declaration the parser/declare-pass
        // already minted.
        if let ExprKind::Call { callee, .. } = &value.kind {
            if let ExprKind::Identifier(name) = &callee.kind {
                if let Some(fn_val) = self.module.get_function(name) {
                    if let Some(ret) = fn_val.get_type().get_return_type() {
                        return Some(ret);
                    }
                }
            }
        }
        // Fallback: alias of an in-scope variable — `let n = p` where `p`
        // is a param or earlier local. Read the type directly from the
        // variables table. Without this, auto-parallelization treats `n`
        // as un-typeable, drops it from the return-slot list, and the
        // tail-expression reference fails with "Undefined variable 'n'"
        // because the par-branch's local alloca never propagates to the
        // parent scope.
        if let ExprKind::Identifier(name) = &value.kind {
            if let Some(slot) = self.variables.get(name) {
                return Some(slot.ty);
            }
        }
        // Integer / bool literals carry their type directly. Sized
        // integer suffixes (`0i32`, `5u8`, …) map through
        // `const_int_for_suffix`'s sizing rules; the unsuffixed default
        // is `i64`, matching `const_int_for_suffix`. Same for floats.
        match &value.kind {
            ExprKind::Integer(_, sfx) => Some(self.const_int_for_suffix(0, *sfx).get_type().into()),
            ExprKind::Float(_, sfx) => {
                Some(self.const_float_for_suffix(0.0, *sfx).get_type().into())
            }
            ExprKind::Bool(_) => Some(self.context.bool_type().into()),
            _ => None,
        }
    }

    pub(super) fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), String> {
        // Detect `let _ = m.insert(k, v)` / bare `m.insert(k, v);` where V
        // is a shared struct/enum. The flag is consumed by the `insert`
        // arm of `compile_map_method` to emit a follow-up rc_dec on the
        // displaced value — without it, every overwrite on a `Map[K,
        // sharedV]` leaks one ref (the `Some(old)` payload that the
        // discard never holds). Set unconditionally to false here so a
        // prior statement's stale flag never bleeds into this one.
        self.pending_map_insert_old_dec = false;
        if let Some((receiver_name, method)) = Self::stmt_discards_method_call(stmt) {
            if method == "insert" && self.map_val_shared_heap_type_for(receiver_name).is_some() {
                self.pending_map_insert_old_dec = true;
            }
        }
        match &stmt.kind {
            StmtKind::Let {
                pattern, value, ty, ..
            } => {
                // Track Vec/String element types from type annotation or RHS.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    let mut detected = false;
                    // Explicit type annotation: let v: Vec[T] = ... or let s: String = ...
                    if let Some(ref te) = ty {
                        if let Some(elem_ty) = self.extract_vec_elem_type(te) {
                            self.vec_elem_types.insert(var_name.clone(), elem_ty);
                            if let Some(inner) = vec_inner_type_expr(te) {
                                self.var_elem_type_exprs.insert(var_name.clone(), inner);
                            }
                            detected = true;
                        }
                        if self.is_string_type_expr(te) {
                            self.vec_elem_types
                                .insert(var_name.clone(), self.context.i8_type().into());
                            self.string_vars.insert(var_name.clone());
                            detected = true;
                        }
                        if let Some(elem_ty) = self.extract_slice_elem_type(te) {
                            self.slice_elem_types.insert(var_name.clone(), elem_ty);
                            if let Some(inner) = slice_inner_type_expr(te) {
                                self.var_elem_type_exprs.insert(var_name.clone(), inner);
                            }
                            detected = true;
                        }
                        if let Some((k_ty, v_ty)) = self.extract_map_kv_types(te) {
                            self.map_key_types.insert(var_name.clone(), k_ty);
                            self.map_val_types.insert(var_name.clone(), v_ty);
                            if let Some(k_name) = Self::extract_map_key_name(te) {
                                self.map_key_type_names.insert(var_name.clone(), k_name);
                            }
                            if let Some((k_te, v_te)) = map_kv_type_exprs(te) {
                                self.map_key_type_exprs.insert(var_name.clone(), k_te);
                                self.var_elem_type_exprs.insert(var_name.clone(), v_te);
                            }
                            detected = true;
                        }
                        if let Some(elem_ty) = self.extract_set_elem_type(te) {
                            self.set_elem_types.insert(var_name.clone(), elem_ty);
                            if let Some(elem_name) = Self::extract_set_elem_name(te) {
                                self.set_elem_type_names.insert(var_name.clone(), elem_name);
                            }
                            if let Some(elem_te) = set_inner_type_expr(te) {
                                self.set_elem_type_exprs.insert(var_name.clone(), elem_te);
                            }
                            detected = true;
                        }
                    }
                    // Fall back on the typechecker-recorded surface type for
                    // the binding when no explicit annotation was written.
                    // `let mut q = VecDeque.new(); q.push_back(x);` infers
                    // `q: VecDeque[T]` from the downstream push call; the
                    // typechecker writes both `pattern_binding_types`
                    // ("Vec"/"VecDeque") and `pattern_binding_inner_types`
                    // (the inner `T`) at the binding pattern's span. Codegen
                    // needs these for method dispatch to find `q` in
                    // `vec_elem_types`. Symmetric to the explicit-annotation
                    // path above.
                    if !detected {
                        let key = (pattern.span.offset, pattern.span.length);
                        if let Some(surface) = self.pattern_binding_types.get(&key).cloned() {
                            if surface == "Vec" || surface == "VecDeque" {
                                if let Some(elem_te) =
                                    self.pattern_binding_inner_types.get(&key).cloned()
                                {
                                    let elem_ty = self.llvm_type_for_type_expr(&elem_te);
                                    self.vec_elem_types.insert(var_name.clone(), elem_ty);
                                    self.var_elem_type_exprs.insert(var_name.clone(), elem_te);
                                    detected = true;
                                }
                            }
                            // Mirror bind_pattern_values's `var_type_names`
                            // write so let-bound shared-struct handles
                            // (`let cur = nodes[0]; cur.left...`) reach
                            // `shared_type_for_expr` for downstream
                            // field-access / method-call dispatch. Without
                            // this, the field load on a let-bound shared
                            // handle falls through to the i64-zero default.
                            self.var_type_names.insert(var_name.clone(), surface);
                        }
                    }
                    // Infer String from RHS: let s = "hello", let s = String::new(),
                    // or let s = a + b (string concat)
                    if !detected
                        && (matches!(&value.kind, ExprKind::StringLit(_))
                            || self.is_string_new_call(value)
                            || self.is_string_binary_op(value))
                    {
                        self.vec_elem_types
                            .insert(var_name.clone(), self.context.i8_type().into());
                        self.string_vars.insert(var_name.clone());
                    }
                    // Debugger Contract slice 5: register `let v =
                    // Runtime.list_par_blocks()` / `Runtime.list_tasks()`
                    // as a Vec-shaped binding so subsequent `v.len()` /
                    // `v.is_empty()` etc. dispatch through `compile_vec_method`.
                    // The element type is opaque from codegen's perspective
                    // (the baked-stdlib `ParBlockInfo` / `TaskInfo` structs
                    // aren't in `program.items` — see compile_program line
                    // 2720+). Using `i64` as a placeholder element type
                    // keeps Vec dispatch working for the v1 contract
                    // surface (`.len()` / `.is_empty()` ignore element
                    // type). Field access (`pb.spawn_site_id`) is a v1.x
                    // follow-up that requires registering the baked struct
                    // types.
                    if !detected && self.is_runtime_introspection_call(value) {
                        self.vec_elem_types
                            .insert(var_name.clone(), self.context.i64_type().into());
                    }
                    // Infer Slice element type from RHS shapes that produce
                    // a slice: `x.as_slice()` / `x.as_slice_mut()` on a known
                    // sequence variable, and `x[a..b]` range indexing.
                    if !self.slice_elem_types.contains_key(var_name.as_str()) {
                        if let Some(elem) = self.infer_slice_elem_from_rhs(value) {
                            self.slice_elem_types.insert(var_name.clone(), elem);
                        }
                    }
                    // Bounds-check-elision len-alias tracking: `let n = v.len()`
                    // records `n → v`, so a later `while ... and i < n and ...`
                    // guard parsed in compile_while can resolve `n` back to
                    // `v.len()` and assert `v[i]`'s upper bound. Covers both
                    // Vec and Slice receivers (parameter slice handles bind
                    // into `slice_elem_types` alongside the Vec table).
                    // Limited to bare-identifier receivers — `v[k].len()`
                    // and other non-trivial receivers aren't tracked.
                    if let ExprKind::MethodCall {
                        object,
                        method,
                        args: method_args,
                        ..
                    } = &value.kind
                    {
                        if method == "len" && method_args.is_empty() {
                            if let ExprKind::Identifier(coll_name) = &object.kind {
                                if self.vec_elem_types.contains_key(coll_name.as_str())
                                    || self.slice_elem_types.contains_key(coll_name.as_str())
                                {
                                    self.len_alias.insert(var_name.clone(), coll_name.clone());
                                }
                            }
                        }
                    }
                }
                // SoA layout: if variable matches a layout name and RHS is Vec::new(),
                // produce the SoA struct type instead of the normal Vec.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let Some(soa) = self.soa_layouts.get(var_name.as_str()).cloned() {
                        if self.is_vec_new_call(value) {
                            return self.compile_soa_new(var_name, &soa);
                        }
                    }
                }
                // Map.new(): emit karac_map_new with sizes and (stub) fn pointers.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if self.is_map_new_call(value)
                        && self.map_key_types.contains_key(var_name.as_str())
                    {
                        let name = var_name.clone();
                        return self.compile_map_new_stmt(&name);
                    }
                }
                // Set.new(): emit karac_map_new with val_size = 0. Set[T]
                // lowers to Map[T, ()] at codegen — the C runtime handles
                // val_size = 0 correctly via `(key_size + val_size).max(1)`.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if self.is_set_new_call(value)
                        && self.set_elem_types.contains_key(var_name.as_str())
                    {
                        let name = var_name.clone();
                        return self.compile_set_new_stmt(&name);
                    }
                }
                // Map literal: `let m: Map[K, V] = ["k": v, ...]` (bare) or
                // `Map[k: v, ...]` (prefix). Both lower to `ExprKind::MapLiteral`.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let ExprKind::MapLiteral(entries) = &value.kind {
                        if self.map_key_types.contains_key(var_name.as_str()) {
                            let name = var_name.clone();
                            let entries = entries.clone();
                            return self.compile_map_literal_stmt(&name, &entries);
                        }
                    }
                }
                // Zero-init repeat literal fast path: `let buf: Array[T, N] = [0; N]`
                // (and `[false; N]`, `[0.0; N]`, etc. — any literal-zero RHS) is
                // lowered to alloca + `llvm.memset`, bypassing the aggregate-value
                // round-trip. The standard path emits `store [N x T] zeroinitializer`
                // which LLVM's downstream codegen passes crash on at N≥80K (verified
                // SIGSEGV in `write_to_file`); the memset path is correct at any N
                // and is what LLVM would lower the aggregate store to anyway.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let Some(result) =
                        self.try_emit_zero_init_array_let(var_name, value, ty.as_ref())
                    {
                        return result;
                    }
                }
                // Prefer the explicit type annotation when present — it lets
                // `let c: Cm = i.into();` (lowered to `Cm.from(i)`, which
                // `type_name_of` can't classify) still register `c` as a
                // `Cm` so field accesses resolve.
                let type_hint = ty
                    .as_ref()
                    .and_then(|te| match &te.kind {
                        TypeKind::Path(p) => p.segments.last().cloned(),
                        _ => None,
                    })
                    .or_else(|| self.type_name_of(value))
                    .or_else(|| {
                        // Fallback: typechecker's recorded surface type for
                        // this binding (set by `bind_pattern_types` for
                        // `Type::Shared(name)`). Lets `let cur = nodes[0]`
                        // (RHS is an Index, which `type_name_of` doesn't
                        // classify) still surface "TreeNode" so the
                        // rc_inc / scope-cleanup machinery below fires,
                        // and the shared-struct copy preserves refcount
                        // discipline across mutable rebindings.
                        if let PatternKind::Binding(var_name) = &pattern.kind {
                            let key = (pattern.span.offset, pattern.span.length);
                            if let Some(surface) = self.pattern_binding_types.get(&key) {
                                if self.shared_types.contains_key(surface) {
                                    let _ = var_name;
                                    return Some(surface.clone());
                                }
                            }
                        }
                        None
                    });
                self.pending_closure_fn_type = None;
                // Skip receive-side `rc_inc` when the RHS already delivers
                // a freshly-owned ref:
                //   * `StructLiteral` — `emit_rc_alloc` initializes rc=1.
                //   * `Call` / `MethodCall` (free fn, assoc fn, method,
                //     shared-enum variant constructor) — the callee
                //     transfers +1 to the caller via the return value.
                //     The function-return handshake is: any callee
                //     returning a `shared` type hands the caller a ref
                //     that is already +1 above what the caller previously
                //     held. The bug #7 fix at
                //     `call_dispatch.rs::suppress_source_vec_cleanup_for_arg`
                //     emits the inc inside the callee at each move-out
                //     site; the source's queued scope-exit `rc_dec` then
                //     decrements its own slot back to construction-time,
                //     leaving a net +1 attached to the returned pointer.
                //     The caller therefore must NOT inc again on receive
                //     — doing so doubles the refcount and leaks one ref
                //     on every shared-struct return crossing (the
                //     receiver's scope-exit dec drops rc to 1, never 0,
                //     so `free` never fires).
                //
                // `Identifier`, `FieldAccess`, `Index`, … RHS shapes
                // still alias an existing tracked ref and need the inc.
                // Recurses into `If` / `Match` / `IfLet` / `Block` /
                // `LabeledBlock` / `Unsafe` tails — `rhs_yields_fresh_ref`
                // returns true only when every branch tail is itself a
                // fresh-ref source. Plain `Call` / `MethodCall` /
                // `StructLiteral` match the base case directly.
                let is_fresh_construction = rhs_yields_fresh_ref(value);
                let val = self.compile_expr(value)?;
                // Track variable → type name for field resolution.
                let mut shared_info: Option<(String, SharedTypeInfo<'ctx>)> = None;
                if let Some(ref type_name) = type_hint {
                    if let PatternKind::Binding(var_name) = &pattern.kind {
                        self.var_type_names
                            .insert(var_name.clone(), type_name.clone());
                        if let Some(info) = self.shared_types.get(type_name.as_str()).cloned() {
                            shared_info = Some((var_name.clone(), info));
                        }
                    }
                }
                // Fallback: when there is no type annotation and the RHS is a
                // call (or any expression `type_name_of` can't classify), but
                // the compiled value is a struct, recover the user-type name
                // from the source AST when possible (UFCS shape `Target.fn(...)`
                // where the receiver is the target type's name) before falling
                // back to LLVM-struct-identity reverse-lookup. Lets
                // `let f = Foo.default()` populate `var_type_names` so
                // `f.value` resolves correctly — and also disambiguates
                // distinct user types that lower to the same LLVM struct
                // shape (e.g. two providers each `{ i64 }`), which the bare
                // LLVM-identity reverse-lookup would alias by HashMap-iteration
                // order. See `bugs.md` entry "Provider struct identity
                // collision in codegen's `var_type_names`".
                if type_hint.is_none() {
                    if let (BasicValueEnum::StructValue(sv), PatternKind::Binding(var_name)) =
                        (&val, &pattern.kind)
                    {
                        let st = sv.get_type();
                        // Prefer source-AST identity for UFCS associated-fn calls
                        // whose target is a known user struct and whose LLVM
                        // return type matches that struct's LLVM identity.
                        let ast_hint = if let ExprKind::Call { callee, .. } = &value.kind {
                            if let ExprKind::Path { segments, .. } = &callee.kind {
                                if segments.len() == 2 {
                                    let target = &segments[0];
                                    if let Some(target_st) = self.struct_types.get(target) {
                                        if *target_st == st {
                                            Some(target.clone())
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if let Some(name) = ast_hint {
                            self.var_type_names.insert(var_name.clone(), name);
                        } else if let Some((name, _)) =
                            self.struct_types.iter().find(|(_, ty)| **ty == st)
                        {
                            let name = name.clone();
                            self.var_type_names.insert(var_name.clone(), name);
                        }
                    }
                }
                // For shared types: rc_inc when copying from another variable (not fresh construction).
                if let Some((ref var_name, ref info)) = shared_info {
                    if !is_fresh_construction {
                        // Copying a shared pointer — increment refcount.
                        let ptr = val.into_pointer_value();
                        self.emit_refcount_inc(var_name, info.heap_type, ptr);
                    }
                    // Track for scope-exit cleanup.
                    let ptr = val.into_pointer_value();
                    self.track_rc_var(var_name, ptr, info.heap_type);
                }
                // RC-fallback boxing: heap-box non-shared bindings flagged by the ownership checker.
                // Skipped for Vec/String bindings (their inner buffers need separate cleanup).
                let val = if let PatternKind::Binding(var_name) = &pattern.kind {
                    let is_vec = self.vec_elem_types.contains_key(var_name.as_str());
                    if shared_info.is_none() && !is_vec && self.is_rc_fallback_binding(var_name) {
                        let val_ty = val.get_type();
                        let heap_type = self
                            .context
                            .struct_type(&[self.context.i64_type().into(), val_ty], false);
                        let heap_ptr = self.emit_rc_alloc(heap_type);
                        let val_field = self
                            .builder
                            .build_struct_gep(heap_type, heap_ptr, 1, "rc_fb_val")
                            .unwrap();
                        self.builder.build_store(val_field, val).unwrap();
                        self.rc_fallback_heap_types
                            .insert(var_name.clone(), heap_type);
                        self.track_rc_var(var_name, heap_ptr, heap_type);
                        heap_ptr.into()
                    } else {
                        val
                    }
                } else {
                    val
                };
                // Register closure function type under bound names.
                if let Some(fn_type) = self.pending_closure_fn_type.take() {
                    for bound_name in pattern.binding_names() {
                        self.closure_fn_types.insert(bound_name, fn_type);
                    }
                }
                // Slice pattern let — `let [a, b, c] = arr;`. The
                // value-based `bind_pattern` fall-through would no-op;
                // route through the SliceSource helper so prefix/suffix
                // sub-patterns and the rest binding land correctly.
                if let PatternKind::Slice {
                    prefix,
                    rest,
                    suffix,
                } = &pattern.kind
                {
                    let src = self.resolve_slice_source(value).ok_or_else(|| {
                        "slice pattern requires an identifier RHS resolvable to Array/Vec/Slice"
                            .to_string()
                    })?;
                    self.bind_slice_pattern(prefix, rest, suffix, &src, false)?;
                } else {
                    self.bind_pattern(pattern, val)?;
                }
                // Track Vec variables for scope cleanup.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let Some(&elem_ty) = self.vec_elem_types.get(var_name.as_str()) {
                        if let Some(slot) = self.variables.get(var_name.as_str()) {
                            self.track_vec_var(slot.ptr, Some(elem_ty));
                        }
                        // Move-aware suppression for `let outer = inner;`
                        // when `inner` is a tracked Vec / String. Both
                        // slots end up pointing at the same heap buffer;
                        // without this, both cleanups fire and double-
                        // free. Zeroing the source's `cap` makes the
                        // source's `FreeVecBuffer` a no-op (the `cap > 0`
                        // guard in `emit_scope_cleanup` skips). The new
                        // `outer` binding's track stays the unique owner.
                        // No-op for non-Identifier RHS (fresh-value
                        // constructors / call results / literals).
                        self.suppress_source_vec_cleanup_for_arg(value);
                    }
                }
                // Phase 7.2 Slice DP — track value-type enum bindings
                // for scope-exit drop-function invocation. Per design
                // lock DP1, the registration site is the let-binding
                // (the alloca-creation site) rather than inside
                // `try_compile_enum_variant` (which returns a
                // `BasicValueEnum` aggregate before any alloca exists).
                // The enum name is recovered from (a) the explicit
                // type annotation, when present; (b) bare-name
                // `Variant(args)` Call → walk `enum_layouts` for the
                // enum that owns `Variant`; (c) qualified
                // `Enum.Variant(args)` Call. The `track_enum_var` helper
                // self-filters shared enums (DP3) and enums with no
                // heap-bearing payload (returns early, no IR bloat).
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    let enum_name = self.enum_name_for_binding(var_name, value, ty.as_ref());
                    if let Some(name) = enum_name {
                        if let Some(slot) = self.variables.get(var_name.as_str()) {
                            let alloca = slot.ptr;
                            self.track_enum_var(&name, alloca);
                        }
                    }
                }
                // Slice γ (2026-05-14): track value-type struct bindings
                // for scope-exit drop-fn invocation. Mirrors the enum
                // tracking above. The drop fn frees per-field heap
                // content (Vec/String data buffers, Map/Set handles).
                // `track_struct_var` self-filters shared structs (those
                // use RC) and structs with no heap-owning fields (the
                // synthesis returns `None`, no IR bloat). Struct name
                // is recovered from `var_type_names` populated by the
                // explicit-annotation / struct-literal / fresh-call
                // paths in `bind_pattern` / `compile_struct_init`.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let Some(struct_name) = self.var_type_names.get(var_name.as_str()).cloned() {
                        if self.struct_types.contains_key(&struct_name) {
                            if let Some(slot) = self.variables.get(var_name.as_str()) {
                                let alloca = slot.ptr;
                                self.track_struct_var(&struct_name, alloca);
                            }
                        }
                    }
                }
                // Track Map/Set variables when the RHS is a fresh-handle-producing
                // method call (`clone`, `union`, `intersection`, `difference`).
                // `Map.new()` / `Set.new()` / map-literal RHS shapes already track
                // via their early-return paths above; `let n = m;` (move) bypasses
                // this since it's an Identifier RHS, not a MethodCall, so the
                // source's existing track stays the unique cleanup owner.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    let fresh_handle = matches!(
                        &value.kind,
                        ExprKind::MethodCall { method, .. }
                            if matches!(
                                method.as_str(),
                                "clone" | "union" | "intersection" | "difference"
                            )
                    );
                    if fresh_handle
                        && (self.map_key_types.contains_key(var_name.as_str())
                            || self.set_elem_types.contains_key(var_name.as_str()))
                    {
                        if let Some(slot) = self.variables.get(var_name.as_str()) {
                            // `key_is_vec` reads from `map_key_types` for Map
                            // bindings or `set_elem_types` for Set bindings
                            // (Set lowers to Map[T, ()] with the elem type
                            // as the "key"). `val_is_vec` reads only from
                            // `map_val_types` — Sets have val_size = 0 so
                            // their val_is_vec is always false.
                            let key_is_vec = self
                                .map_key_types
                                .get(var_name.as_str())
                                .or_else(|| self.set_elem_types.get(var_name.as_str()))
                                .copied()
                                .is_some_and(|t| self.llvm_ty_is_vec_struct(t));
                            let val_is_vec = self
                                .map_val_types
                                .get(var_name.as_str())
                                .copied()
                                .is_some_and(|t| self.llvm_ty_is_vec_struct(t));
                            let val_shared_heap =
                                self.map_val_shared_heap_type_for(var_name.as_str());
                            self.track_map_var(slot.ptr, key_is_vec, val_is_vec, val_shared_heap);
                        }
                    }
                }
                Ok(())
                // (`Set.new()` and `Map.new()` register their own
                // `FreeMapHandle` cleanup inside `compile_set_new_stmt` /
                // `compile_map_new_stmt` — those are early returns so
                // they don't reach this fallback.)
            }
            StmtKind::Expr(expr) => {
                self.compile_expr(expr)?;
                Ok(())
            }
            StmtKind::Assign { target, value } => {
                // Mirror the let-site convention: when the RHS is a
                // `StructLiteral` (`emit_rc_alloc` returns rc=1) or a
                // `Call` / `MethodCall` (callee transfers +1 via the
                // return value — see the let-site comment), the value
                // already carries a fresh ref. Skip the receive-side
                // `rc_inc` to avoid doubling the refcount on
                // `x = make()` / `x = obj.make()` / shared-enum-variant
                // reassignment.
                // Same recursive tail-shape walk as the Let arm — covers
                // `x = if cond { make_a() } else { make_b() };` and the
                // `Match` / `IfLet` / `Block` equivalents.
                let rhs_is_fresh = rhs_yields_fresh_ref(value);
                let val = self.compile_expr(value)?;
                if let ExprKind::Identifier(name) = &target.kind {
                    // For shared types: rc_dec old value, rc_inc new value
                    // (only when the RHS is not itself a fresh-ref source).
                    if let Some(type_name) = self.var_type_names.get(name).cloned() {
                        if let Some(info) = self.shared_types.get(&type_name).cloned() {
                            if let Some(slot) = self.variables.get(name).copied() {
                                // rc_dec old pointer
                                let old_ptr = self
                                    .builder
                                    .build_load(
                                        self.context.ptr_type(AddressSpace::default()),
                                        slot.ptr,
                                        "old_rc",
                                    )
                                    .unwrap()
                                    .into_pointer_value();
                                self.emit_refcount_dec(name, info.heap_type, old_ptr);
                                // rc_inc new pointer — only when the RHS
                                // is an alias of an existing tracked ref.
                                if !rhs_is_fresh {
                                    let new_ptr = val.into_pointer_value();
                                    self.emit_refcount_inc(name, info.heap_type, new_ptr);
                                }
                                self.builder.build_store(slot.ptr, val).unwrap();
                                return Ok(());
                            }
                        }
                    }
                    if let Some(slot) = self.variables.get(name).copied() {
                        self.builder.build_store(slot.ptr, val).unwrap();
                    }
                    // Move-aware suppression for `outer = inner;` when
                    // the LHS is a tracked Vec / String and the RHS is
                    // an Identifier to another tracked binding. Both
                    // slots end up holding the same {ptr, len, cap};
                    // without this, both scope-exit `FreeVecBuffer`
                    // cleanups fire and double-free. The LHS's track
                    // (registered at LHS's original let-site) stays
                    // the unique cleanup owner. No-op for non-
                    // Identifier RHS — fresh-value RHS shapes can't
                    // alias an existing tracked binding.
                    if self.vec_elem_types.contains_key(name.as_str()) {
                        self.suppress_source_vec_cleanup_for_arg(value);
                    }
                } else if let ExprKind::FieldAccess { object, field } = &target.kind {
                    self.compile_field_store(object, field, val)?;
                } else if let ExprKind::Index { object, index } = &target.kind {
                    self.compile_index_store(object, index, val)?;
                } else if let ExprKind::Unary {
                    op: UnaryOp::Deref,
                    operand,
                } = &target.kind
                {
                    // `*r = val` — store through the mut-ref pointer.
                    // get_data_ptr loads the raw pointer from the alloca (one
                    // load, not two), giving us the address to store into.
                    if let ExprKind::Identifier(name) = &operand.kind {
                        if let Some(ptr) = self.get_data_ptr(name) {
                            self.builder.build_store(ptr, val).unwrap();
                        }
                    }
                }
                Ok(())
            }
            StmtKind::CompoundAssign { target, op, value } => {
                if let ExprKind::Identifier(name) = &target.kind {
                    let current = self.load_variable(name)?;
                    let rhs = self.compile_expr(value)?;
                    let binop = match op {
                        CompoundOp::Add => BinOp::Add,
                        CompoundOp::Sub => BinOp::Sub,
                        CompoundOp::Mul => BinOp::Mul,
                        CompoundOp::Div => BinOp::Div,
                        CompoundOp::Mod => BinOp::Mod,
                        CompoundOp::BitAnd => BinOp::BitAnd,
                        CompoundOp::BitOr => BinOp::BitOr,
                        CompoundOp::BitXor => BinOp::BitXor,
                        CompoundOp::Shl => BinOp::Shl,
                        CompoundOp::Shr => BinOp::Shr,
                    };
                    let result = self.compile_binop(&binop, current, rhs)?;
                    if let Some(slot) = self.variables.get(name).copied() {
                        self.builder.build_store(slot.ptr, result).unwrap();
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(super) fn bind_pattern(
        &mut self,
        pattern: &Pattern,
        val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                let fn_val = self.current_fn.unwrap();
                let alloca = self.create_entry_alloca(fn_val, name, val.get_type());
                self.builder.build_store(alloca, val).unwrap();
                self.variables.insert(
                    name.clone(),
                    VarSlot {
                        ptr: alloca,
                        ty: val.get_type(),
                    },
                );
                Ok(())
            }
            PatternKind::Wildcard => Ok(()),
            // Struct destructuring: let Foo { x, y } = val
            PatternKind::Struct { path: _, fields } => {
                if let BasicValueEnum::StructValue(sv) = val {
                    for (idx, field_pat) in fields.iter().enumerate() {
                        let field_val = self
                            .builder
                            .build_extract_value(sv, idx as u32, "field")
                            .unwrap();
                        if let Some(pat) = &field_pat.pattern {
                            self.bind_pattern(pat, field_val)?;
                        } else {
                            // Shorthand `Foo { x }` — bind field name as variable
                            let fn_val = self.current_fn.unwrap();
                            let alloca = self.create_entry_alloca(
                                fn_val,
                                &field_pat.name,
                                field_val.get_type(),
                            );
                            self.builder.build_store(alloca, field_val).unwrap();
                            self.variables.insert(
                                field_pat.name.clone(),
                                VarSlot {
                                    ptr: alloca,
                                    ty: field_val.get_type(),
                                },
                            );
                        }
                    }
                }
                Ok(())
            }
            // Tuple destructuring: let (a, b) = val
            PatternKind::Tuple(pats) => {
                if let BasicValueEnum::StructValue(sv) = val {
                    for (idx, pat) in pats.iter().enumerate() {
                        let elem = self
                            .builder
                            .build_extract_value(sv, idx as u32, "elem")
                            .unwrap();
                        self.bind_pattern(pat, elem)?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl<'ctx> super::Codegen<'ctx> {
    /// Detect statements that discard the result of a direct method call:
    /// `let _ = obj.method(...)` (Wildcard let-binding) or a bare
    /// `obj.method(...);` expression statement. Returns the receiver
    /// identifier name and the method name when matched, so the caller
    /// can gate further work on the specific receiver type. Returns
    /// `None` for any other shape — nested calls, non-Identifier
    /// receivers, non-Wildcard let patterns, etc. Used by the
    /// `Map.insert` shared-value overwrite-leak fix; safe to extend
    /// to other discard-leak shapes by checking the returned method
    /// name at the call site.
    pub(super) fn stmt_discards_method_call(stmt: &Stmt) -> Option<(&str, &str)> {
        let expr: &Expr = match &stmt.kind {
            StmtKind::Let { pattern, value, .. }
                if matches!(&pattern.kind, PatternKind::Wildcard) =>
            {
                value
            }
            StmtKind::Expr(e) => e,
            _ => return None,
        };
        if let ExprKind::MethodCall { object, method, .. } = &expr.kind {
            if let ExprKind::Identifier(name) = &object.kind {
                return Some((name.as_str(), method.as_str()));
            }
        }
        None
    }
}

/// Recurse into branching tail expressions to decide whether the RHS of a
/// shared-type `let` / `Assign` already delivers a freshly-owned ref
/// (callee move-out for `Call` / `MethodCall`, or `emit_rc_alloc` for
/// `StructLiteral`). The receive-site `rc_inc` must be skipped exactly
/// when this returns `true`; otherwise the refcount lands at 2 for a
/// genuinely fresh value and leaks one ref per crossing (same shape as
/// bug #8 receive-side, but for `If` / `Match` / `IfLet` / `Block` /
/// `LabeledBlock` / `Unsafe` tails that nest the fresh-ref source one
/// level deeper than the outer `ExprKind` reveals).
///
/// Conservative on mixed-shape branches: returns `false` when ANY branch
/// tail aliases an existing ref (`Identifier` / `FieldAccess` / `Index`
/// / etc.), so the receive site still incs. The fresh-tail branches in
/// that mix will double-inc (leaking +1 on those paths) — same behavior
/// as before this helper — but the aliasing branch is preserved
/// correctly. Per-branch inc emission would require lowering the
/// receive-inc into each tail block; deferred to a future slice.
pub(super) fn rhs_yields_fresh_ref(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::StructLiteral { .. } | ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
            true
        }
        ExprKind::Block(block)
        | ExprKind::Unsafe(block)
        | ExprKind::LabeledBlock { body: block, .. } => block
            .final_expr
            .as_deref()
            .is_some_and(rhs_yields_fresh_ref),
        ExprKind::If {
            then_block,
            else_branch,
            ..
        }
        | ExprKind::IfLet {
            then_block,
            else_branch,
            ..
        } => {
            then_block
                .final_expr
                .as_deref()
                .is_some_and(rhs_yields_fresh_ref)
                && else_branch.as_deref().is_some_and(rhs_yields_fresh_ref)
        }
        ExprKind::Match { arms, .. } => {
            !arms.is_empty() && arms.iter().all(|arm| rhs_yields_fresh_ref(&arm.body))
        }
        _ => false,
    }
}
