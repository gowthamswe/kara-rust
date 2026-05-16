//! Function declaration + body compilation.
//!
//! Houses `apply_linker_attrs` (per-fn attribute lowering for
//! `#[link_name]` / `#[no_mangle]` / `#[used]`), `declare_function`
//! (LLVM `FunctionType` construction from a Kāra `Function` AST node),
//! and `compile_function` (the per-function-body compilation driver).

use crate::ast::*;

use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum};
use inkwell::values::FunctionValue;
use inkwell::AddressSpace;

use super::helpers::{map_kv_type_exprs, slice_inner_type_expr, vec_inner_type_expr};
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

        let fn_val = self.module.add_function(&func.name, fn_type, None);
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
        self.ref_params.clear();
        self.rc_fallback_heap_types.clear();
        self.scope_cleanup_actions.clear();
        self.scope_cleanup_actions.push(Vec::new());

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
                    // Also track vec_elem_types for ref Vec/String params.
                    if let TypeKind::Ref(inner) | TypeKind::MutRef(inner) = &param.ty.kind {
                        if let Some(elem) = self.extract_vec_elem_type(inner) {
                            self.vec_elem_types.insert(param_name.clone(), elem);
                            if let Some(inner_te) = vec_inner_type_expr(inner) {
                                self.var_elem_type_exprs
                                    .insert(param_name.clone(), inner_te);
                            }
                        }
                        if self.is_string_type_expr(inner) {
                            self.vec_elem_types
                                .insert(param_name.clone(), self.context.i8_type().into());
                            self.string_vars.insert(param_name.clone());
                        }
                    }
                }
                // Track slice params: both `Slice[T]` and `mut Slice[T]` use
                // the 2-field `{ptr, i64}` representation. Recording the
                // element type here lets indexing and iteration dispatch on
                // the slice shape.
                if let Some(elem) = self.extract_slice_elem_type(&param.ty) {
                    self.slice_elem_types.insert(param_name.clone(), elem);
                    if let Some(inner_te) = slice_inner_type_expr(&param.ty) {
                        self.var_elem_type_exprs
                            .insert(param_name.clone(), inner_te);
                    }
                }
                // Track owned `Vec[T]` / `String` params. Without this,
                // `vec_elem_types` only knows about the `ref Vec[T]` case
                // and local let-bound Vecs; slice patterns and other
                // element-typed dispatch sites that look up the param's
                // element type on the side-table would otherwise miss.
                if matches!(&param.ty.kind, TypeKind::Path(_)) {
                    if let Some(elem) = self.extract_vec_elem_type(&param.ty) {
                        self.vec_elem_types.insert(param_name.clone(), elem);
                        if let Some(inner_te) = vec_inner_type_expr(&param.ty) {
                            self.var_elem_type_exprs
                                .insert(param_name.clone(), inner_te);
                        }
                    }
                    if self.is_string_type_expr(&param.ty) {
                        self.vec_elem_types
                            .insert(param_name.clone(), self.context.i8_type().into());
                        self.string_vars.insert(param_name.clone());
                    }
                }
                // Track Map params: both K and V LLVM types + per-position
                // TypeExprs so `for (k, v) in m` can register each binding.
                if let Some((k_ty, v_ty)) = self.extract_map_kv_types(&param.ty) {
                    self.map_key_types.insert(param_name.clone(), k_ty);
                    self.map_val_types.insert(param_name.clone(), v_ty);
                    if let Some(k_name) = Self::extract_map_key_name(&param.ty) {
                        self.map_key_type_names.insert(param_name.clone(), k_name);
                    }
                    if let Some((k_te, v_te)) = map_kv_type_exprs(&param.ty) {
                        self.map_key_type_exprs.insert(param_name.clone(), k_te);
                        self.var_elem_type_exprs.insert(param_name.clone(), v_te);
                    }
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
