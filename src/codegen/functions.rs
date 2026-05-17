//! Function declaration + body compilation.
//!
//! Houses `apply_linker_attrs` (per-fn attribute lowering for
//! `#[link_name]` / `#[no_mangle]` / `#[used]`), `declare_function`
//! (LLVM `FunctionType` construction from a Kāra `Function` AST node),
//! and `compile_function` (the per-function-body compilation driver).

use crate::ast::*;

use inkwell::module::Linkage;
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum};
use inkwell::values::FunctionValue;
use inkwell::AddressSpace;

use super::state::VarSlot;

impl<'ctx> super::Codegen<'ctx> {
    pub(super) fn apply_linker_attrs(&mut self, fn_val: FunctionValue<'ctx>, attrs: &[Attribute]) {
        for attr in attrs {
            match attr.name.as_str() {
                "link_section" => {
                    // `#[link_section("name")]` — first positional arg or
                    // `string_value` carries the section literal. Skip
                    // silently when neither is present; the parser scaffolding
                    // accepts the attribute but does not yet enforce arg shape.
                    let section = attr.string_value.clone().or_else(|| {
                        attr.args.iter().find_map(|a| match a.value.as_ref() {
                            Some(Expr {
                                kind: ExprKind::StringLit(s),
                                ..
                            }) => Some(s.clone()),
                            _ => None,
                        })
                    });
                    if let Some(s) = section {
                        fn_val.as_global_value().set_section(Some(&s));
                    }
                }
                "no_mangle" => {
                    // No-op: codegen already emits the symbol under its
                    // source-level name. Tracked here so future mangling
                    // passes can opt out.
                }
                "used" if !self.used_symbols.contains(&fn_val) => {
                    self.used_symbols.push(fn_val);
                }
                _ => {}
            }
        }
    }

    pub(super) fn declare_function(
        &mut self,
        func: &Function,
    ) -> Result<FunctionValue<'ctx>, String> {
        if func.name == "main" {
            let main_type = self.context.i32_type().fn_type(&[], false);
            return Ok(self.module.add_function("main", main_type, None));
        }

        let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = func
            .params
            .iter()
            .map(|p| self.llvm_param_type(p))
            .collect();

        let fn_type = match self.llvm_return_type(&func.return_type) {
            Some(BasicTypeEnum::IntType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::FloatType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::PointerType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::StructType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::ArrayType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::VectorType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::ScalableVectorType(_)) | None => {
                self.context.void_type().fn_type(&param_types, false)
            }
        };

        // Record which params are ref for call-site argument passing.
        let ref_flags: Vec<bool> = func
            .params
            .iter()
            .map(|p| matches!(&p.ty.kind, TypeKind::Ref(_) | TypeKind::MutRef(_)))
            .collect();
        self.fn_param_ref.insert(func.name.clone(), ref_flags);
        // Record slice-param element types for call-site coercion.
        let slice_elems: Vec<Option<BasicTypeEnum<'ctx>>> = func
            .params
            .iter()
            .map(|p| self.extract_slice_elem_type(&p.ty))
            .collect();
        self.fn_param_slice_elem
            .insert(func.name.clone(), slice_elems);

        // Record the return-type name (bare `Path` segment) so call-chain
        // field access on a call result can recover its static type — see
        // `compile_field_access` and bug #8 (shared-struct return field
        // access on an unbound call result).
        if let Some(ret_ty) = &func.return_type {
            if let TypeKind::Path(path) = &ret_ty.kind {
                if let Some(seg) = path.segments.first() {
                    self.fn_return_type_names
                        .insert(func.name.clone(), seg.clone());
                }
                // Record the inner shared name when the return type is
                // `Option[shared T]` — read by the let-stmt handler's
                // `RcDecOption` registration for untyped lets whose
                // RHS is a call to this function (`let out = call();`
                // shape; explicit `let out: Option[T] = ...` reads the
                // inner directly off the annotation).
                if let Some((inner_name, _)) = self.option_inner_shared_type_for_type_expr(ret_ty) {
                    self.fn_return_option_inner_shared
                        .insert(func.name.clone(), inner_name);
                }
            }
        }

        // Internal linkage for non-`pub`, non-FFI-marked functions lets LLVM's
        // inliner treat them as private to the translation unit — it can elide
        // the standalone symbol after inlining all callers, and the inliner's
        // cost model is more aggressive with internal callees. `pub` items keep
        // external linkage so future multi-crate compilation can resolve them,
        // and `#[no_mangle]` / `#[used]` keep external so the symbol survives
        // for FFI consumers / link-section anchors. `main` is handled above.
        let linkage = if func.is_pub
            || func.attributes.iter().any(|a| {
                let n = a.name.as_str();
                n == "no_mangle" || n == "used"
            }) {
            Some(Linkage::External)
        } else {
            Some(Linkage::Internal)
        };
        let fn_val = self.module.add_function(&func.name, fn_type, linkage);
        self.apply_linker_attrs(fn_val, &func.attributes);
        Ok(fn_val)
    }

    pub(super) fn compile_function(&mut self, func: &Function) -> Result<(), String> {
        let fn_val = self
            .module
            .get_function(&func.name)
            .ok_or_else(|| format!("Function '{}' not declared", func.name))?;

        self.current_fn = Some(fn_val);
        self.current_fn_name = func.name.clone();
        self.variables.clear();
        self.var_type_names.clear();
        self.var_option_shared_heap.clear();
        self.ref_params.clear();
        self.rc_fallback_heap_types.clear();
        self.scope_cleanup_actions.clear();
        self.scope_cleanup_actions.push(Vec::new());
        // Clear cross-function staging slot. `last_fstr_acc` holds an
        // alloca-valued LLVM pointer scoped to a specific function body;
        // a stale value from a prior function's compilation must not
        // leak into the next. The intra-function take points (Let /
        // Assign / function-tail return for `InterpolatedStringLit`
        // shapes) usually clear it, but a function whose final f-string
        // sits behind a non-tail position (e.g. `let _ = f"…";`) can
        // leave the slot populated.
        self.last_fstr_acc = None;

        let entry = self.context.append_basic_block(fn_val, "entry");
        self.builder.position_at_end(entry);

        if func.name != "main" {
            for (i, param) in func.params.iter().enumerate() {
                let param_name = self.param_name(param);
                let param_val = fn_val.get_nth_param(i as u32).unwrap();
                let alloca = self.create_entry_alloca(fn_val, &param_name, param_val.get_type());
                self.builder.build_store(alloca, param_val).unwrap();
                // Track ref params: alloca holds a pointer-to-data.
                if let Some(inner_ty) = self.inner_type_of_ref(&param.ty) {
                    self.ref_params.insert(param_name.clone(), inner_ty);
                }
                // Register collection / String / struct side-tables for the
                // parameter. Mirrors the let-binding registration in
                // `compile_stmt(StmtKind::Let)` so every `ref T` /
                // `mut ref T` / owned-collection parameter participates in
                // the same method-dispatch surface as a let-bound local.
                //
                // For `ref T` / `mut ref T`, `register_var_from_type_expr`
                // is invoked with the inner type — `Vec`, `Map`, `Set`,
                // `String`, `Slice`, and bare user-type names all flow
                // through the same registrar. Without this, the
                // dispatcher in `compile_method_call` falls through to
                // the "no handler for method 'X' on variable 'v'" error
                // for any `mut ref Map[K,V]` / `mut ref Set[T]` /
                // `mut ref VecDeque[T]` receiver — the structural
                // symmetric of the for-loop binding gap fixed in commit
                // `394cd64` (struct fields in for-loop bodies) but for
                // the parameter-mode case. The fix also covers
                // `mut ref Vec[T]` / `mut ref String` uniformly,
                // collapsing the previous ad-hoc per-shape branches
                // into one call.
                //
                // Owned `Slice[T]` / `mut Slice[T]` params take the
                // type expression as-is (no inner unwrap) — both
                // `MutSlice(inner)` and `Path(Slice[...])` flow through
                // `register_var_from_type_expr`'s slice arm.
                let registration_te: Option<&TypeExpr> = match &param.ty.kind {
                    TypeKind::Ref(inner) | TypeKind::MutRef(inner) => Some(inner.as_ref()),
                    _ => Some(&param.ty),
                };
                if let Some(te) = registration_te {
                    self.register_var_from_type_expr(&param_name, te);
                }
                // Track the declared type name so field/variant lookups work on this param.
                // Both owned (`Type`) and ref-wrapped (`ref Type` / `mut ref Type`)
                // paths feed `var_type_names` with the inner struct/enum name —
                // `field_index_for` needs it to find the field index regardless of
                // whether the param is value-typed or pointer-typed.
                let path_for_type_name = match &param.ty.kind {
                    TypeKind::Path(p) => Some(p),
                    TypeKind::Ref(inner) | TypeKind::MutRef(inner) => match &inner.kind {
                        TypeKind::Path(p) => Some(p),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(path) = path_for_type_name {
                    if let Some(type_name) = path.segments.first() {
                        self.var_type_names
                            .insert(param_name.clone(), type_name.clone());
                        // rc_inc for shared-type parameters (caller keeps its
                        // reference). Only fires for owned Path params — a
                        // shared-typed `ref T` doesn't take ownership, so no
                        // refcount bump.
                        if matches!(&param.ty.kind, TypeKind::Path(_)) {
                            if let Some(info) = self.shared_types.get(type_name.as_str()).cloned() {
                                let ptr = param_val.into_pointer_value();
                                self.emit_refcount_inc(&param_name, info.heap_type, ptr);
                                self.track_rc_var(&param_name, ptr, info.heap_type);
                            }
                        }
                    }
                }
                // `Option[shared T]` parameter registration. The
                // param receives the caller's +1 ref by transfer:
                //   - Identifier-arg caller binding (`shadow(chain)`)
                //     has its RcDecOption cleanup defused at the
                //     call site by
                //     `suppress_source_option_shared_cleanup_for_arg`
                //     (in `call_dispatch.rs`); the chain's +1 moves
                //     into the callee's param slot.
                //   - Call-result direct arg (`shadow(make_chain(10))`)
                //     carries the callee's +1 in the return value's
                //     SSA — no caller-side binding exists, no
                //     suppression needed.
                // Either way, the callee owns one ref on entry; no
                // entry-side `emit_refcount_inc` is needed. The
                // `track_rc_option_var` call queues an `RcDecOption`
                // cleanup so the param's inner ref drops at function
                // exit, and populates `var_option_shared_heap` so the
                // Assign-arm in `compile_stmt` dispatches its dec/inc
                // dance for param-shadowing (`opt = Some(...)` /
                // `opt = other_opt`) — the leak shape the 79a7db8
                // follow-up notes called out. No-op for Option[T]
                // where T isn't a shared struct.
                if let Some((_, info)) = self.option_inner_shared_type_for_type_expr(&param.ty) {
                    let option_ty = self.enum_layouts["Option"].llvm_type;
                    self.track_rc_option_var(&param_name, alloca, option_ty, info.heap_type);
                }
                // RC-fallback boxing for non-shared, non-Vec parameters flagged by the
                // ownership checker. The param value is boxed in {i64 rc, T} on the heap
                // so multiple "consumers" each get a copy of T and the heap object is freed
                // at scope exit when the refcount reaches zero.
                let is_ref_param = self.ref_params.contains_key(&param_name);
                let is_vec_param = self.vec_elem_types.contains_key(&param_name);
                let is_shared_param = if let TypeKind::Path(path) = &param.ty.kind {
                    path.segments
                        .first()
                        .is_some_and(|n| self.shared_types.contains_key(n.as_str()))
                } else {
                    false
                };
                if !is_ref_param
                    && !is_vec_param
                    && !is_shared_param
                    && self.is_rc_fallback_binding(&param_name)
                {
                    let val_ty = param_val.get_type();
                    let heap_type = self
                        .context
                        .struct_type(&[self.context.i64_type().into(), val_ty], false);
                    let heap_ptr = self.emit_rc_alloc(heap_type);
                    let val_field = self
                        .builder
                        .build_struct_gep(heap_type, heap_ptr, 1, "rc_fb_param_val")
                        .unwrap();
                    self.builder.build_store(val_field, param_val).unwrap();
                    // Overwrite alloca to hold heap ptr instead of T.
                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                    let ptr_alloca = self.create_entry_alloca(fn_val, &param_name, ptr_ty.into());
                    self.builder.build_store(ptr_alloca, heap_ptr).unwrap();
                    self.rc_fallback_heap_types
                        .insert(param_name.clone(), heap_type);
                    self.track_rc_var(&param_name, heap_ptr, heap_type);
                    self.variables.insert(
                        param_name,
                        VarSlot {
                            ptr: ptr_alloca,
                            ty: ptr_ty.into(),
                        },
                    );
                    continue;
                }
                self.variables.insert(
                    param_name,
                    VarSlot {
                        ptr: alloca,
                        ty: param_val.get_type(),
                    },
                );
            }
        }

        // Slice 2 (auto-par codegen MVP): route the function body through
        // `compile_function_body`, which dispatches inferred parallel
        // groups to `karac_par_run` when a `ConcurrencyAnalysis` was
        // threaded into codegen. With no analysis, `compile_function_body`
        // falls through to `compile_block` and behavior is unchanged.
        let result = self.compile_function_body(&func.body)?;

        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            // Move-aware scope-exit cleanup for tail-expression
            // returns. When the function's final expression is an
            // Identifier that names a tracked Vec / String binding,
            // the binding's data is being moved into the caller's
            // return value — but `track_vec_var` unconditionally
            // queued a `FreeVecBuffer` cleanup at the let-site, and
            // `emit_scope_cleanup` below would free the buffer the
            // caller now owns. Zero the source's `cap` field before
            // cleanup so `FreeVecBuffer`'s `cap > 0` check skips the
            // free; the returned struct (already loaded into
            // `result`) retains the original cap so the caller's
            // own scope cleanup runs against a valid buffer. Same
            // shape as `suppress_source_vec_cleanup_for_arg` used
            // when a tracked Vec is passed as a call argument.
            //
            // Early `return v` statements bypass `emit_scope_cleanup`
            // entirely (the terminator-already-set guard above), so
            // they don't need this — the move-aware suppression only
            // matters when scope cleanup is about to run.
            self.suppress_cleanup_for_tail_return(&func.body);
            // Sibling to `suppress_cleanup_for_tail_return` for the
            // InterpolatedStringLit-tail case: when the function's final
            // expression is `f"…"`, the loaded {data, len, cap} is the
            // return value — but the f-string accumulator's queued
            // `FreeVecBuffer` would free `data` here, between the return-
            // value load and the `ret` instruction. The caller would
            // receive a struct with a dangling data pointer. Zero the
            // acc's `cap` so its cleanup no-ops; the caller's binding
            // becomes the unique owner (or, for a discarded call result,
            // the caller's expression-statement cleanup takes over).
            // Identifier-tail returns are handled by the existing
            // `suppress_cleanup_for_tail_return` above; the two paths
            // cover the two move-aware tail shapes that produce a String
            // value.
            if matches!(
                func.body.final_expr.as_deref().map(|e| &e.kind),
                Some(ExprKind::InterpolatedStringLit(_))
            ) {
                if let Some(acc) = self.last_fstr_acc.take() {
                    self.zero_vec_alloca_cap(acc);
                }
            }
            self.emit_scope_cleanup();
            if func.name == "main" {
                let zero = self.context.i32_type().const_int(0, false);
                self.builder.build_return(Some(&zero)).unwrap();
            } else if let Some(val) = result {
                self.builder.build_return(Some(&val)).unwrap();
            } else {
                self.builder.build_return(None).unwrap();
            }
        }

        self.scope_cleanup_actions.clear();
        Ok(())
    }
}
