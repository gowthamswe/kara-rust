//! Control-flow codegen: for, while, loop, if, if-let, match, labeled
//! blocks, break, continue, plus the bounds-check elision plumbing.
//!
//! Houses every per-source-construct compiler that establishes basic
//! blocks for control transfer — the `compile_for_*` family,
//! `compile_if` / `compile_if_let`, `compile_while`, `compile_loop`,
//! `compile_labeled_block`, `compile_break` / `compile_continue`,
//! plus `compile_match` and its supporting machinery
//! (`scrutinee_is_borrow_call`, `compile_pattern_condition`,
//! `extract_enum_tag`, `enum_tag_for_variant`, `enum_type_for_variant`,
//! `pattern_payload_word_count`, `pattern_payload_llvm_type`,
//! `reconstruct_payload_value`). Also houses the BCE-related
//! `collect_asserted_bounds_*` / `walk_guard_conjuncts` /
//! `extract_index_bound_from_binop` / `resolve_len_origin`,
//! `resolve_slice_source` / `load_slice_pattern_element` /
//! `compile_slice_pattern_condition` / `bind_slice_pattern`, and
//! `compile_print`.

use crate::ast::*;

use inkwell::types::BasicTypeEnum;
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum};

use super::state::LoopFrame;

impl<'ctx> super::Codegen<'ctx> {
    // ── IfLet ────────────────────────────────────────────────────

    pub(super) fn compile_if_let(
        &mut self,
        pattern: &Pattern,
        value: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let val = self.compile_expr(value)?;
        let cond = self.compile_pattern_condition(pattern, val)?;
        // Reuse if-else codegen
        let fn_val = self.current_fn.unwrap();
        let then_bb = self.context.append_basic_block(fn_val, "iflet.then");
        let else_bb = self.context.append_basic_block(fn_val, "iflet.else");
        let merge_bb = self.context.append_basic_block(fn_val, "iflet.merge");

        self.builder
            .build_conditional_branch(cond.into_int_value(), then_bb, else_bb)
            .unwrap();

        self.builder.position_at_end(then_bb);
        self.bind_pattern_values(pattern, val)?;
        let then_val = self.compile_block(then_block)?;
        let then_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let then_end = self.builder.get_insert_block().unwrap();
        if !then_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(else_bb);
        let else_val = if let Some(eb) = else_branch {
            match &eb.kind {
                ExprKind::Block(blk) => self.compile_block(blk)?,
                _ => Some(self.compile_expr(eb)?),
            }
        } else {
            None
        };
        let else_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let else_end = self.builder.get_insert_block().unwrap();
        if !else_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(merge_bb);
        if let (Some(tv), Some(ev)) = (&then_val, &else_val) {
            if !then_terminated && !else_terminated && tv.get_type() == ev.get_type() {
                let phi = self.builder.build_phi(tv.get_type(), "ifletval").unwrap();
                phi.add_incoming(&[(tv, then_end), (ev, else_end)]);
                return Ok(phi.as_basic_value());
            }
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn compile_print(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let zero = self.context.i64_type().const_int(0, false);
        if args.is_empty() {
            let fmt = self.builder.build_global_string_ptr("\n", "nl").unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[BasicMetadataValueEnum::from(fmt.as_pointer_value())],
                    "printf",
                )
                .unwrap();
            return Ok(zero.into());
        }

        let nl = if name == "println" { "\n" } else { "" };

        // Collection dispatch: when the print arg is a bare identifier that
        // we've registered as a Vec or Map variable, emit a call to the
        // per-type Display fn against the variable's alloca. This is the
        // primary path for `println(v)` on collections; it produces the same
        // formatted output the interpreter prints. Bare Vec/Map values appear
        // as struct/pointer values in the legacy `is_struct_value` /
        // `is_pointer_value` arms below — that path is wrong for collections
        // (Vec gets treated as String; Map gets printed as a raw address) —
        // but those arms are still reachable for non-identifier expressions
        // (function returns, fresh literals) where the source-level type is
        // not in the side-tables, so we leave them in place as fallbacks.
        if let ExprKind::Identifier(var_name) = &args[0].value.kind {
            // Vec[T]: side-table both `vec_elem_types` and `var_elem_type_exprs`
            // are set (the latter is what distinguishes a Vec variable from a
            // String variable, which only sets `vec_elem_types`).
            if self.vec_elem_types.contains_key(var_name)
                && self.var_elem_type_exprs.contains_key(var_name)
            {
                let elem_te = self.var_elem_type_exprs[var_name].clone();
                let slot = self
                    .variables
                    .get(var_name)
                    .copied()
                    .ok_or_else(|| format!("compile_print: '{var_name}' not bound"))?;
                let display_fn = self.emit_vec_display_fn_te(&elem_te);
                self.builder
                    .build_call(display_fn, &[slot.ptr.into()], "vd")
                    .unwrap();
                if !nl.is_empty() {
                    let nl_str = self.builder.build_global_string_ptr("\n", "vd.nl").unwrap();
                    self.builder
                        .build_call(self.printf_fn, &[nl_str.as_pointer_value().into()], "p")
                        .unwrap();
                }
                return Ok(zero.into());
            }
            // Map[K, V]: side-tables hold both K and V `TypeExpr`s.
            if self.map_key_type_exprs.contains_key(var_name)
                && self.var_elem_type_exprs.contains_key(var_name)
            {
                let k_te = self.map_key_type_exprs[var_name].clone();
                let v_te = self.var_elem_type_exprs[var_name].clone();
                let slot = self
                    .variables
                    .get(var_name)
                    .copied()
                    .ok_or_else(|| format!("compile_print: '{var_name}' not bound"))?;
                let display_fn = self.emit_map_display_fn(&k_te, &v_te);
                self.builder
                    .build_call(display_fn, &[slot.ptr.into()], "md")
                    .unwrap();
                if !nl.is_empty() {
                    let nl_str = self.builder.build_global_string_ptr("\n", "md.nl").unwrap();
                    self.builder
                        .build_call(self.printf_fn, &[nl_str.as_pointer_value().into()], "p")
                        .unwrap();
                }
                return Ok(zero.into());
            }
            // Set[T]: side-table holds the element `TypeExpr`.
            if self.set_elem_type_exprs.contains_key(var_name) {
                let elem_te = self.set_elem_type_exprs[var_name].clone();
                let slot = self
                    .variables
                    .get(var_name)
                    .copied()
                    .ok_or_else(|| format!("compile_print: '{var_name}' not bound"))?;
                let display_fn = self.emit_set_display_fn(&elem_te);
                self.builder
                    .build_call(display_fn, &[slot.ptr.into()], "sd")
                    .unwrap();
                if !nl.is_empty() {
                    let nl_str = self.builder.build_global_string_ptr("\n", "sd.nl").unwrap();
                    self.builder
                        .build_call(self.printf_fn, &[nl_str.as_pointer_value().into()], "p")
                        .unwrap();
                }
                return Ok(zero.into());
            }
        }

        let val = self.compile_expr(&args[0].value)?;

        if val.is_int_value() {
            let bits = val.into_int_value().get_type().get_bit_width();
            if bits == 1 {
                let true_s = self
                    .builder
                    .build_global_string_ptr(&format!("true{nl}"), "ts")
                    .unwrap();
                let false_s = self
                    .builder
                    .build_global_string_ptr(&format!("false{nl}"), "fs")
                    .unwrap();
                let sel = self
                    .builder
                    .build_select(
                        val.into_int_value(),
                        true_s.as_pointer_value(),
                        false_s.as_pointer_value(),
                        "bstr",
                    )
                    .unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[BasicMetadataValueEnum::from(sel.into_pointer_value())],
                        "printf",
                    )
                    .unwrap();
            } else {
                let fmt = self
                    .builder
                    .build_global_string_ptr(&format!("%lld{nl}"), "fi")
                    .unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[
                            BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                            BasicMetadataValueEnum::from(val.into_int_value()),
                        ],
                        "printf",
                    )
                    .unwrap();
            }
        } else if val.is_struct_value() {
            // String struct { ptr, i64, i64 } — extract the data pointer for printf %s.
            let str_ptr = self
                .builder
                .build_extract_value(val.into_struct_value(), 0, "str.ptr")
                .unwrap()
                .into_pointer_value();
            let fmt = self
                .builder
                .build_global_string_ptr(&format!("%s{nl}"), "fsp")
                .unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[
                        BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                        BasicMetadataValueEnum::from(str_ptr),
                    ],
                    "printf",
                )
                .unwrap();
        } else if val.is_pointer_value() {
            // Raw pointer (shared types, etc.) — pass directly to %s.
            let fmt = self
                .builder
                .build_global_string_ptr(&format!("%s{nl}"), "fsp")
                .unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[
                        BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                        BasicMetadataValueEnum::from(val.into_pointer_value()),
                    ],
                    "printf",
                )
                .unwrap();
        } else if val.is_float_value() {
            let fmt = self
                .builder
                .build_global_string_ptr(&format!("%g{nl}"), "ff")
                .unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[
                        BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                        BasicMetadataValueEnum::from(val.into_float_value()),
                    ],
                    "printf",
                )
                .unwrap();
        }
        Ok(zero.into())
    }

    // ── Control flow ──────────────────────────────────────────────

    pub(super) fn compile_if(
        &mut self,
        condition: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let cond_val = self.compile_expr(condition)?.into_int_value();
        let fn_val = self.current_fn.unwrap();
        let then_bb = self.context.append_basic_block(fn_val, "then");
        let else_bb = self.context.append_basic_block(fn_val, "else");
        let merge_bb = self.context.append_basic_block(fn_val, "ifmerge");

        self.builder
            .build_conditional_branch(cond_val, then_bb, else_bb)
            .unwrap();

        self.builder.position_at_end(then_bb);
        let then_val = self.compile_block(then_block)?;
        let then_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let then_end_bb = self.builder.get_insert_block().unwrap();
        if !then_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(else_bb);
        let else_val = if let Some(else_expr) = else_branch {
            match &else_expr.kind {
                ExprKind::Block(blk) => self.compile_block(blk)?,
                ExprKind::If {
                    condition: c,
                    then_block: tb,
                    else_branch: eb,
                } => {
                    let v = self.compile_if(c, tb, eb.as_deref())?;
                    Some(v)
                }
                _ => {
                    let v = self.compile_expr(else_expr)?;
                    Some(v)
                }
            }
        } else {
            None
        };
        let else_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let else_end_bb = self.builder.get_insert_block().unwrap();
        if !else_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(merge_bb);

        if let (Some(tv), Some(ev)) = (&then_val, &else_val) {
            if !then_terminated && !else_terminated && tv.get_type() == ev.get_type() {
                let phi = self.builder.build_phi(tv.get_type(), "ifval").unwrap();
                phi.add_incoming(&[(tv, then_end_bb), (ev, else_end_bb)]);
                return Ok(phi.as_basic_value());
            }
        }

        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn compile_while(
        &mut self,
        label: Option<&str>,
        condition: &Expr,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let cond_bb = self.context.append_basic_block(fn_val, "while.cond");
        let body_bb = self.context.append_basic_block(fn_val, "while.body");
        let exit_bb = self.context.append_basic_block(fn_val, "while.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: cond_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        self.builder.position_at_end(cond_bb);
        let cond_val = self.compile_expr(condition)?.into_int_value();
        self.builder
            .build_conditional_branch(cond_val, body_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        // Bounds-check-elision: the guard is true at body entry, so every
        // signed comparison conjunct that establishes an index bound can be
        // pushed as an asserted fact. `compile_vec_index` consults the stack
        // and drops the matching half of its runtime bounds check.
        let pushed_bounds = self.collect_asserted_bounds_from_guard(condition);
        let pushed_count = pushed_bounds.len();
        self.asserted_index_bounds.extend(pushed_bounds);
        // Per-iteration scope frame, same shape as compile_for_range — see
        // its comment for the leak rationale.
        self.scope_cleanup_actions.push(Vec::new());
        self.compile_block(body)?;
        // Pop the bounds we pushed for this loop; restore the surrounding
        // scope's stack untouched. Nested loops therefore see only their
        // own and outer-loop bounds, never inner-loop leftovers.
        for _ in 0..pushed_count {
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
            self.builder.build_unconditional_branch(cond_bb).unwrap();
        } else {
            self.scope_cleanup_actions.pop();
        }

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn compile_loop(
        &mut self,
        label: Option<&str>,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let loop_bb = self.context.append_basic_block(fn_val, "loop.body");
        let exit_bb = self.context.append_basic_block(fn_val, "loop.exit");

        // Allocate a slot for `break value` (i64 by default; refined if used)
        let result_slot =
            self.create_entry_alloca(fn_val, "loop.result", self.context.i64_type().into());

        self.loop_stack.push(LoopFrame {
            label: label.map(str::to_string),
            continue_bb: loop_bb,
            break_bb: exit_bb,
            result_slot: Some(result_slot),
        });

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        // Per-iteration scope frame, same shape as compile_for_range — see
        // its comment for the leak rationale (body-local shared-struct
        // lets re-bound on every iteration would otherwise climb refcount
        // N×K and pin the chain). Drained just before the back-edge to
        // `loop_bb`.
        self.scope_cleanup_actions.push(Vec::new());
        self.compile_block(body)?;
        let body_has_terminator = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        if !body_has_terminator {
            self.drain_top_frame_with_emit();
            self.builder.build_unconditional_branch(loop_bb).unwrap();
        } else {
            self.scope_cleanup_actions.pop();
        }

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        // Load result (may be zero if no break-with-value was hit)
        let result = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(
                self.context.i64_type().into(),
                result_slot,
                "loop.val",
            )
            .unwrap();
        Ok(result)
    }

    /// Compile `label: { body }` (`ExprKind::LabeledBlock`).
    ///
    /// LBC2 / LBC3: allocate an i64 result slot at the entry BB, push a
    /// `LoopFrame` carrying the label and the slot, compile the body,
    /// store the body's tail value (when control falls through normally)
    /// into the slot, branch to a freshly-created `lblock.exit` BB, and
    /// load the slot at the exit. Any `break label expr` inside the body
    /// goes through `compile_break`'s label-aware lookup, stores its
    /// value into the same slot, and branches to the same exit BB.
    ///
    /// Slot LLVM type: i64 today, matching `compile_loop`'s precedent.
    /// The typechecker's LUB constraint already guarantees that for
    /// non-i64-shaped block types, all break sites carry a value of the
    /// same shape — when v1 codegen extends to non-i64 break payloads
    /// (consume `expr_types` lookup), this function and `compile_loop`
    /// flip together. For unit-typed blocks LBC3 specifies the slot is
    /// i64 and `break label` (no value) stores zero.
    ///
    /// `continue_bb` for the frame is a dead `lblock.continue.unreachable`
    /// BB: the resolver rejects `continue label` referring to a labeled
    /// block (`E_CONTINUE_LABEL_BLOCK`), so the BB is never reached at
    /// runtime; pre-allocating it keeps the `LoopFrame` shape uniform.
    pub(super) fn compile_labeled_block(
        &mut self,
        label: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();

        let result_slot = self.create_entry_alloca(fn_val, "lblock.result", i64_t.into());
        // Defense-in-depth zero-init so a never-stored slot loads as 0
        // (matching the unit-equivalent semantics for control paths the
        // typechecker rules out but which a future divergence wouldn't
        // catch).
        self.builder
            .build_store(result_slot, i64_t.const_int(0, false))
            .unwrap();

        let body_bb = self
            .context
            .append_basic_block(fn_val, &format!("lblock.{}.body", label));
        let exit_bb = self
            .context
            .append_basic_block(fn_val, &format!("lblock.{}.exit", label));
        let continue_unreachable_bb = self
            .context
            .append_basic_block(fn_val, &format!("lblock.{}.continue.unreachable", label));

        // Populate the unreachable BB once; it will never branch in.
        // Position back at the previous insert point afterwards.
        let prev_bb = self.builder.get_insert_block();
        self.builder.position_at_end(continue_unreachable_bb);
        self.builder.build_unreachable().unwrap();
        if let Some(bb) = prev_bb {
            self.builder.position_at_end(bb);
        }

        self.builder.build_unconditional_branch(body_bb).unwrap();
        self.builder.position_at_end(body_bb);

        self.loop_stack.push(LoopFrame {
            label: Some(label.to_string()),
            continue_bb: continue_unreachable_bb,
            break_bb: exit_bb,
            result_slot: Some(result_slot),
        });

        // Compile the body. `compile_block` returns the tail expression's
        // value when the block has one; on normal fall-through we store
        // that value into the slot and branch to exit. If the body
        // already terminated (e.g., the tail was an early `break label`,
        // a `return`, or a `panic`), don't add a fall-through branch.
        let tail = self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            if let Some(v) = tail {
                if v.is_int_value() {
                    self.builder.build_store(result_slot, v).unwrap();
                }
            }
            self.builder.build_unconditional_branch(exit_bb).unwrap();
        }

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        let result = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), result_slot, "lblock.val")
            .unwrap();
        Ok(result)
    }

    pub(super) fn compile_break(
        &mut self,
        label: Option<&str>,
        value: Option<&Expr>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let zero = self.context.i64_type().const_int(0, false);
        // LBC1: label-aware lookup. With `Some(l)`, walk the frame stack
        // top-down and pick the first frame whose label matches; with
        // `None`, fall back to the innermost frame. This is what makes
        // `break outer;` actually skip past `inner` when `outer` is the
        // labeled loop / labeled block (today's pre-slice behavior would
        // always pick the innermost — silent miscompile under nested
        // labels, no test fixture exercised it before this slice).
        let frame = match label {
            Some(l) => self
                .loop_stack
                .iter()
                .rev()
                .find(|f| f.label.as_deref() == Some(l))
                .cloned(),
            None => self.loop_stack.last().cloned(),
        };
        if let Some(frame) = frame {
            if let Some(slot) = frame.result_slot {
                let val = if let Some(v) = value {
                    self.compile_expr(v)?
                } else {
                    zero.into()
                };
                // Store break value (only works when types match i64)
                if val.is_int_value() {
                    self.builder.build_store(slot, val).unwrap();
                }
            }
            self.builder
                .build_unconditional_branch(frame.break_bb)
                .unwrap();
        }
        Ok(zero.into())
    }

    pub(super) fn compile_continue(
        &mut self,
        label: Option<&str>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let zero = self.context.i64_type().const_int(0, false);
        // LBC1: same label-aware lookup as `compile_break`. The resolver
        // guarantees `continue label` only resolves to a `Loop`-kind
        // frame, but the codegen-side dispatch is uniform.
        let frame = match label {
            Some(l) => self
                .loop_stack
                .iter()
                .rev()
                .find(|f| f.label.as_deref() == Some(l))
                .cloned(),
            None => self.loop_stack.last().cloned(),
        };
        if let Some(frame) = frame {
            self.builder
                .build_unconditional_branch(frame.continue_bb)
                .unwrap();
        }
        Ok(zero.into())
    }
}
