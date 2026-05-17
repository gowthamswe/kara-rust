//! For-loop codegen: every `for pattern in <iterable> { body }` shape
//! the compiler knows how to lower today.
//!
//! Houses `compile_for` (the entry dispatch) and the per-iterable-shape
//! specialisations: `compile_for_range`, `compile_for_range_with_step`,
//! `compile_for_slice_var`, `compile_for_vec_var`,
//! `compile_for_string_chars` / `compile_for_string_chars_inner`,
//! `compile_for_map_var`, `compile_for_set_var`, `compile_for_array_var`,
//! `compile_for_array_values`.
//!
//! Lives in a sibling `impl<'ctx> super::Codegen<'ctx>` block.

use crate::ast::*;

use inkwell::types::BasicTypeEnum;
use inkwell::values::{BasicValueEnum, IntValue, PointerValue};
use inkwell::AddressSpace;
use inkwell::IntPredicate;

use super::state::LoopFrame;

impl<'ctx> super::Codegen<'ctx> {
    // ── For loop ─────────────────────────────────────────────────

    /// Compile `for pattern in iterable { body }`.
    /// Currently supports ranges (`start..end`, `start..=end`) and array literals.
    pub(super) fn compile_for(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        iterable: &Expr,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // `for x in coll.iter()` / `for x in coll.into_iter()` —
        // codegen iterates the underlying storage directly via the
        // existing `compile_for_*_var` paths (no `Value::Iterator`
        // wrapper at this layer), so peel off a transparent `.iter()`
        // / `.into_iter()` and recurse on the inner receiver. Without
        // this, the method-call iterable falls through to the silent
        // `_ =>` arm below — the body never executes and outer-scope
        // mutables look unchanged.
        if let ExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &iterable.kind
        {
            if args.is_empty() && (method == "iter" || method == "into_iter") {
                // Indexed receiver (`coll[i].iter()`): synthesize a
                // temp identifier pointing into `coll`'s storage and
                // recurse, mirroring `compile_nested_index_read`.
                // Without this, the recursed `compile_for` sees an
                // Index expression and falls through the dispatch
                // match's `_ =>` arm — the body never executes.
                if let ExprKind::Index {
                    object: outer,
                    index: idx,
                } = &object.kind
                {
                    return self.compile_for_indexed_iter(label, pattern, outer, idx, body);
                }
                // Field receiver (`obj.field.iter()`) where `obj` is a
                // known struct (shared or plain) and `field` is a
                // `Vec[T]` / `Slice[T]`: synthesize a temp identifier
                // pointing at the field's embedded `{ptr,len,cap}`
                // struct and recurse. Without this, the recursed
                // `compile_for` sees a FieldAccess expression and falls
                // through to the `_ =>` arm — the body never executes
                // and outer-scope mutables look unchanged (the
                // clone-graph kata's `for nb in curr.neighbors.iter()`
                // surface, 2026-05-16).
                if let ExprKind::FieldAccess {
                    object: outer,
                    field,
                } = &object.kind
                {
                    if let Some(result) =
                        self.try_compile_for_field_iter(label, pattern, outer, field, body)?
                    {
                        return Ok(result);
                    }
                }
                return self.compile_for(label, pattern, object, body);
            }
            // `for c in s.chars()` — peel `.chars()` off and recurse on
            // the bare String receiver. The Identifier arm below dispatches
            // through `string_vars` to `compile_for_string_chars`. Same
            // shape as the `.iter()` / `.into_iter()` peel-off above —
            // codegen iterators are dispatch points, not runtime values
            // (design.md § Iterator Adaptors v1 surface).
            if args.is_empty() && method == "chars" {
                return self.compile_for(label, pattern, object, body);
            }
            // `for j in (start..end).step_by(n)` — the only chained
            // iterator-adaptor codegen surface supported in v1.
            // Lowers to a Range loop with a custom step (default 1).
            // The step expression `n` is evaluated once at loop entry
            // and captured for the increment block. Chained beyond
            // step_by (e.g. `.step_by(n).map(f)`) falls through to
            // the silent `_ =>` arm — the broader iterator-adaptor
            // codegen surface is a separate slice.
            if args.len() == 1 && method == "step_by" {
                if let ExprKind::Range {
                    start,
                    end,
                    inclusive,
                } = &object.kind
                {
                    let step_expr = &args[0].value;
                    return self.compile_for_range_with_step(
                        label,
                        pattern,
                        start,
                        end,
                        *inclusive,
                        Some(step_expr),
                        body,
                    );
                }
            }
        }
        match &iterable.kind {
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => self.compile_for_range(label, pattern, start, end, *inclusive, body),
            ExprKind::ArrayLiteral(elems) => {
                // Compile each element eagerly and iterate by index
                let elems: Vec<BasicValueEnum<'ctx>> = elems
                    .iter()
                    .map(|e| self.compile_expr(e))
                    .collect::<Result<_, _>>()?;
                self.compile_for_array_values(pattern, &elems, body)
            }
            ExprKind::StringLit(_) | ExprKind::InterpolatedStringLit(_) => {
                // Bare string literal or f-string as the iterable —
                // `for c in "abc"` / `for c in "abc".chars()` (after the
                // peel-off above). Compile the literal to a {ptr, len, cap}
                // String struct, extract data + len, drive the per-char
                // loop. No alloca needed: the struct is value-form and the
                // backing buffer is the program's read-only string pool
                // (cap=0 indicates static, no scope-exit free).
                let val = self.compile_expr(iterable)?;
                let sv = val.into_struct_value();
                let data = self
                    .builder
                    .build_extract_value(sv, 0, "for.s.lit.data")
                    .unwrap()
                    .into_pointer_value();
                let len = self
                    .builder
                    .build_extract_value(sv, 1, "for.s.lit.len")
                    .unwrap()
                    .into_int_value();
                self.compile_for_string_chars_inner(label, pattern, data, len, body)
            }
            ExprKind::Identifier(name) => {
                if let Some(slot) = self.variables.get(name.as_str()).copied() {
                    // Owned array
                    if let BasicTypeEnum::ArrayType(at) = slot.ty {
                        return self.compile_for_array_var(label, pattern, slot.ptr, at, body);
                    }
                    // Ref array
                    if let Some(&BasicTypeEnum::ArrayType(at)) = self.ref_params.get(name.as_str())
                    {
                        let arr_ptr = self.get_data_ptr(name).unwrap();
                        return self.compile_for_array_var(label, pattern, arr_ptr, at, body);
                    }
                    // String iteration — per Unicode scalar value. Must
                    // come before the `vec_elem_types` arm: String vars
                    // are *also* registered in `vec_elem_types` (with i8
                    // element type, matching the `{ptr, i64, i64}` byte
                    // buffer), but `for c in s` iterates chars (i32), not
                    // bytes (i8). `string_vars` is the disambiguator.
                    // Design pin: design.md § Character type (line 2299).
                    if self.string_vars.contains(name.as_str()) {
                        return self.compile_for_string_chars(label, pattern, name, body);
                    }
                    // Vec iteration (owned or ref)
                    if self.vec_elem_types.contains_key(name.as_str()) {
                        return self.compile_for_vec_var(label, pattern, name, body);
                    }
                    // Slice iteration: `{ptr, len}` struct alloca.
                    if self.slice_elem_types.contains_key(name.as_str()) {
                        return self.compile_for_slice_var(label, pattern, name, body);
                    }
                    // Map iteration: for (k, v) in map { }
                    if self.map_key_types.contains_key(name.as_str()) {
                        return self.compile_for_map_var(label, pattern, name, body);
                    }
                    // Set iteration: for x in set { }
                    if self.set_elem_types.contains_key(name.as_str()) {
                        return self.compile_for_set_var(label, pattern, name, body);
                    }
                }
                Ok(self.context.i64_type().const_int(0, false).into())
            }
            // Bare field receiver: `for x in obj.field { }` (no
            // `.iter()` peel-off). Same synth-identifier pattern as the
            // `.iter()` arm above — recover the field pointer, mint a
            // tracked alias, and recurse with the alias as a regular
            // named-variable iterable.
            ExprKind::FieldAccess {
                object: outer,
                field,
            } => {
                if let Some(result) =
                    self.try_compile_for_field_iter(label, pattern, outer, field, body)?
                {
                    return Ok(result);
                }
                Ok(self.context.i64_type().const_int(0, false).into())
            }
            _ => {
                // Unknown iterable — skip body, return unit
                Ok(self.context.i64_type().const_int(0, false).into())
            }
        }
    }

    /// `for x in obj.field [.iter() / .into_iter()] { body }` driver.
    /// Recovers the field's pointer (heap-GEP for shared structs,
    /// slot-GEP for plain structs), mints a synth identifier with the
    /// field's TypeExpr-derived registries populated through
    /// `register_var_from_type_expr`, and recurses into `compile_for`
    /// with the synth as the iterable. Returns `Ok(None)` when the
    /// shape isn't a known struct-field receiver — caller falls
    /// through to its own diagnostic. Sibling to
    /// `compile_for_indexed_iter` (Index-receiver path) and
    /// `try_compile_field_receiver_method` (method-call FR path).
    /// Closes the `for nb in curr.neighbors.iter()` surface used by
    /// the clone-graph kata (kata-133), 2026-05-16.
    pub(super) fn try_compile_for_field_iter(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        outer: &Expr,
        field: &str,
        body: &Block,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        use super::state::VarSlot;
        // Chained field receivers (`a.b.c.iter()`) — defer to v1.x.
        // Mirrors `try_compile_field_receiver_method`'s FR4 guard.
        if matches!(outer.kind, ExprKind::FieldAccess { .. }) {
            return Err(
                "codegen: chained field receivers in `for x in a.b.c.iter()` \
                 are deferred to v1.x; bind the inner field to a temporary first"
                    .to_string(),
            );
        }
        // Recover the receiver-pointer the field GEP hangs off. Two
        // recognised inner shapes — Identifier (named variable) and
        // Index (`outer[i].field`) — same as the method-call FR path.
        let (type_name, receiver_ptr, is_shared_handle) = match &outer.kind {
            ExprKind::Identifier(outer_name) => {
                let type_name = match self.var_type_names.get(outer_name.as_str()).cloned() {
                    Some(t) => t,
                    None => return Ok(None),
                };
                let slot = match self.variables.get(outer_name.as_str()).copied() {
                    Some(s) => s,
                    None => return Ok(None),
                };
                let is_shared = self.shared_types.contains_key(&type_name);
                let recv_ptr = if is_shared {
                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                    self.builder
                        .build_load(ptr_ty, slot.ptr, "fr.for.shared.handle")
                        .unwrap()
                        .into_pointer_value()
                } else {
                    slot.ptr
                };
                (type_name, recv_ptr, is_shared)
            }
            ExprKind::Index {
                object: container,
                index,
            } => {
                if matches!(container.kind, ExprKind::Index { .. }) {
                    return Err("codegen: chained indexed field receivers \
                         (`a[i][j].field.iter()`) are deferred to v1.x; \
                         bind the intermediate element first"
                        .to_string());
                }
                let outer_name = match &container.kind {
                    ExprKind::Identifier(n) => n.clone(),
                    _ => return Ok(None),
                };
                let elem_te = match self.var_elem_type_exprs.get(outer_name.as_str()).cloned() {
                    Some(te) => te,
                    None => return Ok(None),
                };
                let elem_type_name = match &elem_te.kind {
                    TypeKind::Path(p) => match p.segments.first() {
                        Some(s) => s.clone(),
                        None => return Ok(None),
                    },
                    _ => return Ok(None),
                };
                let (elem_ptr, _elem_ll_ty) =
                    if self.vec_elem_types.contains_key(outer_name.as_str()) {
                        self.lower_indexed_elem_ptr_vec(&outer_name, index)?
                    } else if self.slice_elem_types.contains_key(outer_name.as_str()) {
                        self.lower_indexed_elem_ptr_slice(&outer_name, index)?
                    } else {
                        let slot = match self.variables.get(outer_name.as_str()).copied() {
                            Some(s) => s,
                            None => return Ok(None),
                        };
                        if let BasicTypeEnum::ArrayType(_) = slot.ty {
                            self.lower_indexed_elem_ptr_array(slot, index)?
                        } else {
                            return Ok(None);
                        }
                    };
                let is_shared = self.shared_types.contains_key(&elem_type_name);
                let recv_ptr = if is_shared {
                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                    self.builder
                        .build_load(ptr_ty, elem_ptr, "fr.for.idx.shared.handle")
                        .unwrap()
                        .into_pointer_value()
                } else {
                    elem_ptr
                };
                (elem_type_name, recv_ptr, is_shared)
            }
            _ => return Ok(None),
        };
        // Look up the field's index and TypeExpr.
        let field_idx = match self
            .struct_field_names
            .get(&type_name)
            .and_then(|names| names.iter().position(|n| n == field))
        {
            Some(i) => i,
            None => return Ok(None),
        };
        let field_te = match self
            .struct_field_type_exprs
            .get(&type_name)
            .and_then(|tes| tes.get(field_idx).cloned())
        {
            Some(te) => te,
            None => return Ok(None),
        };
        // GEP the field pointer. Shared: GEP at (idx + 1) past the
        // refcount slot using the heap_type. Plain: GEP directly into
        // the receiver-pointer at idx using the value struct_type.
        let (field_ptr, field_ll_ty) = if is_shared_handle {
            let info = match self.shared_types.get(&type_name).cloned() {
                Some(i) if !i.is_enum => i,
                _ => return Ok(None),
            };
            let fp = self
                .builder
                .build_struct_gep(
                    info.heap_type,
                    receiver_ptr,
                    (field_idx + 1) as u32,
                    &format!("for_sh_{}", field),
                )
                .unwrap();
            let fty = match info
                .heap_type
                .get_field_type_at_index((field_idx + 1) as u32)
            {
                Some(ty) => ty,
                None => return Ok(None),
            };
            (fp, fty)
        } else if let Some(st) = self.struct_types.get(&type_name).copied() {
            let fp = self
                .builder
                .build_struct_gep(
                    st,
                    receiver_ptr,
                    field_idx as u32,
                    &format!("for_pl_{}", field),
                )
                .unwrap();
            let fty = match st.get_field_type_at_index(field_idx as u32) {
                Some(ty) => ty,
                None => return Ok(None),
            };
            (fp, fty)
        } else {
            return Ok(None);
        };
        // Mint a synth identifier aliasing the field storage and
        // populate its registries. `register_var_from_type_expr`
        // covers Vec/Slice/String/Map/Set element-type tables and
        // also propagates `var_type_names` for bare user-struct
        // types (the regression-fix in this same commit).
        let synth = format!("__for_field_{}", self.indexed_elem_counter);
        self.indexed_elem_counter += 1;
        self.variables.insert(
            synth.clone(),
            VarSlot {
                ptr: field_ptr,
                ty: field_ll_ty,
            },
        );
        self.register_var_from_type_expr(&synth, &field_te);

        let synth_expr = Expr {
            kind: ExprKind::Identifier(synth.clone()),
            span: outer.span.clone(),
        };
        let result = self.compile_for(label, pattern, &synth_expr, body);

        // Clean up synth registrations so they don't leak across
        // sibling for-loops at the same nesting depth.
        self.variables.remove(&synth);
        self.vec_elem_types.remove(&synth);
        self.slice_elem_types.remove(&synth);
        self.var_elem_type_exprs.remove(&synth);
        self.var_type_names.remove(&synth);
        self.map_key_types.remove(&synth);
        self.map_val_types.remove(&synth);
        self.map_key_type_names.remove(&synth);
        self.map_key_type_exprs.remove(&synth);
        self.set_elem_types.remove(&synth);
        self.set_elem_type_names.remove(&synth);
        self.set_elem_type_exprs.remove(&synth);
        self.string_vars.remove(&synth);

        result.map(Some)
    }

    pub(super) fn compile_for_range(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        start: &Option<Box<Expr>>,
        end: &Option<Box<Expr>>,
        inclusive: bool,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        self.compile_for_range_with_step(label, pattern, start, end, inclusive, None, body)
    }

    /// Generic for-range codegen with an optional step expression.
    /// Step expr `Some(expr)` evaluates `expr` once before the loop
    /// and uses the result as the increment; `None` defaults to 1.
    /// Drives both the plain `for i in start..end` shape and the
    /// `for i in (start..end).step_by(n)` peel-off in `compile_for`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compile_for_range_with_step(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        start: &Option<Box<Expr>>,
        end: &Option<Box<Expr>>,
        inclusive: bool,
        step: Option<&Expr>,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();

        let start_val = if let Some(s) = start {
            self.compile_expr(s)?.into_int_value()
        } else {
            i64_t.const_int(0, false)
        };
        let end_val = if let Some(e) = end {
            self.compile_expr(e)?.into_int_value()
        } else {
            return Err("for-range loop requires an end bound".to_string());
        };
        // Evaluate the step expression once before the loop and stash
        // it. Default to 1 when absent.
        let step_val = if let Some(s) = step {
            self.compile_expr(s)?.into_int_value()
        } else {
            i64_t.const_int(1, false)
        };

        // Allocate loop counter
        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder.build_store(counter, start_val).unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // Condition: i < end (or i <= end for inclusive)
        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let pred = if inclusive {
            IntPredicate::SLE
        } else {
            IntPredicate::SLT
        };
        let cond = self
            .builder
            .build_int_compare(pred, cur, end_val, "for.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        // Body: bind pattern, compile block
        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap();
        self.bind_pattern(pattern, cur)?;
        // Bounds-check elision: a for-range loop establishes `start <= i < end`
        // (or `<= end` for inclusive). Push the facts compile_vec_index /
        // compile_slice_index need to elide the bounds check on `v[i]`
        // inside the body. The conservative rules match what we can prove
        // without arithmetic reasoning: start = 0 / non-negative literal
        // gives a lower bound; end resolving to a Vec/Slice's `.len()`
        // (directly or via a local alias) gives an upper bound, only for
        // exclusive ranges (inclusive ranges include the end value, which
        // would be OOB on `v[end]`).
        let pushed_for_bounds =
            self.collect_asserted_bounds_from_for_range(pattern, start, end, inclusive);
        let pushed_for_count = pushed_for_bounds.len();
        self.asserted_index_bounds.extend(pushed_for_bounds);
        // Per-iteration scope frame for body-local lets — the alloca lives
        // for the whole function (entry-block one-shot), but a `let node
        // = SharedT { … }` rebound on every iteration must drop the
        // previous iteration's value before the next store, or the
        // refcount climbs N×K and the chain leaks. Pushing a frame here
        // and draining it just before the increment branch emits one
        // rc_dec per body-local shared-struct let per iteration. Matches
        // the match-arm push/drain pattern in `control_flow_match.rs`.
        // Function-tail `emit_scope_cleanup` no longer walks these
        // bindings (the frame is gone by the time control reaches the
        // function tail), so the slot's null sentinel (emitted by
        // `null_init_slot_in_entry_block` for nested-block shared-struct
        // lets) only matters for the unreachable-body case, not the
        // iterate-then-cleanup case.
        self.scope_cleanup_actions.push(Vec::new());
        self.compile_block(body)?;
        for _ in 0..pushed_for_count {
            self.asserted_index_bounds.pop();
        }
        let body_has_terminator = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        if !body_has_terminator {
            self.drain_top_frame_with_emit();
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        } else {
            // Body ended with a terminator (break / continue / return) —
            // the early-exit path's own cleanup walk already handled
            // every frame in the stack including this one. Pop without
            // emitting so the frame doesn't shadow the surrounding
            // scope's bindings.
            self.scope_cleanup_actions.pop();
        }

        // Increment by `step_val`
        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let next = self.builder.build_int_add(cur, step_val, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn compile_for_slice_var(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();
        let elem_ty = *self.slice_elem_types.get(var_name).unwrap();
        let slice_ptr = self.get_data_ptr(var_name).unwrap();

        let data_pp = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 0, "for.s.data.pp")
            .unwrap();
        let len_p = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 1, "for.s.len.p")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "for.s.data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_p, "for.s.len")
            .unwrap()
            .into_int_value();

        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_int(0, false))
            .unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.s.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.s.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.s.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.s.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, cur, len, "for.s.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[cur], "for.s.elem.ptr")
                .unwrap()
        };
        let elem_val = self
            .builder
            .build_load(elem_ty, elem_ptr, "for.s.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        self.register_for_loop_bindings(pattern, var_name);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let one = i64_t.const_int(1, false);
        let next = self.builder.build_int_add(cur, one, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn compile_for_vec_var(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.vec_elem_type_for_var(var_name);
        let vec_ptr = self.get_data_ptr(var_name).unwrap();

        // Load len and data pointer.
        let len_ptr = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 1, "for.v.len.ptr")
            .unwrap();
        let data_ptr_ptr = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 0, "for.v.data.ptr")
            .unwrap();
        let len = self
            .builder
            .build_load(i64_t, len_ptr, "for.v.len")
            .unwrap()
            .into_int_value();
        let data = self
            .builder
            .build_load(ptr_ty, data_ptr_ptr, "for.v.data")
            .unwrap()
            .into_pointer_value();

        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_int(0, false))
            .unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // Condition: i < len
        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, cur, len, "for.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        // Body: load data[i], bind, execute
        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[cur], "for.v.elem.ptr")
                .unwrap()
        };
        let elem_val = self
            .builder
            .build_load(elem_ty, elem_ptr, "for.v.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        self.register_for_loop_bindings(pattern, var_name);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        // Increment
        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let one = i64_t.const_int(1, false);
        let next = self.builder.build_int_add(cur, one, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    /// Compile `for <pattern> in <s>` and `for <pattern> in <s>.chars()` for
    /// a String variable `<s>`. Loads the `{ptr, len}` from the variable's
    /// String struct alloca and delegates to `compile_for_string_chars_inner`
    /// which emits the actual per-Unicode-scalar-value loop.
    pub(super) fn compile_for_string_chars(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let str_ptr = self.get_data_ptr(var_name).unwrap();
        let len_ptr = self
            .builder
            .build_struct_gep(vec_ty, str_ptr, 1, "for.s.len.ptr")
            .unwrap();
        let data_ptr_ptr = self
            .builder
            .build_struct_gep(vec_ty, str_ptr, 0, "for.s.data.ptr")
            .unwrap();
        let len = self
            .builder
            .build_load(i64_t, len_ptr, "for.s.len")
            .unwrap()
            .into_int_value();
        let data = self
            .builder
            .build_load(ptr_ty, data_ptr_ptr, "for.s.data")
            .unwrap()
            .into_pointer_value();
        self.compile_for_string_chars_inner(label, pattern, data, len, body)
    }

    /// Inner per-char loop driver — takes already-extracted `data` and `len`
    /// from any String value (variable alloca, string literal, interpolated
    /// string, function return). Iterates per Unicode scalar value via the
    /// `karac_string_decode_char` runtime helper. The codepoint is bound as
    /// `i32` (LLVM type for `char`).
    ///
    /// Shape:
    /// - `byte_offset` alloca, initialised to 0.
    /// - `out_codepoint` alloca (i32), populated each iteration by the helper.
    /// - cond block: `byte_offset < len`.
    /// - body block: call `karac_string_decode_char(data, len, byte_offset,
    ///   &out_codepoint)`; bind the pattern to the loaded `i32` codepoint;
    ///   run the user body; store the returned byte offset back.
    /// - incr block: branch back to cond.
    pub(super) fn compile_for_string_chars_inner(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        data: PointerValue<'ctx>,
        len: IntValue<'ctx>,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let i32_t = self.context.i32_type();

        let byte_offset = self.create_entry_alloca(fn_val, "for.s.offset", i64_t.into());
        self.builder
            .build_store(byte_offset, i64_t.const_int(0, false))
            .unwrap();
        let out_codepoint = self.create_entry_alloca(fn_val, "for.s.cp", i32_t.into());

        let cond_bb = self.context.append_basic_block(fn_val, "for.s.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.s.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.s.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.s.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // Condition: byte_offset < len. (Empty string: len == 0, falls
        // straight through to exit.)
        self.builder.position_at_end(cond_bb);
        let cur_off = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), byte_offset, "for.s.off")
            .unwrap()
            .into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::SLT, cur_off, len, "for.s.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        // Body: decode the next char, bind, execute. The decode helper
        // returns the post-char byte offset; stash it for the incr block
        // via the alloca write below.
        self.builder.position_at_end(body_bb);
        let cur_off = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), byte_offset, "for.s.off")
            .unwrap()
            .into_int_value();
        let new_off = self
            .builder
            .build_call(
                self.karac_string_decode_char_fn,
                &[
                    data.into(),
                    len.into(),
                    cur_off.into(),
                    out_codepoint.into(),
                ],
                "for.s.decode",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let cp_val = self
            .builder
            .build_load(i32_t, out_codepoint, "for.s.cp.load")
            .unwrap();
        self.bind_pattern(pattern, cp_val)?;
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            // Stash new_off in the offset alloca so the incr block picks
            // it up. Written here at body-tail rather than at the call
            // site so a mid-body `break` doesn't corrupt the offset
            // (the break path skips this store and exits via exit_bb).
            self.builder.build_store(byte_offset, new_off).unwrap();
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        // Increment: no-op — body already wrote the new offset. Kept as
        // a separate block so `continue` (which branches to incr_bb)
        // routes through one stable label.
        self.builder.position_at_end(incr_bb);
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    /// Compile `for <pattern> in <map_var> { body }`.
    ///
    /// Uses the `karac_map_iter_*` runtime functions:
    /// - `karac_map_iter_new` creates the iterator before the loop.
    /// - `karac_map_iter_next` drives the loop; returns `false` when exhausted.
    /// - `karac_map_iter_free` runs unconditionally in the exit block so it fires
    ///   on both normal exit and `break`.
    ///
    /// The `(K, V)` pair delivered to `bind_pattern` is a two-field struct so
    /// tuple patterns like `for (k, v) in m` work via the existing struct-extract
    /// path in `bind_pattern`.
    pub(super) fn compile_for_map_var(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        self.variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;
        // Use `get_data_ptr` so `for (k, v) in mut_ref_map` unwraps one
        // ref-level before the handle load. Owned bindings yield
        // `slot.ptr` directly.
        let handle_ptr = self
            .get_data_ptr(var_name)
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, handle_ptr, "map.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        // Create the iterator (opaque ptr, lives for the duration of the loop).
        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[map_handle.into()], "map.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Persistent allocas for out_key / out_val — overwritten each iteration.
        let out_key = self.create_entry_alloca(fn_val, "map.iter.key", key_ty);
        let out_val = self.create_entry_alloca(fn_val, "map.iter.val", val_ty);

        let loop_bb = self.context.append_basic_block(fn_val, "map.for.loop");
        let body_bb = self.context.append_basic_block(fn_val, "map.for.body");
        let exit_bb = self.context.append_basic_block(fn_val, "map.for.exit");

        self.builder.build_unconditional_branch(loop_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: loop_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // loop_bb: advance iterator; branch on result.
        self.builder.position_at_end(loop_bb);
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_ptr.into(), out_key.into(), out_val.into()],
                "map.iter.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, body_bb, exit_bb)
            .unwrap();

        // body_bb: load key/val, build {K,V} struct, bind pattern, compile body.
        self.builder.position_at_end(body_bb);
        let key_val = self.builder.build_load(key_ty, out_key, "map.k").unwrap();
        let val_val = self.builder.build_load(val_ty, out_val, "map.v").unwrap();
        let kv_ty = self.context.struct_type(&[key_ty, val_ty], false);
        let mut kv = kv_ty.get_undef();
        kv = self
            .builder
            .build_insert_value(kv, key_val, 0, "kv.k")
            .unwrap()
            .into_struct_value();
        kv = self
            .builder
            .build_insert_value(kv, val_val, 1, "kv.v")
            .unwrap()
            .into_struct_value();
        self.bind_pattern(pattern, kv.into())?;
        self.register_for_loop_bindings(pattern, var_name);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(loop_bb).unwrap();
        }

        self.loop_stack.pop();

        // exit_bb: free iterator — runs on both normal exhaustion and break.
        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_ptr.into()], "")
            .unwrap();

        Ok(i64_t.const_int(0, false).into())
    }

    /// Compile `for x in s { ... }` for a `Set[T]` variable. Mirror of
    /// `compile_for_map_var` — Set lowers to `Map[T, ()]` so the runtime
    /// iterator is the same; the value out-slot is sized 0 (a single
    /// shared `i8` alloca) and discarded since Set iteration produces only
    /// the element. The element pattern is bound directly (no `(k, v)`
    /// destructuring like Map's tuple-shaped iteration delivery).
    pub(super) fn compile_for_set_var(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let i8_t = self.context.i8_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        self.variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown set variable '{var_name}'"))?;
        // Use `get_data_ptr` so `for x in mut_ref_set` unwraps one
        // ref-level before the handle load. Owned bindings yield
        // `slot.ptr` directly.
        let handle_ptr = self
            .get_data_ptr(var_name)
            .ok_or_else(|| format!("unknown set variable '{var_name}'"))?;
        let set_handle = self
            .builder
            .build_load(ptr_ty, handle_ptr, "set.handle")
            .unwrap()
            .into_pointer_value();

        let elem_ty = self
            .set_elem_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[set_handle.into()], "set.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let out_elem = self.create_entry_alloca(fn_val, "set.iter.elem", elem_ty);
        // val_size = 0 in the runtime; the val out-slot is overwritten
        // with zero bytes per iteration so a single `i8` is sufficient.
        let dummy_val = self.create_entry_alloca(fn_val, "set.iter.dummy", i8_t.into());

        let loop_bb = self.context.append_basic_block(fn_val, "set.for.loop");
        let body_bb = self.context.append_basic_block(fn_val, "set.for.body");
        let exit_bb = self.context.append_basic_block(fn_val, "set.for.exit");

        self.builder.build_unconditional_branch(loop_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: loop_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        self.builder.position_at_end(loop_bb);
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_ptr.into(), out_elem.into(), dummy_val.into()],
                "set.iter.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, body_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let elem_val = self
            .builder
            .build_load(elem_ty, out_elem, "set.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        // Re-derive collection side-tables for the bound element so
        // `for x in s.union(t) { x.len() }` etc. dispatch correctly when
        // the element type itself is a Vec/Slice/Map (currently a no-op
        // for scalar Set elements; cheap insurance for the future).
        if let PatternKind::Binding(elem_name) = &pattern.kind {
            if let Some(elem_te) = self.set_elem_type_exprs.get(var_name).cloned() {
                self.register_var_from_type_expr(elem_name, &elem_te);
            }
        }
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(loop_bb).unwrap();
        }

        self.loop_stack.pop();

        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_ptr.into()], "")
            .unwrap();

        Ok(i64_t.const_int(0, false).into())
    }

    pub(super) fn compile_for_array_var(
        &mut self,
        label: Option<&str>,
        pattern: &Pattern,
        arr_ptr: PointerValue<'ctx>,
        arr_ty: inkwell::types::ArrayType<'ctx>,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let len = arr_ty.len() as u64;
        let elem_ty = arr_ty.get_element_type();

        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_int(0, false))
            .unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // Condition: i < N
        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let end_val = i64_t.const_int(len, false);
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, cur, end_val, "for.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        // Body: load arr[i], bind to pattern, compile block
        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let zero = i64_t.const_int(0, false);
        let elem_ptr = unsafe {
            self.builder
                .build_gep(
                    BasicTypeEnum::ArrayType(arr_ty),
                    arr_ptr,
                    &[zero, cur],
                    "for.elem.ptr",
                )
                .unwrap()
        };
        let elem_val = self
            .builder
            .build_load(elem_ty, elem_ptr, "for.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        // Increment
        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let one = i64_t.const_int(1, false);
        let next = self.builder.build_int_add(cur, one, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn compile_for_array_values(
        &mut self,
        pattern: &Pattern,
        elems: &[BasicValueEnum<'ctx>],
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        for &elem in elems {
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_some()
            {
                break;
            }
            self.bind_pattern(pattern, elem)?;
            self.compile_block(body)?;
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }
}
