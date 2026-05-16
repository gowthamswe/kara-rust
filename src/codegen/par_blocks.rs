//! `par {}` block lowering — branch-fn synthesis + runtime spawn.
//!
//! Houses `compile_par_block` (the entry point), `emit_par_run`
//! (which builds the `KaracBranch[]` array and emits the
//! `karac_par_run` call), `emit_par_branch_fn` (the per-branch
//! synthesized fn body), and `emit_branch_cancel_check` (the
//! cooperative-cancel atomic-load emitted before each call site
//! inside a par-branch body).

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::token::Span;

use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, StructType};
use inkwell::values::{BasicValueEnum, PointerValue};
use inkwell::AddressSpace;

use super::state::{ReturnSlot, VarSlot};

impl<'ctx> super::Codegen<'ctx> {
    /// Compile a `par {}` block by spawning each stmt as a per-branch
    /// fn, building a `KaracBranch[]` array, and handing it to
    /// `karac_par_run`. Each branch fn is given a fresh stack ctx that
    /// captures any outer bindings it reads (and writes them back through
    /// caller-allocated return slots when applicable). The par block
    /// itself evaluates to `unit` (i64 0); return-value propagation is
    /// the slot mechanism.
    #[allow(clippy::result_large_err)]
    pub(super) fn compile_par_block(
        &mut self,
        block: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Slice A: explicit `par {}` blocks pass an empty slot list — the
        // par-block-as-expression doesn't have outer let-bindings to feed,
        // so the slot mechanism is dormant on this path. The auto-par
        // dispatch site in `compile_function_body` is the only call site
        // that supplies a non-empty slot list today. Lifting this for
        // `let x = par { ... }` is a v1.x extension noted in the slice-A
        // out-of-scope list.
        self.emit_par_run(&block.stmts, &block.span, &[])?;
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    /// Lower a list of statements to a `karac_par_run` runtime dispatch.
    ///
    /// Shared between the explicit-`par`-block lowering (`compile_par_block`)
    /// and slice 2's auto-par lowering on inferred parallel groups
    /// (`compile_function_body`). Both call sites pass a slice of stmts that
    /// should run concurrently and a span used for capture-set scoping —
    /// for the explicit path the span is the par-block's own span; for the
    /// inferred path it is best-effort the function-body span (per-stmt
    /// span resolution is slice 3's concern). Trivial fan-outs (zero or
    /// one statement) compile sequentially without invoking the runtime.
    ///
    /// **Slice A (Phase-7 — Par codegen: return values, 2026-05-09):**
    /// `return_slots` carries the per-group set of let-bindings whose
    /// values must flow out of the parallel group to subsequent stmts in
    /// the surrounding function body. For each non-empty slot list, this
    /// function: (1) synthesizes a parent-allocated return struct
    /// `__karac_ParGroup_<spawn_site_id>_Returns` with one field per
    /// slot in slot-order; (2) passes its pointer through the env-struct
    /// as a trailing field so each branch can write to it; (3) the
    /// branch fn writes its produced value(s) into the assigned
    /// field(s) right after the let-binding's local alloca is filled,
    /// before the branch returns; (4) after `karac_par_run` joins, the
    /// parent loads each slot back into a `HashMap<String,
    /// BasicValueEnum>` keyed by binding-name. The caller (the auto-par
    /// dispatch site in `compile_function_body`) consumes the map to
    /// bind each loaded value as a fresh local in the function-body
    /// scope. Empty `return_slots` reduces to slice 2's behavior:
    /// no return-struct alloca, no slot field on the env-struct, no
    /// loads after the runtime call.
    #[allow(clippy::result_large_err)]
    pub(super) fn emit_par_run(
        &mut self,
        stmts: &[Stmt],
        span: &Span,
        return_slots: &[ReturnSlot<'ctx>],
    ) -> Result<HashMap<String, BasicValueEnum<'ctx>>, String> {
        // Zero statements: nothing to do. Single statement: no parallelism
        // needed — compile in place to avoid the runtime call overhead.
        // The slot map is populated by reading each slot's binding from
        // `self.variables` after `compile_stmt` runs, so the caller's
        // outside-of-group reads still resolve.
        if stmts.is_empty() {
            return Ok(HashMap::new());
        }
        if stmts.len() == 1 {
            self.compile_stmt(&stmts[0])?;
            let mut map: HashMap<String, BasicValueEnum<'ctx>> = HashMap::new();
            for slot in return_slots {
                if let Some(local) = self.variables.get(&slot.binding_name).copied() {
                    let v = self
                        .builder
                        .build_load(local.ty, local.ptr, &slot.binding_name)
                        .unwrap();
                    map.insert(slot.binding_name.clone(), v);
                }
            }
            return Ok(map);
        }

        // 1. Collect the union of captured variables across all branch statements.
        //    Intersection with self.variables filters out non-locals (top-level
        //    functions, struct names, etc.) that refs_in_block doesn't distinguish.
        let mut refs: HashSet<String> = HashSet::new();
        let mut inner_defs: HashSet<String> = HashSet::new();
        for stmt in stmts {
            let mini = Block {
                stmts: vec![stmt.clone()],
                final_expr: None,
                span: span.clone(),
            };
            self.refs_in_block(&mini, &mut refs, &mut inner_defs);
        }
        let mut captures: Vec<String> = refs
            .into_iter()
            .filter(|n| !inner_defs.contains(n))
            .filter(|n| self.variables.contains_key(n.as_str()))
            .collect();
        captures.sort(); // deterministic order

        // 2. Build the shared env struct. Captured user locals fill the
        //    leading slots; the next slot (added in slice 4) is the
        //    `*const ProviderFrame` snapshot of the calling thread's
        //    stack head (Theme 6 sub-step 5 — provider inheritance).
        //    The final slot (added in slice A) is a `*mut
        //    ParGroupReturns` pointing at the parent-allocated return
        //    struct — branches dereference and write through it. The
        //    env-struct grows by one pointer field whether the slot
        //    list is empty or not (ABI uniformity — keeps the env-
        //    struct shape predictable per spawn-site for downstream
        //    debugger introspection).
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let mut env_field_types: Vec<BasicTypeEnum<'ctx>> =
            captures.iter().map(|n| self.variables[n].ty).collect();
        let provider_head_idx = env_field_types.len();
        env_field_types.push(ptr_ty.into());
        let par_returns_idx = env_field_types.len();
        env_field_types.push(ptr_ty.into());
        let env_struct_ty = self.context.struct_type(&env_field_types, false);

        // 3. Allocate and populate the env struct in the outer function.
        //    Captures are copied by value (sufficient for ints, floats,
        //    pointers — the types the rest of codegen already supports).
        //    The provider-head field is filled by calling
        //    `karac_provider_get_stack_head()`; that read is cheap (one
        //    TLS get) and runs once per par-block, not per branch.
        let outer_fn = self.current_fn.unwrap();
        let env_alloca = self.create_entry_alloca(outer_fn, "__par_env", env_struct_ty.into());
        let mut env_agg = env_struct_ty.get_undef();
        for (i, name) in captures.iter().enumerate() {
            let slot = self.variables[name];
            let val = self.builder.build_load(slot.ty, slot.ptr, name).unwrap();
            env_agg = self
                .builder
                .build_insert_value(env_agg, val, i as u32, "__par_env_field")
                .unwrap()
                .into_struct_value();
        }
        let head_val = self
            .builder
            .build_call(
                self.karac_provider_get_stack_head_fn,
                &[],
                "__par_env_head_snap",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic();
        env_agg = self
            .builder
            .build_insert_value(
                env_agg,
                head_val,
                provider_head_idx as u32,
                "__par_env_head",
            )
            .unwrap()
            .into_struct_value();

        // Slice A: mint the per-group return-struct type and alloca it
        // in the parent frame. We use the spawn-site ID (recorded just
        // below by `record_spawn_site`) as the type-name disambiguator.
        // To know the ID before recording, we mint it here and pass it
        // through. The struct lives module-scope as a named LLVM struct
        // so re-emission collisions are caught by inkwell. Empty slot
        // list → no struct, no alloca, the env-struct's
        // `__par_returns` field is a null `ptr` (never dereferenced
        // because the branch's slot-write path is dead code without
        // slots).
        let par_id = self.record_spawn_site(span, Some(stmts.len() as u32));
        let return_struct_ty: Option<StructType<'ctx>> = if return_slots.is_empty() {
            None
        } else {
            let name = format!("__karac_ParGroup_{par_id}_Returns");
            let st = self.context.opaque_struct_type(&name);
            let field_tys: Vec<BasicTypeEnum<'ctx>> =
                return_slots.iter().map(|s| s.llvm_ty).collect();
            st.set_body(&field_tys, false);
            Some(st)
        };
        let return_struct_alloca: PointerValue<'ctx> = if let Some(st) = return_struct_ty {
            self.create_entry_alloca(outer_fn, "__par_returns", st.into())
        } else {
            ptr_ty.const_null()
        };
        env_agg = self
            .builder
            .build_insert_value(
                env_agg,
                return_struct_alloca,
                par_returns_idx as u32,
                "__par_env_returns",
            )
            .unwrap()
            .into_struct_value();
        self.builder.build_store(env_alloca, env_agg).unwrap();

        // 4. Generate one branch function per statement.
        //    The SpawnSiteId minted above is reused as the branch fn
        //    name disambiguator and as the `karac_par_run` argument
        //    (Debugger Contract slice 4: the runtime uses it to
        //    populate `KaracFrame::spawn_site_id` for slice 5's
        //    enumeration surface).
        let mut branch_fn_ptrs: Vec<PointerValue<'ctx>> = Vec::with_capacity(stmts.len());
        for (i, stmt) in stmts.iter().enumerate() {
            // Per-branch slot list: only the slots whose `branch_index`
            // matches this branch flow into `emit_par_branch_fn` for
            // slot-write emission. Branches with no slots emit unchanged.
            let branch_slots: Vec<ReturnSlot<'ctx>> = return_slots
                .iter()
                .filter(|s| s.branch_index == i)
                .cloned()
                .collect();
            let fn_ptr = self.emit_par_branch_fn(
                par_id,
                i,
                stmt,
                &captures,
                &env_field_types,
                env_struct_ty,
                par_returns_idx,
                return_struct_ty,
                &branch_slots,
                return_slots,
            )?;
            branch_fn_ptrs.push(fn_ptr);
        }

        // 5. Build the KaracBranch array on the stack, one entry per branch.
        let i64_type = self.context.i64_type();
        let branches_ty = self.karac_branch_ty.array_type(stmts.len() as u32);
        let branches_alloca =
            self.create_entry_alloca(outer_fn, "__par_branches", branches_ty.into());
        for (i, fn_ptr) in branch_fn_ptrs.iter().enumerate() {
            let mut entry = self.karac_branch_ty.get_undef();
            entry = self
                .builder
                .build_insert_value(entry, *fn_ptr, 0, "__par_branch_fn")
                .unwrap()
                .into_struct_value();
            entry = self
                .builder
                .build_insert_value(entry, env_alloca, 1, "__par_branch_ctx")
                .unwrap()
                .into_struct_value();
            let idx = [
                i64_type.const_int(0, false),
                i64_type.const_int(i as u64, false),
            ];
            let elem_ptr = unsafe {
                self.builder
                    .build_in_bounds_gep(branches_ty, branches_alloca, &idx, "__par_branch_slot")
                    .unwrap()
            };
            self.builder.build_store(elem_ptr, entry).unwrap();
        }

        // 6. Call karac_par_run(branches, count, par_id).
        //    `par_id` (Debugger Contract slice 4) was minted via
        //    `record_spawn_site` above; the runtime uses it to populate
        //    `KaracFrame::spawn_site_id` for slice 5's enumeration surface.
        let count = i64_type.const_int(stmts.len() as u64, false);
        let par_id_val = self.context.i32_type().const_int(par_id as u64, false);
        self.builder
            .build_call(
                self.karac_par_run_fn,
                &[branches_alloca.into(), count.into(), par_id_val.into()],
                "__par_run",
            )
            .unwrap();

        // 7. Slice A: load each return slot back from the parent-allocated
        //    return struct. The runtime barrier inside `karac_par_run`
        //    guarantees all branch fns completed before this point, so
        //    every slot the analyzer assigned is initialized (decision
        //    iii — move-only slot semantics with no destructor; the
        //    barrier replaces the destructor that would otherwise
        //    enforce ordering).
        let mut slot_values: HashMap<String, BasicValueEnum<'ctx>> = HashMap::new();
        if let Some(st) = return_struct_ty {
            for (field_idx, slot) in return_slots.iter().enumerate() {
                let field_ptr = self
                    .builder
                    .build_struct_gep(
                        st,
                        return_struct_alloca,
                        field_idx as u32,
                        &format!("__par_slot_{}_ptr", slot.binding_name),
                    )
                    .unwrap();
                let val = self
                    .builder
                    .build_load(slot.llvm_ty, field_ptr, &slot.binding_name)
                    .unwrap();
                slot_values.insert(slot.binding_name.clone(), val);
            }
        }
        Ok(slot_values)
    }

    /// Generate the branch function for a single par-block statement.
    /// Signature: `void __par_branch_<par_id>_<i>(ptr ctx, ptr cancel_flag)`.
    ///
    /// The function unpacks captured locals from the shared env struct,
    /// compiles the statement, and returns. Captures are loaded as fresh
    /// allocas so the statement body sees them as ordinary locals.
    ///
    /// **Slice A (Phase-7 — Par codegen: return values):** when
    /// `branch_slots` is non-empty, after the statement body's
    /// `compile_stmt` succeeds, this function emits a load+store
    /// sequence for each assigned slot — loading the just-bound
    /// variable's value out of its branch-local alloca and storing it
    /// into the matching field of the parent-allocated return struct
    /// (reached via the `__par_returns` field of the env struct). The
    /// store happens *before* the branch fn's `ret void`, so by the
    /// time `karac_par_run`'s join barrier returns to the parent every
    /// slot the analyzer assigned is initialized. Move-only semantics
    /// (decision iii): the branch's `scope_cleanup_actions` are
    /// discarded on `emit_par_branch_fn` exit (the existing
    /// `mem::take`/restore dance), so destructor-bearing slot values
    /// move into the slot rather than being dropped at branch end —
    /// the parent's load + subsequent `track_*` is the unique cleanup
    /// owner.
    #[allow(clippy::result_large_err)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_par_branch_fn(
        &mut self,
        par_id: u32,
        index: usize,
        stmt: &Stmt,
        captures: &[String],
        env_field_types: &[BasicTypeEnum<'ctx>],
        env_struct_ty: StructType<'ctx>,
        par_returns_idx: usize,
        return_struct_ty: Option<StructType<'ctx>>,
        branch_slots: &[ReturnSlot<'ctx>],
        all_slots: &[ReturnSlot<'ctx>],
    ) -> Result<PointerValue<'ctx>, String> {
        let fn_name = format!("__par_branch_{}_{}", par_id, index);
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        // Branch function signature: void fn(ptr ctx, ptr cancel_flag)
        let fn_type = self.context.void_type().fn_type(
            &[
                BasicMetadataTypeEnum::from(ptr_ty),
                BasicMetadataTypeEnum::from(ptr_ty),
            ],
            false,
        );
        let branch_fn = self.module.add_function(&fn_name, fn_type, None);

        // Save outer codegen state — we're about to compile a fresh function.
        let saved_bb = self.builder.get_insert_block();
        let saved_fn = self.current_fn;
        let saved_vars = std::mem::take(&mut self.variables);
        let saved_var_types = std::mem::take(&mut self.var_type_names);
        let saved_loop_stack = std::mem::take(&mut self.loop_stack);
        let saved_cleanup = std::mem::take(&mut self.scope_cleanup_actions);
        let saved_cancel_ptr = self.branch_cancel_ptr.take();

        self.current_fn = Some(branch_fn);
        let entry = self.context.append_basic_block(branch_fn, "entry");
        self.builder.position_at_end(entry);

        // Cancel check at branch start: if *cancel_flag != 0, return immediately.
        let cancel_ptr = branch_fn.get_nth_param(1).unwrap().into_pointer_value();
        // Stash the cancel pointer so subsequent `compile_call` invocations
        // can emit mid-branch cooperative cancel checks before each callee.
        self.branch_cancel_ptr = Some(cancel_ptr);
        let i8_ty = self.context.i8_type();
        let cancel_val = self
            .builder
            .build_load(i8_ty, cancel_ptr, "cancel")
            .unwrap()
            .into_int_value();
        let is_cancelled = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                cancel_val,
                i8_ty.const_int(0, false),
                "is_cancelled",
            )
            .unwrap();
        let body_bb = self.context.append_basic_block(branch_fn, "body");
        let cancel_bb = self.context.append_basic_block(branch_fn, "cancelled");
        self.builder
            .build_conditional_branch(is_cancelled, cancel_bb, body_bb)
            .unwrap();
        self.builder.position_at_end(cancel_bb);
        self.builder.build_return(None).unwrap();
        self.builder.position_at_end(body_bb);

        // Theme 6 sub-step 5: seed this worker thread's provider stack
        // from the env-struct snapshot taken at par-block entry. Always
        // emitted because every par-block env-struct now carries the
        // head-pointer slot in its trailing field (the captures vec may
        // be empty but the env still has at least the one ptr field).
        // Run before unpacking captures so any with_provider bindings
        // are visible inside their initialization (defensive — none of
        // the existing capture-init paths invoke R.method, but this
        // ordering is the cheap, future-proof choice).
        let env_ptr = branch_fn.get_nth_param(0).unwrap().into_pointer_value();
        let env_val_for_head = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(env_struct_ty.into(), env_ptr, "__env_head_load")
            .unwrap();
        let head_val = self
            .builder
            .build_extract_value(
                env_val_for_head.into_struct_value(),
                captures.len() as u32,
                "__par_branch_head",
            )
            .unwrap();
        self.builder
            .build_call(
                self.karac_provider_set_stack_head_fn,
                &[head_val.into()],
                "",
            )
            .unwrap();

        // Unpack captures from the env struct into fresh allocas.
        if !captures.is_empty() {
            let env_val = self
                .builder
                .build_load::<BasicTypeEnum<'ctx>>(env_struct_ty.into(), env_ptr, "__env")
                .unwrap();
            for (i, var_name) in captures.iter().enumerate() {
                let cap_ty = env_field_types[i];
                let field_val = self
                    .builder
                    .build_extract_value(env_val.into_struct_value(), i as u32, var_name)
                    .unwrap();
                let alloca = self.create_entry_alloca(branch_fn, var_name, cap_ty);
                self.builder.build_store(alloca, field_val).unwrap();
                self.variables.insert(
                    var_name.clone(),
                    VarSlot {
                        ptr: alloca,
                        ty: cap_ty,
                    },
                );
                // Propagate the outer scope's struct/enum type binding so
                // method dispatch can route `var.method()` through the
                // user impl-block path inside the par branch.
                if let Some(type_name) = saved_var_types.get(var_name) {
                    self.var_type_names
                        .insert(var_name.clone(), type_name.clone());
                }
            }
        }

        // Compile the statement body. Any errors surface to the outer context.
        let stmt_result = self.compile_stmt(stmt);

        // Slice A: emit slot writes for class-(ii) bindings produced by
        // this branch. Walk `branch_slots` (the slots whose
        // `branch_index == index`), find the matching variable in
        // `self.variables` (just bound by the let inside `compile_stmt`
        // above), load it, then store into the parent-allocated return
        // struct's field at the slot's position in `all_slots`. Done
        // before the branch fn's `ret` so the runtime barrier inside
        // `karac_par_run` correctly orders the writes against the
        // parent's subsequent load.
        let stmt_ok = stmt_result.is_ok()
            && self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_none();
        if stmt_ok && !branch_slots.is_empty() {
            if let Some(rt_struct) = return_struct_ty {
                // Reload the env-struct here to extract the
                // `__par_returns` pointer. We can't keep a stale value
                // from prologue because `compile_stmt` may have emitted
                // arbitrary basic blocks between then and now; safer to
                // re-load.
                let env_val = self
                    .builder
                    .build_load::<BasicTypeEnum<'ctx>>(
                        env_struct_ty.into(),
                        env_ptr,
                        "__env_for_returns",
                    )
                    .unwrap();
                let returns_ptr_v = self
                    .builder
                    .build_extract_value(
                        env_val.into_struct_value(),
                        par_returns_idx as u32,
                        "__par_returns_ptr",
                    )
                    .unwrap();
                let returns_ptr = returns_ptr_v.into_pointer_value();
                for slot in branch_slots {
                    // Find this slot's index in the all-slots list (i.e.
                    // its field position in the return struct). Linear
                    // search — slot lists are tiny (≤ branch count).
                    let Some(field_idx) = all_slots
                        .iter()
                        .position(|s| s.binding_name == slot.binding_name)
                    else {
                        continue;
                    };
                    let Some(local) = self.variables.get(&slot.binding_name).copied() else {
                        // Variable wasn't bound (compile_stmt error path,
                        // class-(ii) binding shape mismatch, etc.) — skip
                        // the slot write defensively.
                        continue;
                    };
                    let val = self
                        .builder
                        .build_load(
                            local.ty,
                            local.ptr,
                            &format!("__par_slot_{}_load", slot.binding_name),
                        )
                        .unwrap();
                    let field_ptr = self
                        .builder
                        .build_struct_gep(
                            rt_struct,
                            returns_ptr,
                            field_idx as u32,
                            &format!("__par_slot_{}_dst", slot.binding_name),
                        )
                        .unwrap();
                    self.builder.build_store(field_ptr, val).unwrap();
                }
            }
        }

        // Terminate the branch function. The par-block API discards branch
        // return values in this first cut.
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_return(None).unwrap();
        }

        // Restore outer state.
        self.branch_cancel_ptr = saved_cancel_ptr;
        self.scope_cleanup_actions = saved_cleanup;
        self.loop_stack = saved_loop_stack;
        self.var_type_names = saved_var_types;
        self.variables = saved_vars;
        self.current_fn = saved_fn;
        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        stmt_result?;
        Ok(branch_fn.as_global_value().as_pointer_value())
    }

    /// If we are currently compiling a par-branch function body, emit a
    /// cooperative cancel check at the current insertion point: load the
    /// runtime's `AtomicBool` cancel flag, branch to a fresh "cancelled"
    /// block when set, otherwise fall through to a "continue" block. The
    /// cancelled block drains scope cleanup actions and `return`s void
    /// from the branch function, mirroring the entry-time check shape.
    /// No-op outside par branches.
    ///
    /// `callee` is the canonical name of the call about to be emitted (free
    /// fn `name` or `Type.method`). When `Some(name)` and
    /// `callee_effectful[name] == false`, the check is skipped — the
    /// callee carries no `reads`/`writes`/`sends`/`receives`, so a mid-branch
    /// cancellation cannot observe a partial side effect via this call.
    /// `None` (or an unknown name) preserves the conservative MVP behavior.
    pub(super) fn emit_branch_cancel_check(&mut self, label: &str, callee: Option<&str>) {
        let Some(cancel_ptr) = self.branch_cancel_ptr else {
            return;
        };
        if let Some(name) = callee {
            if let Some(false) = self.callee_effectful.get(name) {
                return;
            }
        }
        let i8_ty = self.context.i8_type();
        let cancel_val = self
            .builder
            .build_load(i8_ty, cancel_ptr, &format!("{label}.cancel.flag"))
            .unwrap()
            .into_int_value();
        let is_cancelled = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                cancel_val,
                i8_ty.const_int(0, false),
                &format!("{label}.cancelled"),
            )
            .unwrap();
        let fn_val = self.current_fn.unwrap();
        let cancel_bb = self
            .context
            .append_basic_block(fn_val, &format!("{label}.cancel.bb"));
        let cont_bb = self
            .context
            .append_basic_block(fn_val, &format!("{label}.cont.bb"));
        self.builder
            .build_conditional_branch(is_cancelled, cancel_bb, cont_bb)
            .unwrap();
        self.builder.position_at_end(cancel_bb);
        self.emit_scope_cleanup();
        self.builder.build_return(None).unwrap();
        self.builder.position_at_end(cont_bb);
    }
}
