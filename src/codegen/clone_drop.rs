//! Synthesized per-type clone and drop functions.
//!
//! Houses the `emit_*_clone_fn` and `emit_*_drop_fn` families plus
//! the `karac_map_insert_fn` lazy-extern accessor consumed inside
//! `emit_map_clone_fn`. Both clone and drop fns lazy-emit per-type
//! and are cached in `clone_fn_cache` / `drop_fn_cache`. Mirrors
//! the dispatch shape of `synth.rs`'s display / hash / eq emitters
//! but lives in its own submodule because the bodies are
//! collection-shape-aware (recurse through Vec/Map/Set/Tuple/String
//! element types via per-shape helpers).

use crate::ast::*;

use inkwell::module::Linkage;
use inkwell::types::{BasicType, BasicTypeEnum};
use inkwell::values::FunctionValue;
use inkwell::AddressSpace;
use inkwell::IntPredicate;

impl<'ctx> super::Codegen<'ctx> {
    /// TypeExpr-aware clone-fn dispatcher. Canonical entry point for any
    /// caller that needs a `void karac_clone_<typename>(*const T, *mut T)`
    /// function for type `T`. Routes by shape: primitives (load+store),
    /// String (call runtime helper), Vec[T] (deep clone with elem
    /// recursion), Map[K, V] (iterate + insert into fresh map),
    /// Set[T] (Map[T, ()]), Tuple (per-field recurse). Mirrors
    /// `emit_display_fn_for_type_expr` / `emit_hash_fn_for_type_expr`.
    /// Cached via `clone_fn_cache` on `display_mangle_te(te)`.
    ///
    /// `#[derive(Clone)]` user struct support is a follow-up — emit at the
    /// derive site by walking field types and recursing through this fn.
    pub(super) fn emit_clone_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::display_mangle_te(te);
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }
        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => self.emit_tuple_clone_fn(elems),
            TypeKind::Path(p) => {
                let head = p.segments.first().map(String::as_str);
                if head == Some("Vec") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        return self.emit_vec_clone_fn(&elem_te);
                    }
                }
                if head == Some("Map") {
                    let args = p.generic_args.as_ref();
                    let k_te = args.and_then(|a| a.first()).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    let v_te = args.and_then(|a| a.get(1)).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    if let (Some(k), Some(v)) = (k_te, v_te) {
                        return self.emit_map_clone_fn(&k, &v);
                    }
                }
                if head == Some("Set") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        // Set[T] clones as Map[T, ()] — same iterator + insert
                        // path with a zero-byte value half. The runtime's
                        // `(key_size + val_size).max(1)` keeps allocations
                        // valid (val_size = 0).
                        let unit_te = TypeExpr {
                            kind: TypeKind::Tuple(Vec::new()),
                            span: elem_te.span.clone(),
                        };
                        return self.emit_map_clone_fn(&elem_te, &unit_te);
                    }
                }
                if head == Some("String") {
                    return self.emit_string_clone_fn();
                }
                // Primitive (or unsupported path) — emit the load+store body.
                self.emit_primitive_clone_fn(&type_name, te)
            }
            _ => self.emit_primitive_clone_fn(&type_name, te),
        }
    }

    /// Emit a primitive `karac_clone_<typename>(*const T, *mut T)` whose
    /// body is `*dst = *src` — single load + store. Covers every Copy-by-
    /// memcpy type (i8…i64, u8…u64, f32/f64, bool, char, unit). Cache-keyed
    /// on `type_name` so repeat callers reuse the same fn.
    pub(super) fn emit_primitive_clone_fn(
        &mut self,
        type_name: &str,
        te: &TypeExpr,
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name.to_string(), f);
            return f;
        }
        let val_ty = self.llvm_type_for_type_expr(te);
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name.to_string(), clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();
        let v = self.builder.build_load(val_ty, src, "v").unwrap();
        self.builder.build_store(dst, v).unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Emit (or fetch) the cloned-String fn — a thin wrapper that just
    /// tail-calls the `karac_string_clone` runtime helper. The wrapper
    /// keeps the per-type clone-fn signature uniform with other types so
    /// callers don't special-case Strings.
    pub(super) fn emit_string_clone_fn(&mut self) -> FunctionValue<'ctx> {
        let type_name = "String".to_string();
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = "karac_clone_String".to_string();
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();
        self.builder
            .build_call(self.karac_string_clone_fn, &[src.into(), dst.into()], "")
            .unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Emit `karac_clone_Vec_<elem>` — read the source `{data, len, cap}`,
    /// allocate a fresh buffer of the same capacity, walk `0..len` calling
    /// the per-element clone fn through the new dispatcher, write the new
    /// `{data, len, cap}` to dst.
    ///
    /// Empty-source fast path (subtask 9): `len == 0` skips the malloc;
    /// dst gets `{null, 0, 0}` with `cap == 0` matching the static-literal
    /// convention so scope-exit cleanup is a no-op.
    pub(super) fn emit_vec_clone_fn(&mut self, elem_te: &TypeExpr) -> FunctionValue<'ctx> {
        let elem_name = Self::display_mangle_te(elem_te);
        let type_name = format!("Vec_{elem_name}");
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i8_t = self.context.i8_type();
        let i64_t = self.context.i64_type();
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);
        // Recurse first — emit may switch the builder's insert block.
        let elem_clone = self.emit_clone_fn_for_type_expr(elem_te);

        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();

        // Load src.{data, len, cap}
        let src_data_pp = self
            .builder
            .build_struct_gep(vec_ty, src, 0, "src.data.pp")
            .unwrap();
        let src_len_p = self
            .builder
            .build_struct_gep(vec_ty, src, 1, "src.len.p")
            .unwrap();
        let src_cap_p = self
            .builder
            .build_struct_gep(vec_ty, src, 2, "src.cap.p")
            .unwrap();
        let src_data = self
            .builder
            .build_load(ptr_ty, src_data_pp, "src.data")
            .unwrap()
            .into_pointer_value();
        let src_len = self
            .builder
            .build_load(i64_t, src_len_p, "src.len")
            .unwrap()
            .into_int_value();
        let src_cap = self
            .builder
            .build_load(i64_t, src_cap_p, "src.cap")
            .unwrap()
            .into_int_value();

        // dst.{data, len, cap} GEPs
        let dst_data_pp = self
            .builder
            .build_struct_gep(vec_ty, dst, 0, "dst.data.pp")
            .unwrap();
        let dst_len_p = self
            .builder
            .build_struct_gep(vec_ty, dst, 1, "dst.len.p")
            .unwrap();
        let dst_cap_p = self
            .builder
            .build_struct_gep(vec_ty, dst, 2, "dst.cap.p")
            .unwrap();

        // Empty fast path: len == 0 → {null, 0, 0}.
        let empty_bb = self.context.append_basic_block(clone_fn, "empty");
        let alloc_bb = self.context.append_basic_block(clone_fn, "alloc");
        let is_empty = self
            .builder
            .build_int_compare(IntPredicate::EQ, src_len, i64_t.const_zero(), "is.empty")
            .unwrap();
        self.builder
            .build_conditional_branch(is_empty, empty_bb, alloc_bb)
            .unwrap();

        self.builder.position_at_end(empty_bb);
        self.builder
            .build_store(dst_data_pp, ptr_ty.const_null())
            .unwrap();
        self.builder
            .build_store(dst_len_p, i64_t.const_zero())
            .unwrap();
        self.builder
            .build_store(dst_cap_p, i64_t.const_zero())
            .unwrap();
        self.builder.build_return(None).unwrap();

        // alloc + memcpy-loop path.
        self.builder.position_at_end(alloc_bb);
        let raw_size = elem_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let elem_size = if raw_size.get_type().get_bit_width() == 64 {
            raw_size
        } else {
            self.builder
                .build_int_z_extend(raw_size, i64_t, "esz64")
                .unwrap()
        };
        // Buffer cap matches src.cap when > 0; otherwise (static-literal
        // source with cap=0 but non-zero len) allocate len-byte buffer.
        let cap_zero = self
            .builder
            .build_int_compare(IntPredicate::EQ, src_cap, i64_t.const_zero(), "cap.zero")
            .unwrap();
        let new_cap = self
            .builder
            .build_select(cap_zero, src_len, src_cap, "new.cap")
            .unwrap()
            .into_int_value();
        let alloc_bytes = self
            .builder
            .build_int_mul(new_cap, elem_size, "alloc.bytes")
            .unwrap();
        let new_data = self
            .builder
            .build_call(self.malloc_fn, &[alloc_bytes.into()], "new.data")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Loop: i in 0..len; call elem_clone(src.data + i*size, new_data + i*size).
        let pre_bb = self.builder.get_insert_block().unwrap();
        let hdr_bb = self.context.append_basic_block(clone_fn, "loop.hdr");
        let bdy_bb = self.context.append_basic_block(clone_fn, "loop.bdy");
        let exit_bb = self.context.append_basic_block(clone_fn, "loop.exit");
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let i_phi = self.builder.build_phi(i64_t, "i").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), pre_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, i_val, src_len, "cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        let offset = self.builder.build_int_mul(i_val, elem_size, "off").unwrap();
        let src_elem = unsafe {
            self.builder
                .build_gep(i8_t, src_data, &[offset], "src.elem")
                .unwrap()
        };
        let dst_elem = unsafe {
            self.builder
                .build_gep(i8_t, new_data, &[offset], "dst.elem")
                .unwrap()
        };
        self.builder
            .build_call(elem_clone, &[src_elem.into(), dst_elem.into()], "")
            .unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "i.next")
            .unwrap();
        i_phi.add_incoming(&[(&i_next, bdy_bb)]);
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        self.builder.build_store(dst_data_pp, new_data).unwrap();
        self.builder.build_store(dst_len_p, src_len).unwrap();
        self.builder.build_store(dst_cap_p, new_cap).unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Emit a Map[K, V] clone fn. Iterates the source via `karac_map_iter_*`,
    /// per-entry: clone K and V into stack allocas, then `karac_map_insert`
    /// into the fresh destination map. Hash/eq fn pointers come from the
    /// existing TypeExpr-aware emit fns, so compound keys (`Map[(i64, String), V]`)
    /// compose correctly.
    ///
    /// Set[T] reuses this path with V = unit (empty-tuple). The runtime's
    /// `(key_size + val_size).max(1)` keeps the bucket allocation valid
    /// when val_size = 0.
    pub(super) fn emit_map_clone_fn(
        &mut self,
        key_te: &TypeExpr,
        val_te: &TypeExpr,
    ) -> FunctionValue<'ctx> {
        let key_name = Self::display_mangle_te(key_te);
        let val_name = Self::display_mangle_te(val_te);
        let type_name = format!("Map_{key_name}_{val_name}");
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let key_ty = self.llvm_type_for_type_expr(key_te);
        let val_ty = self.llvm_type_for_type_expr(val_te);
        let hash_fn = self.emit_hash_fn_for_type_expr(key_te);
        let eq_fn = self.emit_eq_fn_for_type_expr(key_te);
        let key_clone = self.emit_clone_fn_for_type_expr(key_te);
        let val_clone = self.emit_clone_fn_for_type_expr(val_te);

        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();

        // Load source map handle.
        let src_handle = self
            .builder
            .build_load(ptr_ty, src, "src.handle")
            .unwrap()
            .into_pointer_value();

        // Allocate a fresh map. Sizes = sizeof(K), sizeof(V); val_size = 0
        // for Set's unit-tuple case is fine since llvm_type_for_type_expr
        // on empty-tuple returns i64 → size 8. For a true zero-size value,
        // we'd need extra plumbing; the runtime's `.max(1)` already keeps
        // the allocation valid so 8-byte slots are harmless overhead.
        let key_size = key_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let val_size = val_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let new_handle = self
            .builder
            .build_call(
                self.karac_map_new_fn,
                &[
                    key_size.into(),
                    val_size.into(),
                    hash_fn.as_global_value().as_pointer_value().into(),
                    eq_fn.as_global_value().as_pointer_value().into(),
                ],
                "new.map",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Stack allocas for the iterator's key/val out-slots and for the
        // cloned key/val we pass to `karac_map_insert`.
        let key_out = self.create_entry_alloca(clone_fn, "k.out", key_ty);
        let val_out = self.create_entry_alloca(clone_fn, "v.out", val_ty);
        let key_clone_slot = self.create_entry_alloca(clone_fn, "k.clone", key_ty);
        let val_clone_slot = self.create_entry_alloca(clone_fn, "v.clone", val_ty);

        // Iterator handle.
        let iter_handle = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[src_handle.into()], "iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let hdr_bb = self.context.append_basic_block(clone_fn, "iter.hdr");
        let bdy_bb = self.context.append_basic_block(clone_fn, "iter.bdy");
        let exit_bb = self.context.append_basic_block(clone_fn, "iter.exit");
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let has = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_handle.into(), key_out.into(), val_out.into()],
                "iter.has",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        // Clone key and value into fresh allocas, then insert.
        self.builder
            .build_call(key_clone, &[key_out.into(), key_clone_slot.into()], "")
            .unwrap();
        self.builder
            .build_call(val_clone, &[val_out.into(), val_clone_slot.into()], "")
            .unwrap();
        self.builder
            .build_call(
                self.karac_map_insert_fn(),
                &[
                    new_handle.into(),
                    key_clone_slot.into(),
                    val_clone_slot.into(),
                ],
                "",
            )
            .unwrap();
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_handle.into()], "")
            .unwrap();
        self.builder.build_store(dst, new_handle).unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Helper: get-or-declare the `karac_map_insert(map, key, val) -> void`
    /// runtime fn. We don't use `karac_map_insert_old` here because the
    /// fresh destination map is empty by construction — there's no old
    /// value to capture.
    pub(super) fn karac_map_insert_fn(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("karac_map_insert") {
            return f;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into(), ptr_ty.into()], false);
        self.module
            .add_function("karac_map_insert", ty, Some(Linkage::External))
    }

    /// Emit a per-field-recursive clone fn for an n-tuple. Mirrors the
    /// tuple Hash/Eq/Display pattern — recursive per-field calls into the
    /// per-field clone fn via struct GEP.
    pub(super) fn emit_tuple_clone_fn(&mut self, elems: &[TypeExpr]) -> FunctionValue<'ctx> {
        let elems_owned: Vec<TypeExpr> = elems.to_vec();
        let parts: Vec<String> = elems_owned.iter().map(Self::display_mangle_te).collect();
        let type_name = format!("tuple_{}", parts.join("_"));
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }

        let child_fns: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_clone_fn_for_type_expr(e))
            .collect();
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();
        for (i, child_fn) in child_fns.iter().enumerate() {
            let src_field = self
                .builder
                .build_struct_gep(tuple_ty, src, i as u32, &format!("t.f{i}.s"))
                .unwrap();
            let dst_field = self
                .builder
                .build_struct_gep(tuple_ty, dst, i as u32, &format!("t.f{i}.d"))
                .unwrap();
            self.builder
                .build_call(*child_fn, &[src_field.into(), dst_field.into()], "")
                .unwrap();
        }
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    // ── Drop fn framework (per-type drop emitters, mirror of clone) ──

    /// Emit (or fetch) `karac_drop_<typename>(value: *mut T)` for a given
    /// `TypeExpr`. Mirrors `emit_clone_fn_for_type_expr` (`src/codegen.rs:13367`)
    /// — same dispatcher shape with per-shape sub-emitters and the same
    /// cache-by-`display_mangle_te(te)` pattern.
    ///
    /// The emitted fn has signature `void karac_drop_<typename>(*mut T)`.
    /// Body releases any heap the value owns:
    /// - Primitives: no-op (`ret void`).
    /// - String: free the data buffer when `cap > 0` (skips static-literal
    ///   strings whose data lives in the program's read-only string pool).
    /// - Vec[T]: iterate `0..len` calling `emit_drop_fn_for_type_expr(T)` on
    ///   each element, then free the data buffer when `cap > 0`. Improvement
    ///   over the existing `CleanupAction::FreeVecBuffer` cleanup which only
    ///   recurses one level (`Vec[Vec[T]]` works; `Vec[Vec[Vec[T]]]`
    ///   previously leaked the innermost buffers — tracked in `deferred.md`).
    /// - Tuple: iterate fields, calling each field's drop fn through the
    ///   tuple's `build_struct_gep` offsets.
    /// - Map[K, V] / Set[T]: **placeholder this slice (0.c)** — delegates to
    ///   the existing `karac_map_free` runtime. Per-K/V specialization
    ///   happens in Slice 1+ alongside the monomorphized Map layout.
    ///
    /// Caller convention: takes a pointer to the value's storage (not the
    /// value itself). The pointer-by-reference shape mirrors clone so the
    /// dispatcher returns a uniform signature regardless of type shape.
    ///
    /// See [`wip-monomorphized-collections.md`](../docs/implementation_checklist/wip-monomorphized-collections.md)
    /// §3.3 for the locked design position.
    ///
    /// `#[allow(dead_code)]` (and on each sub-emitter below) until Slice 1
    /// lands the first production consumer. End-to-end tests come with
    /// that slice; the framework is foundation only.
    #[allow(dead_code)]
    pub(super) fn emit_drop_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::display_mangle_te(te);
        if let Some(&f) = self.drop_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_drop_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.drop_fn_cache.insert(type_name, f);
            return f;
        }
        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => self.emit_tuple_drop_fn(elems),
            TypeKind::Path(p) => {
                let head = p.segments.first().map(String::as_str);
                if head == Some("Vec") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        return self.emit_vec_drop_fn(&elem_te);
                    }
                }
                if head == Some("Map") {
                    let args = p.generic_args.as_ref();
                    let k_te = args.and_then(|a| a.first()).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    let v_te = args.and_then(|a| a.get(1)).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    if let (Some(k), Some(v)) = (k_te, v_te) {
                        return self.emit_map_drop_fn(&k, &v);
                    }
                }
                if head == Some("Set") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        // Per §3.4 lock: Set[T] drops as Map[T, ()] — same
                        // delegation pattern emit_clone_fn_for_type_expr
                        // uses at line 13402–13416.
                        let unit_te = TypeExpr {
                            kind: TypeKind::Tuple(Vec::new()),
                            span: elem_te.span.clone(),
                        };
                        return self.emit_map_drop_fn(&elem_te, &unit_te);
                    }
                }
                if head == Some("String") {
                    return self.emit_string_drop_fn();
                }
                self.emit_primitive_drop_fn(&type_name)
            }
            _ => self.emit_primitive_drop_fn(&type_name),
        }
    }

    /// Emit `karac_drop_<typename>` for a primitive (Copy-by-memcpy) type.
    /// Body is `ret void` — primitives don't own heap. Cache-keyed on
    /// `type_name` so repeat callers reuse the same fn.
    #[allow(dead_code)]
    pub(super) fn emit_primitive_drop_fn(&mut self, type_name: &str) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_drop_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.drop_fn_cache.insert(type_name.to_string(), f);
            return f;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let drop_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let drop_fn = self
            .module
            .add_function(&fn_name, drop_fn_ty, Some(Linkage::Internal));
        self.drop_fn_cache.insert(type_name.to_string(), drop_fn);

        let entry_bb = self.context.append_basic_block(drop_fn, "entry");
        self.builder.position_at_end(entry_bb);
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        drop_fn
    }

    /// Emit `karac_drop_String` — read `cap`; if `cap > 0`, load `data` and
    /// call `free(data)`. Mirrors the existing scope-exit String cleanup's
    /// cap-zero static-buffer skip (see `CleanupAction::FreeVecBuffer`
    /// handling at `src/codegen.rs:3216+`). Does NOT zero out the `{data,
    /// len, cap}` fields after free — caller's responsibility if the slot
    /// is reused; in scope-exit usage the slot is dead anyway.
    #[allow(dead_code)]
    pub(super) fn emit_string_drop_fn(&mut self) -> FunctionValue<'ctx> {
        let type_name = "String".to_string();
        if let Some(&f) = self.drop_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = "karac_drop_String".to_string();
        if let Some(f) = self.module.get_function(&fn_name) {
            self.drop_fn_cache.insert(type_name, f);
            return f;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let vec_ty = self.vec_struct_type();
        let saved_bb = self.builder.get_insert_block();
        let drop_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let drop_fn = self
            .module
            .add_function(&fn_name, drop_fn_ty, Some(Linkage::Internal));
        self.drop_fn_cache.insert(type_name, drop_fn);

        let entry_bb = self.context.append_basic_block(drop_fn, "entry");
        let free_bb = self.context.append_basic_block(drop_fn, "free");
        let exit_bb = self.context.append_basic_block(drop_fn, "exit");

        self.builder.position_at_end(entry_bb);
        let val = drop_fn.get_nth_param(0).unwrap().into_pointer_value();
        let cap_p = self
            .builder
            .build_struct_gep(vec_ty, val, 2, "cap.p")
            .unwrap();
        let cap = self
            .builder
            .build_load(i64_t, cap_p, "cap")
            .unwrap()
            .into_int_value();
        let is_heap = self
            .builder
            .build_int_compare(IntPredicate::UGT, cap, i64_t.const_zero(), "is.heap")
            .unwrap();
        self.builder
            .build_conditional_branch(is_heap, free_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(free_bb);
        let data_pp = self
            .builder
            .build_struct_gep(vec_ty, val, 0, "data.pp")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "data")
            .unwrap()
            .into_pointer_value();
        self.builder
            .build_call(self.free_fn, &[data.into()], "")
            .unwrap();
        self.builder.build_unconditional_branch(exit_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        drop_fn
    }

    /// Emit `karac_drop_Vec_<elem>` — iterate `0..len` calling the per-
    /// element drop fn on each `data[i]`, then `free(data)` when `cap > 0`.
    /// Strictly recursive: nested `Vec[Vec[T]]` correctly recurses through
    /// the cache to drop every level, closing the deeper-nesting leak the
    /// existing `FreeVecBuffer` cleanup carries (tracked in `deferred.md` §
    /// *Recursive Drop for Heap-Owned Collection Elements*).
    #[allow(dead_code)]
    pub(super) fn emit_vec_drop_fn(&mut self, elem_te: &TypeExpr) -> FunctionValue<'ctx> {
        let elem_name = Self::display_mangle_te(elem_te);
        let type_name = format!("Vec_{elem_name}");
        if let Some(&f) = self.drop_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_drop_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.drop_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);
        // Recurse first — sub-emitter may switch the builder's insert block.
        let elem_drop = self.emit_drop_fn_for_type_expr(elem_te);

        let saved_bb = self.builder.get_insert_block();
        let drop_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let drop_fn = self
            .module
            .add_function(&fn_name, drop_fn_ty, Some(Linkage::Internal));
        self.drop_fn_cache.insert(type_name, drop_fn);

        let entry_bb = self.context.append_basic_block(drop_fn, "entry");
        let loop_cond_bb = self.context.append_basic_block(drop_fn, "loop.cond");
        let loop_body_bb = self.context.append_basic_block(drop_fn, "loop.body");
        let loop_incr_bb = self.context.append_basic_block(drop_fn, "loop.incr");
        let after_loop_bb = self.context.append_basic_block(drop_fn, "after.loop");
        let free_bb = self.context.append_basic_block(drop_fn, "free");
        let exit_bb = self.context.append_basic_block(drop_fn, "exit");

        self.builder.position_at_end(entry_bb);
        let val = drop_fn.get_nth_param(0).unwrap().into_pointer_value();
        let data_pp = self
            .builder
            .build_struct_gep(vec_ty, val, 0, "data.pp")
            .unwrap();
        let len_p = self
            .builder
            .build_struct_gep(vec_ty, val, 1, "len.p")
            .unwrap();
        let cap_p = self
            .builder
            .build_struct_gep(vec_ty, val, 2, "cap.p")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_p, "len")
            .unwrap()
            .into_int_value();
        let cap = self
            .builder
            .build_load(i64_t, cap_p, "cap")
            .unwrap()
            .into_int_value();
        let counter = self.create_entry_alloca(drop_fn, "i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_zero())
            .unwrap();
        self.builder
            .build_unconditional_branch(loop_cond_bb)
            .unwrap();

        // Loop: for i in 0..len { drop(data[i]); }
        self.builder.position_at_end(loop_cond_bb);
        let cur = self
            .builder
            .build_load(i64_t, counter, "i.cur")
            .unwrap()
            .into_int_value();
        let lt = self
            .builder
            .build_int_compare(IntPredicate::ULT, cur, len, "i.lt.len")
            .unwrap();
        self.builder
            .build_conditional_branch(lt, loop_body_bb, after_loop_bb)
            .unwrap();

        self.builder.position_at_end(loop_body_bb);
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[cur], "elem.ptr")
                .unwrap()
        };
        self.builder
            .build_call(elem_drop, &[elem_ptr.into()], "")
            .unwrap();
        self.builder
            .build_unconditional_branch(loop_incr_bb)
            .unwrap();

        self.builder.position_at_end(loop_incr_bb);
        let next = self
            .builder
            .build_int_add(cur, i64_t.const_int(1, false), "i.next")
            .unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder
            .build_unconditional_branch(loop_cond_bb)
            .unwrap();

        // After the per-element loop, free the data buffer if cap > 0
        // (static-literal Vecs with cap=0 skip the free — same convention
        // as the existing FreeVecBuffer cleanup).
        self.builder.position_at_end(after_loop_bb);
        let is_heap = self
            .builder
            .build_int_compare(IntPredicate::UGT, cap, i64_t.const_zero(), "is.heap")
            .unwrap();
        self.builder
            .build_conditional_branch(is_heap, free_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(free_bb);
        self.builder
            .build_call(self.free_fn, &[data.into()], "")
            .unwrap();
        self.builder.build_unconditional_branch(exit_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        drop_fn
    }

    /// Emit `karac_drop_tuple_<f0>_<f1>_...` — iterate fields calling each
    /// field's drop fn through `build_struct_gep` offsets. Empty tuples
    /// (unit type `()`) are handled at the dispatcher by the
    /// `TypeKind::Tuple(elems) if !elems.is_empty()` guard — they fall
    /// through to `emit_primitive_drop_fn` with `type_name = "unit"` (or
    /// whatever `display_mangle_te` produces) and emit a `ret void` no-op.
    #[allow(dead_code)]
    pub(super) fn emit_tuple_drop_fn(&mut self, elems: &[TypeExpr]) -> FunctionValue<'ctx> {
        let elems_owned: Vec<TypeExpr> = elems.to_vec();
        let parts: Vec<String> = elems_owned.iter().map(Self::display_mangle_te).collect();
        let type_name = format!("tuple_{}", parts.join("_"));
        if let Some(&f) = self.drop_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_drop_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.drop_fn_cache.insert(type_name, f);
            return f;
        }

        // Recurse first — sub-emitters may switch the builder's insert block.
        let child_fns: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_drop_fn_for_type_expr(e))
            .collect();
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let drop_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let drop_fn = self
            .module
            .add_function(&fn_name, drop_fn_ty, Some(Linkage::Internal));
        self.drop_fn_cache.insert(type_name, drop_fn);

        let entry_bb = self.context.append_basic_block(drop_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val = drop_fn.get_nth_param(0).unwrap().into_pointer_value();
        for (i, child_fn) in child_fns.iter().enumerate() {
            let field_ptr = self
                .builder
                .build_struct_gep(tuple_ty, val, i as u32, &format!("t.f{i}"))
                .unwrap();
            self.builder
                .build_call(*child_fn, &[field_ptr.into()], "")
                .unwrap();
        }
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        drop_fn
    }

    /// Emit `karac_drop_Map_<K>_<V>` — **placeholder this slice (0.c)**.
    /// Body delegates to the existing `karac_map_free` runtime, preserving
    /// today's type-erased Map drop behavior. Slice 1+ replaces this body
    /// (per K/V tuple) with a monomorphized drop sequence that inlines the
    /// per-K and per-V drops without going through the runtime fn.
    ///
    /// The placeholder exists so the framework is complete enough that
    /// callers can request a drop fn for any TypeExpr; it does not commit
    /// to the monomorphized layout.
    #[allow(dead_code)]
    pub(super) fn emit_map_drop_fn(
        &mut self,
        key_te: &TypeExpr,
        val_te: &TypeExpr,
    ) -> FunctionValue<'ctx> {
        let k_name = Self::display_mangle_te(key_te);
        let v_name = Self::display_mangle_te(val_te);
        let type_name = format!("Map_{k_name}_{v_name}");
        if let Some(&f) = self.drop_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_drop_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.drop_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let drop_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let drop_fn = self
            .module
            .add_function(&fn_name, drop_fn_ty, Some(Linkage::Internal));
        self.drop_fn_cache.insert(type_name, drop_fn);

        let entry_bb = self.context.append_basic_block(drop_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val = drop_fn.get_nth_param(0).unwrap().into_pointer_value();
        // Load the Map handle from the alloca and pass to karac_map_free.
        let handle = self
            .builder
            .build_load(ptr_ty, val, "map.handle")
            .unwrap()
            .into_pointer_value();
        self.builder
            .build_call(self.karac_map_free_fn, &[handle.into()], "")
            .unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        drop_fn
    }
}
