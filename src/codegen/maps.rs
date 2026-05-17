//! Map-collection codegen: construction, indexing, method dispatch.
//!
//! Houses the `compile_map_*` cluster: `compile_map_new_stmt`,
//! `compile_set_new_stmt`, `compile_map_literal_stmt`,
//! `compile_map_index_store`, `compile_map_index`,
//! `compile_map_keys_values_entries`, and `compile_map_method`.
//!
//! Maps lower to the `karac_map_*` runtime in `runtime/src/map.rs`;
//! these methods marshal arguments / extract results across the FFI
//! boundary and emit the per-K/V hash + eq fns via `synth.rs`.

use crate::ast::*;

use inkwell::types::{BasicType, BasicTypeEnum};
use inkwell::values::BasicValueEnum;
use inkwell::AddressSpace;

use super::state::VarSlot;

impl<'ctx> super::Codegen<'ctx> {
    /// Emit `karac_map_new`, alloca a ptr slot to hold the opaque handle, and
    /// register a scope-exit `karac_map_free` cleanup action.
    /// Called from `compile_stmt` when the RHS is `Map.new()`.
    pub(super) fn compile_map_new_stmt(&mut self, var_name: &str) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

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

        let key_size = key_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let val_size = val_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));

        // Emit (or reuse) hash/eq functions for the concrete key type.
        // Prefer the TypeExpr-aware path so compound key shapes (tuples, …)
        // compose correctly via per-field recursion. The plain
        // `emit_hash_fn_for_type` path is the fallback for code paths that
        // never registered a `TypeExpr` for the variable.
        let (hash_fn, eq_fn) = if let Some(key_te) = self.map_key_type_exprs.get(var_name).cloned()
        {
            (
                self.emit_hash_fn_for_type_expr(&key_te),
                self.emit_eq_fn_for_type_expr(&key_te),
            )
        } else {
            let type_name = self
                .map_key_type_names
                .get(var_name)
                .cloned()
                .unwrap_or_else(|| "i64".to_string());
            (
                self.emit_hash_fn_for_type(&type_name, key_ty),
                self.emit_eq_fn_for_type(&type_name, key_ty),
            )
        };
        let hash_fn_ptr = hash_fn.as_global_value().as_pointer_value();
        let eq_fn_ptr = eq_fn.as_global_value().as_pointer_value();

        let map_handle = self
            .builder
            .build_call(
                self.karac_map_new_fn,
                &[
                    key_size.into(),
                    val_size.into(),
                    hash_fn_ptr.into(),
                    eq_fn_ptr.into(),
                ],
                "map.new",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let fn_val = self.current_fn.unwrap();
        let slot_ptr = self.create_entry_alloca(fn_val, &format!("{var_name}.slot"), ptr_ty.into());
        self.builder.build_store(slot_ptr, map_handle).unwrap();
        self.variables.insert(
            var_name.to_string(),
            VarSlot {
                ptr: slot_ptr,
                ty: ptr_ty.into(),
            },
        );
        let key_is_vec = self.llvm_ty_is_vec_struct(key_ty);
        let val_is_vec = self.llvm_ty_is_vec_struct(val_ty);
        let val_shared_heap = self.map_val_shared_heap_type_for(var_name);
        self.track_map_var(slot_ptr, key_is_vec, val_is_vec, val_shared_heap);
        Ok(())
    }

    /// Compile `let s: Set[T] = Set.new()` — emit `karac_map_new(elem_size,
    /// 0, hash_fn, eq_fn)` (val_size = 0 → key-only table), alloca a slot
    /// for the opaque handle, register the scope-exit `karac_map_free`
    /// cleanup. Mirrors `compile_map_new_stmt` with the value side stripped.
    pub(super) fn compile_set_new_stmt(&mut self, var_name: &str) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let elem_ty = self
            .set_elem_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        let elem_size = elem_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let val_size = i64_t.const_int(0, false);

        // Hash/eq fns for the element type. Prefer the TypeExpr-aware path
        // so compound element shapes (tuples, …) compose correctly.
        let (hash_fn, eq_fn) =
            if let Some(elem_te) = self.set_elem_type_exprs.get(var_name).cloned() {
                (
                    self.emit_hash_fn_for_type_expr(&elem_te),
                    self.emit_eq_fn_for_type_expr(&elem_te),
                )
            } else {
                let type_name = self
                    .set_elem_type_names
                    .get(var_name)
                    .cloned()
                    .unwrap_or_else(|| "i64".to_string());
                (
                    self.emit_hash_fn_for_type(&type_name, elem_ty),
                    self.emit_eq_fn_for_type(&type_name, elem_ty),
                )
            };
        let hash_fn_ptr = hash_fn.as_global_value().as_pointer_value();
        let eq_fn_ptr = eq_fn.as_global_value().as_pointer_value();

        let set_handle = self
            .builder
            .build_call(
                self.karac_map_new_fn,
                &[
                    elem_size.into(),
                    val_size.into(),
                    hash_fn_ptr.into(),
                    eq_fn_ptr.into(),
                ],
                "set.new",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let fn_val = self.current_fn.unwrap();
        let slot_ptr = self.create_entry_alloca(fn_val, &format!("{var_name}.slot"), ptr_ty.into());
        self.builder.build_store(slot_ptr, set_handle).unwrap();
        self.variables.insert(
            var_name.to_string(),
            VarSlot {
                ptr: slot_ptr,
                ty: ptr_ty.into(),
            },
        );
        // Set handles use the same `karac_map_free` cleanup as Map handles —
        // the runtime is the same; only the type-system identity differs.
        // Sets have no value slot (val_size = 0 in the bucket layout), so the
        // value-side recursive drop never applies (`val_is_vec = false`).
        // The KEY side, however, is the element type — `Set[Vec[T]]` /
        // `Set[String]` need `key_is_vec = true` so the runtime helper
        // walks live entries and frees each key's data buffer before
        // deallocating the bucket storage (slice α of the recursive-drop
        // work, 2026-05-14). Primitive-element sets (`Set[i64]`) keep
        // `key_is_vec = false` and stay on plain `karac_map_free`.
        let key_is_vec = self.llvm_ty_is_vec_struct(elem_ty);
        // Sets have no value slot, so `Set[shared T]` does not exist as
        // a leak shape — pass `None` unconditionally for the shared-val
        // heap type. (The element T is the *key*, which is bit-copied
        // into the slot just like Vec keys — shared-key handling is a
        // separate follow-up; see the report for details.)
        self.track_map_var(slot_ptr, key_is_vec, false, None);
        Ok(())
    }

    /// Compile `let m: Map[K, V] = ["k1": v1, "k2": v2, ...]` (bare or prefix
    /// `Map[k1: v1, ...]` form — both lower to `ExprKind::MapLiteral`). Calls
    /// `compile_map_new_stmt` first to build the empty map + register the
    /// binding + cleanup tracking, then inserts each entry via
    /// `karac_map_insert_old` (discarding the previous-value out-slot since
    /// every key is fresh on construction).
    pub(super) fn compile_map_literal_stmt(
        &mut self,
        var_name: &str,
        entries: &[(Expr, Expr)],
    ) -> Result<(), String> {
        // Build the empty map first (registers slot + cleanup).
        self.compile_map_new_stmt(var_name)?;

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();

        let slot = self
            .variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("compile_map_literal_stmt: '{var_name}' not registered"))?;
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

        // Reuse a single set of allocas across all inserts in the literal —
        // the storage is overwritten per iteration.
        let key_slot = self.create_entry_alloca(fn_val, "map.lit.key", key_ty);
        let val_slot = self.create_entry_alloca(fn_val, "map.lit.val", val_ty);
        let old_slot = self.create_entry_alloca(fn_val, "map.lit.old", val_ty);

        for (k_expr, v_expr) in entries {
            let map_handle = self
                .builder
                .build_load(ptr_ty, slot.ptr, "map.lit.handle")
                .unwrap()
                .into_pointer_value();
            let k_val = self.compile_expr(k_expr)?;
            let v_val = self.compile_expr(v_expr)?;
            // Move semantics for tracked Vec/String keys and values —
            // see `Map.insert` arm below for the rationale. Key
            // suppression added alongside the recursive key-drop path
            // (slice α/β, 2026-05-14).
            self.suppress_source_vec_cleanup_for_arg(k_expr);
            self.suppress_source_vec_cleanup_for_arg(v_expr);
            self.builder.build_store(key_slot, k_val).unwrap();
            self.builder.build_store(val_slot, v_val).unwrap();
            self.builder
                .build_call(
                    self.karac_map_insert_old_fn,
                    &[
                        map_handle.into(),
                        key_slot.into(),
                        val_slot.into(),
                        old_slot.into(),
                    ],
                    "map.lit.insert",
                )
                .unwrap();
        }

        Ok(())
    }

    /// Compile `m[k] = v` index-store on a `Map[K, V]` variable. Lowers to
    /// `karac_map_insert_old` and discards the previous-value out-slot. The
    /// write path is uniform regardless of whether the key already exists —
    /// `karac_map_insert_old` overwrites or fresh-inserts as appropriate.
    pub(super) fn compile_map_index_store(
        &mut self,
        name: &str,
        index: &Expr,
        val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        self.variables
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{name}' in index-store"))?;
        // Use `get_data_ptr` so `mut ref Map[K,V]` params unwrap one
        // ref-level before the handle load. Owned bindings yield
        // `slot.ptr` directly.
        let handle_ptr = self
            .get_data_ptr(name)
            .ok_or_else(|| format!("unknown map variable '{name}' in index-store"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, handle_ptr, "map.idxst.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());

        let key_val = self.compile_expr(index)?;
        let fn_val = self.current_fn.unwrap();
        let key_slot = self.create_entry_alloca(fn_val, "map.idxst.key", key_ty);
        let val_slot = self.create_entry_alloca(fn_val, "map.idxst.val", val_ty);
        let old_slot = self.create_entry_alloca(fn_val, "map.idxst.old", val_ty);
        self.builder.build_store(key_slot, key_val).unwrap();
        self.builder.build_store(val_slot, val).unwrap();

        self.builder
            .build_call(
                self.karac_map_insert_old_fn,
                &[
                    map_handle.into(),
                    key_slot.into(),
                    val_slot.into(),
                    old_slot.into(),
                ],
                "map.idxst.existed",
            )
            .unwrap();

        Ok(())
    }

    /// Compile `m[k]` indexing on a `Map[K, V]` variable. Panics at runtime if
    /// the key is missing — matches the spec's `fn index(ref self, key: ref K)
    /// -> ref V` semantics. The returned value is a bit-copy of the bucket's V,
    /// not a borrow into the bucket; this matches the existing `Map.get`
    /// codegen behavior. Proper `ref V` return semantics is a follow-up that
    /// applies uniformly to both `[]` and `Map.get`.
    pub(super) fn compile_map_index(
        &mut self,
        name: &str,
        index: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        self.variables
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{name}' in index expression"))?;
        // Use `get_data_ptr` so `mut ref Map[K,V]` params unwrap one
        // ref-level before the handle load. Owned bindings yield
        // `slot.ptr` directly.
        let handle_ptr = self
            .get_data_ptr(name)
            .ok_or_else(|| format!("unknown map variable '{name}' in index expression"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, handle_ptr, "map.idx.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());

        let key_val = self.compile_expr(index)?;
        let fn_val = self.current_fn.unwrap();
        let key_slot = self.create_entry_alloca(fn_val, "map.idx.key", key_ty);
        let val_slot = self.create_entry_alloca(fn_val, "map.idx.val", val_ty);
        self.builder.build_store(key_slot, key_val).unwrap();

        let found = self
            .builder
            .build_call(
                self.karac_map_get_fn,
                &[map_handle.into(), key_slot.into(), val_slot.into()],
                "map.idx.found",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();

        let found_bb = self.context.append_basic_block(fn_val, "map.idx.found");
        let notfound_bb = self.context.append_basic_block(fn_val, "map.idx.notfound");

        self.builder
            .build_conditional_branch(found, found_bb, notfound_bb)
            .unwrap();

        self.builder.position_at_end(notfound_bb);
        self.emit_panic("Map index: key not present");
        self.builder.build_unreachable().unwrap();

        self.builder.position_at_end(found_bb);
        let elem_val = self
            .builder
            .build_load(val_ty, val_slot, "map.idx.val")
            .unwrap();
        Ok(elem_val)
    }

    /// Compile `Map.keys()`, `Map.values()`, or `Map.entries()` — each
    /// materializes a fresh Vec by iterating the map. Pre-allocates the result
    /// buffer at `karac_map_len` capacity (matches Rust's reserve-then-fill
    /// pattern for known-size collections), then writes elements at index `i`
    /// via the iterator. Returns the resulting Vec struct value `{data, len,
    /// cap}` directly; the receiving binding's let-statement registers it for
    /// scope cleanup via the existing `vec_elem_types` machinery (the type
    /// annotation `let v: Vec[K] = m.keys()` drives that path).
    ///
    /// Iteration order is unspecified — matches the spec at design.md
    /// "Iteration order is unspecified" (line 1588).
    pub(super) fn compile_map_keys_values_entries(
        &mut self,
        var_name: &str,
        method: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let fn_val = self.current_fn.unwrap();

        self.variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;
        // Use `get_data_ptr` so `mut ref Map[K,V]` params unwrap one
        // ref-level before the handle load. Owned bindings yield
        // `slot.ptr` directly.
        let handle_ptr = self
            .get_data_ptr(var_name)
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, handle_ptr, "kvg.map.handle")
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

        // Resulting Vec's element type depends on which method we're emitting.
        // For `entries`, the element is the {K, V} tuple struct — same shape
        // as `extract_vec_elem_type` produces for `Vec[(K, V)]`.
        let elem_ty: BasicTypeEnum<'ctx> = match method {
            "keys" => key_ty,
            "values" => val_ty,
            "entries" => self.context.struct_type(&[key_ty, val_ty], false).into(),
            _ => {
                return Err(format!(
                    "compile_map_keys_values_entries: unexpected method '{method}'"
                ))
            }
        };

        let elem_size = elem_ty.size_of().unwrap();

        // len = karac_map_len(map)
        let len = self
            .builder
            .build_call(self.karac_map_len_fn, &[map_handle.into()], "kvg.len")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();

        // Allocate buffer: malloc(len * elem_size). On len == 0 this calls
        // malloc(0) — implementation-defined; the resulting Vec carries cap=0
        // so scope cleanup never frees it (the bytes leak only on empty maps,
        // a pre-existing pattern shared with empty Vec literals).
        let alloc_bytes = self
            .builder
            .build_int_mul(len, elem_size, "kvg.alloc.bytes")
            .unwrap();
        let buf = self
            .builder
            .build_call(self.malloc_fn, &[alloc_bytes.into()], "kvg.buf")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Map iterator + per-iteration out-slots.
        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[map_handle.into()], "kvg.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let out_key = self.create_entry_alloca(fn_val, "kvg.out.k", key_ty);
        let out_val = self.create_entry_alloca(fn_val, "kvg.out.v", val_ty);
        let i_slot = self.create_entry_alloca(fn_val, "kvg.i", i64_t.into());
        self.builder
            .build_store(i_slot, i64_t.const_int(0, false))
            .unwrap();

        let loop_bb = self.context.append_basic_block(fn_val, "kvg.loop");
        let body_bb = self.context.append_basic_block(fn_val, "kvg.body");
        let exit_bb = self.context.append_basic_block(fn_val, "kvg.exit");

        self.builder.build_unconditional_branch(loop_bb).unwrap();

        // loop_bb: advance iterator; branch on result.
        self.builder.position_at_end(loop_bb);
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_ptr.into(), out_key.into(), out_val.into()],
                "kvg.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, body_bb, exit_bb)
            .unwrap();

        // body_bb: load key/val from slots, build the element value, write
        // into buf[i], increment i.
        self.builder.position_at_end(body_bb);
        let i_val = self
            .builder
            .build_load(i64_t, i_slot, "kvg.i.cur")
            .unwrap()
            .into_int_value();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, buf, &[i_val], "kvg.elem.ptr")
                .unwrap()
        };
        let written: BasicValueEnum<'ctx> = match method {
            "keys" => self
                .builder
                .build_load(key_ty, out_key, "kvg.k.load")
                .unwrap(),
            "values" => self
                .builder
                .build_load(val_ty, out_val, "kvg.v.load")
                .unwrap(),
            "entries" => {
                let kv_struct_ty = self.context.struct_type(&[key_ty, val_ty], false);
                let key_val = self
                    .builder
                    .build_load(key_ty, out_key, "kvg.k.load")
                    .unwrap();
                let val_val = self
                    .builder
                    .build_load(val_ty, out_val, "kvg.v.load")
                    .unwrap();
                let mut kv = kv_struct_ty.get_undef();
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
                kv.into()
            }
            _ => unreachable!(),
        };
        self.builder.build_store(elem_ptr, written).unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "kvg.i.next")
            .unwrap();
        self.builder.build_store(i_slot, i_next).unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();

        // exit_bb: free iterator, build Vec struct {data, len, cap=len}.
        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_ptr.into()], "")
            .unwrap();

        let mut vec_val = vec_ty.get_undef();
        vec_val = self
            .builder
            .build_insert_value(vec_val, buf, 0, "vec.data")
            .unwrap()
            .into_struct_value();
        vec_val = self
            .builder
            .build_insert_value(vec_val, len, 1, "vec.len")
            .unwrap()
            .into_struct_value();
        vec_val = self
            .builder
            .build_insert_value(vec_val, len, 2, "vec.cap")
            .unwrap()
            .into_struct_value();

        Ok(vec_val.into())
    }

    /// Compile a method call on a `Map[K,V]` variable.
    pub(super) fn compile_map_method(
        &mut self,
        var_name: &str,
        method: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        self.variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;

        // Load the opaque map handle. `get_data_ptr` returns the alloca
        // for owned Map params/locals (alloca holds the handle), or the
        // caller's alloca address for `ref Map` / `mut ref Map` params
        // (alloca holds a `*Map`). The single subsequent load yields the
        // opaque handle in both cases — owned reads through one level,
        // ref reads through two levels, with the first level already
        // performed inside `get_data_ptr`.
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

        match method {
            "len" => {
                // Slice 1a: route Map[i64, i64].len through the
                // monomorphized symbol family; every other K/V tuple
                // stays on the erased fallback per § 3.6 coexist.
                let len_fn = if self.should_use_mono_map_for(key_ty, val_ty) {
                    self.get_or_emit_map_mono_methods(key_ty, val_ty).len_fn
                } else {
                    self.karac_map_len_fn
                };
                let len = self
                    .builder
                    .build_call(len_fn, &[map_handle.into()], "map.len")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(len)
            }
            "is_empty" => {
                let len = self
                    .builder
                    .build_call(self.karac_map_len_fn, &[map_handle.into()], "map.len")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let zero = i64_t.const_int(0, false);
                let cmp = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::EQ, len, zero, "map.is_empty")
                    .unwrap();
                Ok(cmp.into())
            }
            "insert" => {
                if args.len() < 2 {
                    return Err("Map.insert requires key and value arguments".to_string());
                }
                let key_val = self.compile_expr(&args[0].value)?;
                let val_val = self.compile_expr(&args[1].value)?;
                // Move semantics — same shape as `Vec.push`. When the
                // key OR value argument is a tracked Vec/String binding,
                // the bucket bit-copies its `{ptr, len, cap}` and the
                // `karac_map_free_with_drop_vec` cleanup would
                // double-free the buffer against the source binding's
                // own scope-exit `FreeVecBuffer`. Suppress the source's
                // cleanup on both sides so the Map becomes the unique
                // owner. Slice α/β (2026-05-14): key suppression added
                // alongside the new key-side drop in the runtime helper.
                self.suppress_source_vec_cleanup_for_arg(&args[0].value);
                self.suppress_source_vec_cleanup_for_arg(&args[1].value);
                let fn_val = self.current_fn.unwrap();
                let old_slot = self.create_entry_alloca(fn_val, "map.insert.old", val_ty);
                // Slice 1b.2a — Map[i64, i64] inserts route through
                // the mono `karac_map_i64_i64_insert_old` symbol (value
                // calling convention: i64 key + i64 val rather than
                // pointer args). The mono body does the stack-alloca
                // forwarding to the erased runtime today; Slice 1b.2b
                // adds the inline fast path.
                //
                // Gate the *compiled value types* against the side-
                // table-derived `key_ty` / `val_ty`. They must
                // agree before routing to mono — the mono fn's
                // calling convention is value-pass per K/V, so a
                // mismatch is an LLVM verifier error. The erased
                // path uses pointers so it tolerates side-table
                // shape drift; mono doesn't. Also catches the
                // pre-existing `ExprKind::CharLit → 0_i64` gap
                // where literal chars compile to i64 zero in
                // `compile_expr` (per Slice 1b notes); the
                // resulting i64 key won't match an i32 key_ty
                // and falls through to erased — same behavior as
                // pre-mono.
                let key_val_matches = key_val.get_type() == key_ty;
                let val_val_matches = val_val.get_type() == val_ty;
                let existed = if self.should_use_mono_map_for(key_ty, val_ty)
                    && key_val_matches
                    && val_val_matches
                {
                    let mono = self.get_or_emit_map_mono_methods(key_ty, val_ty);
                    self.builder
                        .build_call(
                            mono.insert_old_fn,
                            &[
                                map_handle.into(),
                                key_val.into(),
                                val_val.into(),
                                old_slot.into(),
                            ],
                            "map.insert.existed",
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .unwrap_basic()
                        .into_int_value()
                } else {
                    let key_slot = self.create_entry_alloca(fn_val, "map.insert.key", key_ty);
                    let val_slot = self.create_entry_alloca(fn_val, "map.insert.val", val_ty);
                    self.builder.build_store(key_slot, key_val).unwrap();
                    self.builder.build_store(val_slot, val_val).unwrap();
                    self.builder
                        .build_call(
                            self.karac_map_insert_old_fn,
                            &[
                                map_handle.into(),
                                key_slot.into(),
                                val_slot.into(),
                                old_slot.into(),
                            ],
                            "map.insert.existed",
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .unwrap_basic()
                        .into_int_value()
                };
                // Build Option[V]: Some(old) if existed, None if fresh insert.
                let some_bb = self.context.append_basic_block(fn_val, "map.ins.some");
                let none_bb = self.context.append_basic_block(fn_val, "map.ins.none");
                let merge_bb = self.context.append_basic_block(fn_val, "map.ins.merge");
                self.builder
                    .build_conditional_branch(existed, some_bb, none_bb)
                    .unwrap();
                self.builder.position_at_end(some_bb);
                let old_val = self
                    .builder
                    .build_load(val_ty, old_slot, "map.ins.old")
                    .unwrap();
                // Shared-V overwrite-leak fix: when the caller discards
                // the `Option[V]` result (`let _ = m.insert(...)` or a
                // bare `m.insert(...)` statement) AND V is a shared
                // struct / enum, the displaced bucket value's +1 is
                // transferred to the synthesized `Some(old)` payload
                // that no one will hold — so dec it here before the
                // payload is materialized. The flag is set by
                // `compile_stmt`'s discard detection and cleared
                // unconditionally at the next statement; only consume
                // (read + clear) here so a no-op for the discard path
                // doesn't poison the bound-result path. When V isn't
                // shared, `map_val_shared_heap_type_for` returns None
                // and we skip — Vec/String/primitive V's don't have a
                // refcount to dec.
                if self.pending_map_insert_old_dec {
                    self.pending_map_insert_old_dec = false;
                    if let Some(heap_type) = self.map_val_shared_heap_type_for(var_name) {
                        if old_val.is_pointer_value() {
                            self.emit_refcount_dec(
                                var_name,
                                heap_type,
                                old_val.into_pointer_value(),
                            );
                        }
                    }
                }
                // Multi-word payload via `coerce_to_payload_words` — see
                // `Vec.first`/`Vec.last` arm for the rationale.
                let some_payload_words = self.coerce_to_payload_words(old_val, 3)?;
                let some_end_bb = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(none_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                let agg = self.build_option_some_via_phis(
                    &some_payload_words,
                    some_end_bb,
                    none_bb,
                    "ins.opt",
                );
                Ok(agg)
            }
            "get" => {
                if args.is_empty() {
                    return Err("Map.get requires a key argument".to_string());
                }
                let key_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                // Slice 1b.3 — Map[i64, i64].get routes through the
                // mono `karac_map_i64_i64_get` symbol (value calling
                // convention: i64 key). Gate on the compiled value
                // type rather than the side-table `key_ty` for the
                // same reason as the insert arm — see the comment
                // there. Allocate the key/val slots BEFORE the gate
                // so the alloca order in the entry block remains
                // identical between mono and erased paths
                // (`map.get.key` before `map.get.val`); the
                // failing-char-map test passes under that layout.
                let key_slot = self.create_entry_alloca(fn_val, "map.get.key", key_ty);
                let val_slot = self.create_entry_alloca(fn_val, "map.get.val", val_ty);
                self.builder.build_store(key_slot, key_val).unwrap();
                let key_val_matches = key_val.get_type() == key_ty;
                let found = if self.should_use_mono_map_for(key_ty, val_ty) && key_val_matches {
                    let mono = self.get_or_emit_map_mono_methods(key_ty, val_ty);
                    self.builder
                        .build_call(
                            mono.get_fn,
                            &[map_handle.into(), key_val.into(), val_slot.into()],
                            "map.get.found",
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .unwrap_basic()
                        .into_int_value()
                } else {
                    self.builder
                        .build_call(
                            self.karac_map_get_fn,
                            &[map_handle.into(), key_slot.into(), val_slot.into()],
                            "map.get.found",
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .unwrap_basic()
                        .into_int_value()
                };

                let found_bb = self.context.append_basic_block(fn_val, "map.get.found.bb");
                let notfound_bb = self
                    .context
                    .append_basic_block(fn_val, "map.get.notfound.bb");
                let merge_bb = self.context.append_basic_block(fn_val, "map.get.merge");

                self.builder
                    .build_conditional_branch(found, found_bb, notfound_bb)
                    .unwrap();

                // Found — load value and split into payload words.
                self.builder.position_at_end(found_bb);
                let elem_val = self
                    .builder
                    .build_load(val_ty, val_slot, "map.get.val")
                    .unwrap();
                // Multi-word payload via `coerce_to_payload_words` — see
                // `Vec.first`/`Vec.last` arm for the rationale.
                let some_payload_words = self.coerce_to_payload_words(elem_val, 3)?;
                let found_end_bb = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Not found.
                self.builder.position_at_end(notfound_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Merge — phi and build Option struct.
                self.builder.position_at_end(merge_bb);
                let agg = self.build_option_some_via_phis(
                    &some_payload_words,
                    found_end_bb,
                    notfound_bb,
                    "opt",
                );
                Ok(agg)
            }
            "remove" => {
                if args.is_empty() {
                    return Err("Map.remove requires a key argument".to_string());
                }
                let key_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                let key_slot = self.create_entry_alloca(fn_val, "map.remove.key", key_ty);
                let old_slot = self.create_entry_alloca(fn_val, "map.remove.old", val_ty);
                self.builder.build_store(key_slot, key_val).unwrap();
                let found = self
                    .builder
                    .build_call(
                        self.karac_map_remove_old_fn,
                        &[map_handle.into(), key_slot.into(), old_slot.into()],
                        "map.remove.found",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                // Build Option[V]: Some(old) if found, None otherwise.
                let found_bb = self.context.append_basic_block(fn_val, "map.rm.found");
                let notfound_bb = self.context.append_basic_block(fn_val, "map.rm.notfound");
                let merge_bb = self.context.append_basic_block(fn_val, "map.rm.merge");
                self.builder
                    .build_conditional_branch(found, found_bb, notfound_bb)
                    .unwrap();
                self.builder.position_at_end(found_bb);
                let old_val = self
                    .builder
                    .build_load(val_ty, old_slot, "map.rm.old")
                    .unwrap();
                // Multi-word payload via `coerce_to_payload_words` — see
                // `Vec.first`/`Vec.last` arm for the rationale.
                let some_payload_words = self.coerce_to_payload_words(old_val, 3)?;
                let found_end_bb = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(notfound_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                let agg = self.build_option_some_via_phis(
                    &some_payload_words,
                    found_end_bb,
                    notfound_bb,
                    "rm.opt",
                );
                Ok(agg)
            }
            "contains_key" => {
                if args.is_empty() {
                    return Err("Map.contains_key requires a key argument".to_string());
                }
                let key_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                let key_slot = self.create_entry_alloca(fn_val, "map.contains.key", key_ty);
                self.builder.build_store(key_slot, key_val).unwrap();
                let found = self
                    .builder
                    .build_call(
                        self.karac_map_contains_fn,
                        &[map_handle.into(), key_slot.into()],
                        "map.contains",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                Ok(found.into())
            }
            "clear" => {
                self.builder
                    .build_call(self.karac_map_clear_fn, &[map_handle.into()], "")
                    .unwrap();
                // Map.clear returns Unit — codegen represents Unit as i64 0.
                Ok(i64_t.const_int(0, false).into())
            }
            "keys" | "values" | "entries" => self.compile_map_keys_values_entries(var_name, method),
            _ => Err(format!("codegen: Map.{method} not yet implemented")),
        }
    }
}
