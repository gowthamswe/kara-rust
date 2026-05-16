//! `<receiver>.clone()` dispatch and Map `entry()` chain compilation.
//!
//! Houses `try_compile_clone` (the identifier-bound collection clone
//! dispatcher) and the Map `entry()` chain lowering family:
//! `try_compile_entry_chain` (dispatcher recognizing
//! `m.entry(k).or_insert(...) / .or_insert_with(...) / .and_modify(...)`),
//! `emit_entry_chain` / `emit_entry_and_modify` (per-arm emission),
//! and `invoke_inline_closure` / `invoke_and_modify_closure` (in-place
//! closure body emission for the `or_insert_with` and `and_modify`
//! callbacks, sidestepping closure-fat-pointer construction).

use crate::ast::*;

use inkwell::types::BasicTypeEnum;
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, PointerValue};
use inkwell::AddressSpace;

use super::state::VarSlot;

impl<'ctx> super::Codegen<'ctx> {
    /// Lower `<receiver>.clone()` for an identifier-bound collection
    /// receiver (Vec[T], String, Map[K, V], Set[T]). Returns `Some(value)`
    /// when the receiver is recognised; `None` otherwise (caller falls
    /// through to the impl-block / generic dispatch so user `clone` impls
    /// keep working).
    ///
    /// Synthesises a `TypeExpr` for the receiver from the codegen side-
    /// tables (`vec_elem_types` / `var_elem_type_exprs` / `map_key_type_exprs`
    /// / `set_elem_type_exprs`), routes through `emit_clone_fn_for_type_expr`,
    /// and emits the `karac_clone_<typename>(src_slot, dst)` call. The
    /// destination is a fresh stack alloca that the caller's let-binding
    /// (or expression-statement) consumes. Scope-cleanup integration for
    /// the cloned value lives in subtask 6 — at this layer the alloca is
    /// just a temporary; the binding's slot inherits ownership when the
    /// `let` stores into it.
    pub(super) fn try_compile_clone(
        &mut self,
        object: &Expr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        let ExprKind::Identifier(name) = &object.kind else {
            return Ok(None);
        };
        let name_owned = name.clone();
        let span_zero = crate::token::Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        };
        let mk_path = |seg: &str, args: Vec<TypeExpr>| -> TypeExpr {
            TypeExpr {
                kind: TypeKind::Path(crate::ast::PathExpr {
                    segments: vec![seg.to_string()],
                    generic_args: if args.is_empty() {
                        None
                    } else {
                        Some(args.into_iter().map(GenericArg::Type).collect())
                    },
                    span: span_zero.clone(),
                }),
                span: span_zero.clone(),
            }
        };

        // Build the receiver's TypeExpr from the side-tables. Order matters
        // — Set/Map come before Vec since Set's bucket is also routed through
        // map_key_types when lowered as Map[T, ()], and a Vec with elem=i8
        // overlaps with String at the LLVM-type level.
        let te: TypeExpr = if self.set_elem_types.contains_key(name_owned.as_str()) {
            let elem = self
                .set_elem_type_exprs
                .get(name_owned.as_str())
                .cloned()
                .ok_or_else(|| {
                    format!("clone: missing set_elem_type_exprs for '{}'", name_owned)
                })?;
            mk_path("Set", vec![elem])
        } else if self.map_key_types.contains_key(name_owned.as_str()) {
            let k = self
                .map_key_type_exprs
                .get(name_owned.as_str())
                .cloned()
                .ok_or_else(|| format!("clone: missing map_key_type_exprs for '{}'", name_owned))?;
            let v = self
                .var_elem_type_exprs
                .get(name_owned.as_str())
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "clone: missing var_elem_type_exprs (val) for '{}'",
                        name_owned
                    )
                })?;
            mk_path("Map", vec![k, v])
        } else if self.vec_elem_types.contains_key(name_owned.as_str()) {
            // Distinguish String from Vec[T]: String registers in
            // `vec_elem_types` (so the str-method dispatch finds it) but
            // skips `var_elem_type_exprs`. Vec[T] populates both.
            if let Some(elem_te) = self.var_elem_type_exprs.get(name_owned.as_str()).cloned() {
                mk_path("Vec", vec![elem_te])
            } else {
                mk_path("String", vec![])
            }
        } else {
            return Ok(None);
        };

        let clone_fn = self.emit_clone_fn_for_type_expr(&te);
        let llvm_ty = self.llvm_type_for_type_expr(&te);
        let fn_val = self
            .current_fn
            .ok_or_else(|| "clone: no current function".to_string())?;
        let dst = self.create_entry_alloca(fn_val, "clone.dst", llvm_ty);
        let src_slot = self
            .variables
            .get(name_owned.as_str())
            .copied()
            .ok_or_else(|| format!("clone: unknown variable '{}'", name_owned))?;
        self.builder
            .build_call(clone_fn, &[src_slot.ptr.into(), dst.into()], "")
            .unwrap();
        let dst_val = self.builder.build_load(llvm_ty, dst, "clone.val").unwrap();
        Ok(Some(dst_val))
    }

    /// Recognise the `Map.entry(k)` chain pattern and lower it as a single
    /// sequence. Returns `Some(value)` only when `<object>.<method>(<args>)`
    /// matches:
    ///
    /// ```text
    /// m.entry(k){.and_modify(f)}*.{or_insert(d)|or_insert_with(f)|and_modify(f)}
    /// ```
    ///
    /// where `m` is an Identifier-bound Map variable. The single `karac_map_entry`
    /// call at the chain root is followed by branch blocks for each
    /// `and_modify` (innermost first) and the terminal method, keeping the
    /// slot pointer valid for the whole sequence — exactly one hash per chain.
    ///
    /// The terminal method's return shape:
    ///
    /// - `or_insert(default)` / `or_insert_with(closure)` — returns the slot
    ///   pointer (`*mut V`), the LLVM realisation of `mut ref V`. Subsequent
    ///   `.push(row)` etc. on the result is the per-type Clone codegen story.
    /// - `and_modify(closure)` — returns the Entry struct value
    ///   `{slot_ptr, occupied}` so further chaining (`.or_insert(d)`) sees
    ///   the same Entry. v1 only nests further `and_modify`s on top; chained
    ///   terminal methods are recognised by recursing through this fn.
    pub(super) fn try_compile_entry_chain(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        if !matches!(method, "or_insert" | "or_insert_with" | "and_modify") {
            return Ok(None);
        }
        // Peel `and_modify` wrappers off the receiver until we reach
        // `m.entry(k)`. Anything else means the receiver isn't an entry chain.
        // Closure exprs collected in outermost-first order; we reverse before
        // emitting so the innermost (= first written) and_modify runs first.
        let mut and_modify_closures: Vec<&Expr> = Vec::new();
        let mut current = object;
        let (map_obj, key_expr) = loop {
            let ExprKind::MethodCall {
                object: inner_obj,
                method: m,
                args: inner_args,
                ..
            } = &current.kind
            else {
                return Ok(None);
            };
            if m == "entry" && inner_args.len() == 1 {
                break (inner_obj.as_ref(), &inner_args[0].value);
            } else if m == "and_modify" && inner_args.len() == 1 {
                and_modify_closures.push(&inner_args[0].value);
                current = inner_obj;
            } else {
                return Ok(None);
            }
        };
        let ExprKind::Identifier(map_name) = &map_obj.kind else {
            return Ok(None);
        };
        if !self.map_key_types.contains_key(map_name.as_str()) {
            return Ok(None);
        }
        let map_name = map_name.clone();
        let value =
            self.emit_entry_chain(&map_name, key_expr, &and_modify_closures, method, args)?;
        Ok(Some(value))
    }

    /// Emit the entry-chain IR. Caller has already verified that
    /// `<map_name>` is a Map variable. Branches happen at every `and_modify`
    /// site and the terminal method, all sharing the slot pointer returned
    /// by the single `karac_map_entry` call.
    pub(super) fn emit_entry_chain(
        &mut self,
        map_name: &str,
        key_expr: &Expr,
        and_modify_closures: &[&Expr],
        terminal_method: &str,
        terminal_args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let map_slot = self
            .variables
            .get(map_name)
            .copied()
            .ok_or_else(|| format!("entry chain: unknown map '{}'", map_name))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, map_slot.ptr, "entry.map.handle")
            .unwrap()
            .into_pointer_value();
        let key_ty = *self
            .map_key_types
            .get(map_name)
            .ok_or_else(|| format!("entry chain: missing key type for '{}'", map_name))?;
        let val_ty = *self
            .map_val_types
            .get(map_name)
            .ok_or_else(|| format!("entry chain: missing val type for '{}'", map_name))?;

        let fn_val = self.current_fn.unwrap();

        // Compile the key, store to alloca for the C ABI.
        let key_alloca = self.create_entry_alloca(fn_val, "entry.key", key_ty);
        let key_val = self.compile_expr(key_expr)?;
        self.builder.build_store(key_alloca, key_val).unwrap();

        // Out-pointer alloca: the runtime writes the slot value-pointer into
        // this slot. The slot pointer is `*mut V` after the call.
        let slot_pp = self.create_entry_alloca(fn_val, "entry.slot.pp", ptr_ty.into());

        // Pick the runtime fn based on the terminal: `or_insert` /
        // `or_insert_with` need the runtime to claim the bucket on Vacant
        // (so codegen can store the default through the slot pointer).
        // Bare `and_modify(...)` must NOT insert on Vacant — use the
        // lookup-only variant.
        let runtime_fn = match terminal_method {
            "or_insert" | "or_insert_with" => self.karac_map_entry_fn,
            "and_modify" => self.karac_map_lookup_slot_fn,
            _ => unreachable!("terminal method already validated by caller"),
        };
        let occupied = self
            .builder
            .build_call(
                runtime_fn,
                &[map_handle.into(), key_alloca.into(), slot_pp.into()],
                "entry.occupied",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let slot_ptr = self
            .builder
            .build_load(ptr_ty, slot_pp, "entry.slot.ptr")
            .unwrap()
            .into_pointer_value();

        // Inner `and_modify` closures — innermost first (chain order is
        // outermost-first; reverse to get execution order).
        for &am_closure in and_modify_closures.iter().rev() {
            self.emit_entry_and_modify(am_closure, occupied, slot_ptr, val_ty)?;
        }

        // Terminal method.
        match terminal_method {
            "or_insert" => {
                if terminal_args.is_empty() {
                    return Err("Entry.or_insert requires a default argument".to_string());
                }
                let store_bb = self.context.append_basic_block(fn_val, "or_ins.store");
                let merge_bb = self.context.append_basic_block(fn_val, "or_ins.merge");
                // Vacant (occupied=false) → store default; Occupied → merge.
                self.builder
                    .build_conditional_branch(occupied, merge_bb, store_bb)
                    .unwrap();
                self.builder.position_at_end(store_bb);
                let default_val = self.compile_expr(&terminal_args[0].value)?;
                self.builder.build_store(slot_ptr, default_val).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(slot_ptr.into())
            }
            "or_insert_with" => {
                if terminal_args.is_empty() {
                    return Err("Entry.or_insert_with requires a closure argument".to_string());
                }
                let store_bb = self.context.append_basic_block(fn_val, "or_ins_w.store");
                let merge_bb = self.context.append_basic_block(fn_val, "or_ins_w.merge");
                self.builder
                    .build_conditional_branch(occupied, merge_bb, store_bb)
                    .unwrap();
                self.builder.position_at_end(store_bb);
                let default_val =
                    self.invoke_inline_closure(&terminal_args[0].value, &[], val_ty)?;
                self.builder.build_store(slot_ptr, default_val).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(slot_ptr.into())
            }
            "and_modify" => {
                if terminal_args.is_empty() {
                    return Err("Entry.and_modify requires a closure argument".to_string());
                }
                self.emit_entry_and_modify(&terminal_args[0].value, occupied, slot_ptr, val_ty)?;
                // Return the Entry struct value `{slot_ptr, occupied}` so a
                // chained terminal sees both halves. Currently no callers
                // consume the struct directly (chained-after-terminal is
                // recognised by the dispatcher), but materialising it keeps
                // the contract honest.
                let entry_struct_ty = self
                    .context
                    .struct_type(&[ptr_ty.into(), self.context.bool_type().into()], false);
                let mut agg = entry_struct_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, slot_ptr, 0, "entry.slot.f")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, occupied, 1, "entry.occ.f")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            _ => unreachable!("terminal method already validated"),
        }
    }

    /// Emit the branch-and-call for one `and_modify(closure)` step. Closure
    /// is invoked only when `occupied` is true; receives the slot pointer
    /// (`*mut V`) so `|v| { v += 1 }` mutates through.
    pub(super) fn emit_entry_and_modify(
        &mut self,
        closure_expr: &Expr,
        occupied: inkwell::values::IntValue<'ctx>,
        slot_ptr: PointerValue<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> Result<(), String> {
        let fn_val = self.current_fn.unwrap();
        let run_bb = self.context.append_basic_block(fn_val, "and_mod.run");
        let cont_bb = self.context.append_basic_block(fn_val, "and_mod.cont");
        self.builder
            .build_conditional_branch(occupied, run_bb, cont_bb)
            .unwrap();
        self.builder.position_at_end(run_bb);
        // The closure's mut-ref-V parameter is realised as a pointer-to-V.
        // We invoke inline with [slot_ptr] so the closure body's mutations
        // through the parameter target the map slot directly. The body's
        // value type is V (loaded once at param bind, stored back at exit).
        self.invoke_and_modify_closure(closure_expr, slot_ptr, val_ty)?;
        self.builder.build_unconditional_branch(cont_bb).unwrap();
        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    /// Invoke a closure expression inline. The closure is compiled to a fat
    /// pointer `{fn_ptr, env_ptr}`; we extract both halves and `build_indirect_call`
    /// with `[env_ptr, ...args]`. Used by `or_insert_with`'s no-arg closure
    /// invocation.
    ///
    /// `expected_return_ty` is the V type the slot stores; the return value
    /// is coerced to it via `coerce_to_i64` and back when needed (in practice
    /// all V types this fn sees fit in a register and round-trip through
    /// the closure return slot losslessly).
    pub(super) fn invoke_inline_closure(
        &mut self,
        closure_expr: &Expr,
        extra_args: &[BasicValueEnum<'ctx>],
        _expected_return_ty: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let closure_val = self.compile_expr(closure_expr)?;
        let fn_type = self
            .pending_closure_fn_type
            .take()
            .ok_or_else(|| "entry chain: inline closure missing fn_type".to_string())?;
        let fat_sv = closure_val.into_struct_value();
        let fn_ptr = self
            .builder
            .build_extract_value(fat_sv, 0, "entry.cls.fn")
            .unwrap()
            .into_pointer_value();
        let env_ptr = self
            .builder
            .build_extract_value(fat_sv, 1, "entry.cls.env")
            .unwrap()
            .into_pointer_value();
        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
            vec![BasicMetadataValueEnum::from(env_ptr)];
        for &arg in extra_args {
            call_args.push(BasicMetadataValueEnum::from(arg));
        }
        let call = self
            .builder
            .build_indirect_call(fn_type, fn_ptr, &call_args, "entry.cls.call")
            .unwrap();
        let basic = call.try_as_basic_value();
        if basic.is_instruction() {
            Ok(self.context.i64_type().const_int(0, false).into())
        } else {
            Ok(basic.unwrap_basic())
        }
    }

    /// Specialised closure-invocation for `and_modify`. The closure's
    /// parameter is `mut ref V` per the spec, but Kāra closures default to
    /// passing user params by value when unannotated (`|v| { v += 1 }`). To
    /// preserve the mut-ref-V semantic without surgery on the closure-param
    /// type-inference path, we inline the closure body directly: bind the
    /// closure parameter name to a local alloca initialised from `slot_ptr`,
    /// run the body, then store the alloca value back through `slot_ptr`.
    /// The closure-fn boundary is bypassed entirely — mutations to the
    /// parameter inside the body are mutations to the slot.
    ///
    /// Restriction: only inline `ExprKind::Closure` exprs are supported (the
    /// common case — `m.entry(k).and_modify(|v| { ... })`). Named-fn forms
    /// like `m.entry(k).and_modify(f)` for a previously-bound `f` would
    /// require the indirect-call path; left unsupported for v1 since the
    /// stdlib spec only documents the inline closure form.
    pub(super) fn invoke_and_modify_closure(
        &mut self,
        closure_expr: &Expr,
        slot_ptr: PointerValue<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> Result<(), String> {
        let ExprKind::Closure { params, body, .. } = &closure_expr.kind else {
            return Err("entry chain: and_modify expects an inline closure expression".to_string());
        };
        // Closure must have exactly one user-side parameter — the `mut ref V`.
        let Some(param) = params.first() else {
            return Err("entry chain: and_modify closure has no parameter".to_string());
        };
        let PatternKind::Binding(param_name) = &param.pattern.kind else {
            return Err(
                "entry chain: and_modify closure parameter must be an identifier".to_string(),
            );
        };
        let fn_val = self.current_fn.unwrap();

        // Bind param to an alloca initialised from the slot. The body's
        // mutations through `param_name` write the alloca; we store back
        // to `slot_ptr` after the body exits.
        let local = self.create_entry_alloca(fn_val, param_name, val_ty);
        let initial = self
            .builder
            .build_load(val_ty, slot_ptr, "entry.am.load")
            .unwrap();
        self.builder.build_store(local, initial).unwrap();
        let saved_slot = self.variables.insert(
            param_name.clone(),
            VarSlot {
                ptr: local,
                ty: val_ty,
            },
        );

        // Compile the body in the enclosing scope so it can see captures
        // (the typical case: `|v| { v += 1 }` only reads param-local `v`).
        // body is an Expr; if it's a block we evaluate for side effects.
        let _body_val = self.compile_expr(body)?;

        // Restore the prior binding (if any) and write back the mutated V.
        match saved_slot {
            Some(prev) => {
                self.variables.insert(param_name.clone(), prev);
            }
            None => {
                self.variables.remove(param_name);
            }
        }
        let new_v = self
            .builder
            .build_load(val_ty, local, "entry.am.new")
            .unwrap();
        self.builder.build_store(slot_ptr, new_v).unwrap();
        Ok(())
    }
}
