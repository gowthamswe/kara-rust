//! Drop-fn synthesis: per-type `__karac_drop_<T>` LLVM functions.
//!
//! Phase 7.2 Slice DP. Houses the two drop-fn synthesisers:
//!
//! - `emit_enum_drop_switch` — for value-type enums with heap-bearing
//!   payloads. Body is a tag switch with per-variant cleanup BBs that
//!   emit the `cap > 0 ? free(data)` pattern for each
//!   `EnumDropKind::VecOrString` field. Memoized in `enum_drop_fns`;
//!   returns `None` for enums with no heap-bearing payload.
//! - `emit_struct_drop_synthesis` — for value-type structs. Walks each
//!   field's drop kind and emits the matching cleanup IR.
//!
//! Lives in a sibling `impl<'ctx> super::Codegen<'ctx>` block.

use inkwell::basic_block::BasicBlock;
use inkwell::module::Linkage;
use inkwell::values::FunctionValue;
use inkwell::AddressSpace;
use inkwell::IntPredicate;

use super::state::EnumDropKind;

impl<'ctx> super::Codegen<'ctx> {
    /// Phase 7.2 Slice DP — synthesize (or reuse) the per-enum drop
    /// function `__karac_drop_<EnumName>` for value-type enums.
    ///
    /// Body shape:
    /// ```text
    /// fn __karac_drop_E(p: *const E) {
    ///   let tag = (*p).tag;
    ///   switch tag {
    ///     0 => cleanup_variant_0(p);
    ///     1 => cleanup_variant_1(p);
    ///     ...
    ///     default => {}
    ///   }
    ///   ret void
    /// }
    /// ```
    ///
    /// Each per-variant cleanup BB walks the variant's
    /// `field_drop_kinds`; for every `EnumDropKind::VecOrString` field
    /// the BB emits the same `cap > 0 ? free(data)` pattern that
    /// `CleanupAction::FreeVecBuffer` uses inline at the top-level
    /// scope-cleanup drain. Field word offsets come from
    /// `EnumLayout::field_word_offsets` (laid out by `declare_enums`).
    ///
    /// Returns `None` when the enum has no heap-bearing payload anywhere
    /// — saves the synth cost and lets `track_enum_var` skip
    /// registration entirely (no payload to free, no IR bloat from a
    /// tag-switch with all-`ret` arms).
    ///
    /// Lazily memoized in `enum_drop_fns`. Mirrors the existing
    /// `emit_hash_fn_for_type` lazy-synth pattern: the saved insert
    /// block is restored on exit so callers don't have to.
    pub(super) fn emit_enum_drop_switch(&mut self, enum_name: &str) -> Option<FunctionValue<'ctx>> {
        if let Some(f) = self.enum_drop_fns.get(enum_name) {
            return Some(*f);
        }
        // Snapshot what we need before mutably borrowing `self.module`
        // / `self.builder`. The layout is reconstituted from
        // `enum_layouts`; we clone the relevant pieces so the loop body
        // doesn't fight the builder over `&mut self`.
        let layout = self.enum_layouts.get(enum_name)?.clone();
        if layout.is_shared {
            return None; // DP3 — shared enums use RC machinery
        }
        // Skip enums whose every variant has zero heap-bearing fields.
        let any_heap = layout
            .field_drop_kinds
            .values()
            .any(|kinds| kinds.iter().any(|k| *k != EnumDropKind::None));
        if !any_heap {
            return None;
        }

        let fn_name = format!("__karac_drop_{enum_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.enum_drop_fns.insert(enum_name.to_string(), f);
            return Some(f);
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let void_ty = self.context.void_type();
        let vec_ty = self.vec_struct_type();

        let saved_bb = self.builder.get_insert_block();

        let drop_fn_ty = void_ty.fn_type(&[ptr_ty.into()], false);
        let drop_fn = self
            .module
            .add_function(&fn_name, drop_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(drop_fn, "entry");
        let exit_bb = self.context.append_basic_block(drop_fn, "exit");
        self.builder.position_at_end(entry_bb);
        let p_arg = drop_fn.get_nth_param(0).unwrap().into_pointer_value();

        // Load tag (field 0 of the enum struct).
        let tag_ptr = self
            .builder
            .build_struct_gep(layout.llvm_type, p_arg, 0, "drop.tag.p")
            .unwrap();
        let tag_val = self
            .builder
            .build_load(i64_t, tag_ptr, "drop.tag")
            .unwrap()
            .into_int_value();

        // Sort variants by tag for deterministic IR. `tags` HashMap
        // doesn't preserve insertion order; sorting on the discriminant
        // makes the BB layout reproducible across runs.
        let mut tag_entries: Vec<(String, u64)> =
            layout.tags.iter().map(|(n, t)| (n.clone(), *t)).collect();
        tag_entries.sort_by_key(|(_, t)| *t);

        // One BB per variant, all branching to `exit_bb` after their
        // cleanup work.
        let mut switch_cases: Vec<(inkwell::values::IntValue<'ctx>, BasicBlock<'ctx>)> = Vec::new();
        let case_bbs: Vec<(String, u64, BasicBlock<'ctx>)> = tag_entries
            .iter()
            .map(|(name, tag)| {
                let bb = self
                    .context
                    .append_basic_block(drop_fn, &format!("drop.{}", name));
                switch_cases.push((i64_t.const_int(*tag, false), bb));
                (name.clone(), *tag, bb)
            })
            .collect();

        self.builder
            .build_switch(tag_val, exit_bb, &switch_cases)
            .unwrap();

        // Per-variant cleanup BBs — for each heap-bearing payload field
        // (`EnumDropKind::VecOrString`), reload the (data, len, cap)
        // payload words and free the data pointer when cap > 0.
        for (variant_name, _tag, bb) in &case_bbs {
            self.builder.position_at_end(*bb);
            if let Some(kinds) = layout.field_drop_kinds.get(variant_name) {
                if let Some(offsets) = layout.field_word_offsets.get(variant_name) {
                    for (kind, (start_word, _num_words)) in kinds.iter().zip(offsets.iter()) {
                        if *kind != EnumDropKind::VecOrString {
                            continue;
                        }
                        // Field index in `llvm_type` is `start_word + 1`
                        // for the data ptr (tag is field 0); +2 for len;
                        // +3 for cap. Match the insert-side at
                        // `try_compile_enum_variant`.
                        let data_idx = (*start_word + 1) as u32;
                        let cap_idx = (*start_word + 3) as u32;

                        let cap_ptr = self
                            .builder
                            .build_struct_gep(layout.llvm_type, p_arg, cap_idx, "drop.cap.p")
                            .unwrap();
                        let cap_val = self
                            .builder
                            .build_load(i64_t, cap_ptr, "drop.cap")
                            .unwrap()
                            .into_int_value();
                        let zero = i64_t.const_int(0, false);
                        let is_heap = self
                            .builder
                            .build_int_compare(IntPredicate::UGT, cap_val, zero, "drop.is_heap")
                            .unwrap();
                        let free_bb = self.context.append_basic_block(drop_fn, "drop.free");
                        let skip_bb = self.context.append_basic_block(drop_fn, "drop.skip");
                        self.builder
                            .build_conditional_branch(is_heap, free_bb, skip_bb)
                            .unwrap();

                        self.builder.position_at_end(free_bb);
                        // Payload words are stored as i64 at the start_word
                        // slot — for VecOrString that's the data pointer
                        // bit-cast to i64. Load it and convert back to
                        // a pointer for the free call.
                        let data_word_ptr = self
                            .builder
                            .build_struct_gep(layout.llvm_type, p_arg, data_idx, "drop.data.wp")
                            .unwrap();
                        let data_word = self
                            .builder
                            .build_load(i64_t, data_word_ptr, "drop.data.w")
                            .unwrap()
                            .into_int_value();
                        let data_ptr = self
                            .builder
                            .build_int_to_ptr(data_word, ptr_ty, "drop.data.p")
                            .unwrap();
                        self.builder
                            .build_call(self.free_fn, &[data_ptr.into()], "")
                            .unwrap();
                        // After freeing, zero the cap word so a
                        // re-entrant invocation (via aliased binding,
                        // unusual in v1 but defensive) becomes a no-op
                        // through the cap > 0 guard. Mirrors the
                        // FreeVecBuffer semantics implicitly carried by
                        // the runtime's own grow/clear paths.
                        self.builder.build_store(cap_ptr, zero).unwrap();
                        self.builder.build_unconditional_branch(skip_bb).unwrap();

                        self.builder.position_at_end(skip_bb);
                    }
                }
            }
            // Reference the vec_ty so the unused-binding lint stays
            // quiet on builds that don't enter the inner loop with
            // VecOrString fields. (Most do, but the suppress here keeps
            // the helper robust to future drop-kind additions.)
            let _ = vec_ty;
            self.builder.build_unconditional_branch(exit_bb).unwrap();
        }

        self.builder.position_at_end(exit_bb);
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        self.enum_drop_fns.insert(enum_name.to_string(), drop_fn);
        Some(drop_fn)
    }

    /// TypeExpr-aware hash-fn wrapper. Dispatches tuples to a recursive
    /// composition (per-field hash + FNV tail-mix combine) and falls through
    /// Synthesize (or fetch from cache) a per-struct drop function for a
    /// non-shared user struct. Returns `None` when the struct has no
    /// heap-owning fields (every field is primitive / Slice / Ref / etc.)
    /// — in that case no cleanup is needed and `track_struct_var` skips
    /// `CleanupAction::StructDrop` registration entirely. Otherwise emits
    /// `__karac_drop_struct_<Name>(*mut StructTy)` once per struct type
    /// (cached in `struct_drop_fns`) that iterates fields and frees:
    ///
    /// - **Vec / String fields** (`{ptr, len, cap}` layout): load `cap`,
    ///   if > 0 free `(field).data`. Same shape as `FreeVecBuffer`'s
    ///   inline cleanup, just GEP'd into the struct.
    /// - **Map / Set handle fields** (single `ptr`): call
    ///   `karac_map_free` (primitive K / V) or `karac_map_free_with_drop_vec`
    ///   when the field's K or V is itself Vec/String. The drop fn does
    ///   NOT have per-field-instance K/V type info — it conservatively
    ///   routes every Map/Set field to `karac_map_free_with_drop_vec`
    ///   with both flags set, which is correct (the runtime helper
    ///   reads no key/value heap when the relevant size is 0 or the
    ///   field's `cap == 0`).
    ///
    /// Limited to direct Vec/String/Map/Set fields. Nested-struct /
    /// enum / Vec[Vec[T]] field types are NOT recursed in this slice
    /// — that's slice δ's `emit_drop_fn_for_type` framework. Field
    /// type identification uses `struct_field_type_names` (first path
    /// segment of each field's source TypeExpr), so a field typed
    /// `Vec[i64]` is detected by its first segment "Vec".
    pub(super) fn emit_struct_drop_synthesis(
        &mut self,
        struct_name: &str,
    ) -> Option<FunctionValue<'ctx>> {
        if let Some(f) = self.struct_drop_fns.get(struct_name) {
            return Some(*f);
        }
        // Shared structs use the RC machinery; their cleanup is via
        // `track_rc_var` / `emit_refcount_dec`, not a synthesized
        // per-value drop fn.
        if self.shared_types.contains_key(struct_name) {
            return None;
        }
        let st = *self.struct_types.get(struct_name)?;
        let field_kinds = self.struct_field_type_names.get(struct_name)?.clone();

        // Classify each field: Vec/String (vec-struct layout), Map/Set
        // (single ptr handle), or no-cleanup. If every field is no-cleanup,
        // skip emission entirely — `track_struct_var` will get `None`
        // and skip the `StructDrop` cleanup action.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum FieldDrop {
            None,
            VecOrString,
            MapOrSet,
        }
        let kinds: Vec<FieldDrop> = field_kinds
            .iter()
            .map(|opt_name| match opt_name.as_deref() {
                Some("Vec") | Some("VecDeque") | Some("String") => FieldDrop::VecOrString,
                Some("Map") | Some("HashMap") | Some("Set") | Some("HashSet") => {
                    FieldDrop::MapOrSet
                }
                _ => FieldDrop::None,
            })
            .collect();
        if kinds.iter().all(|k| *k == FieldDrop::None) {
            return None;
        }

        let fn_name = format!("__karac_drop_struct_{struct_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.struct_drop_fns.insert(struct_name.to_string(), f);
            return Some(f);
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let i32_t = self.context.i32_type();
        let void_ty = self.context.void_type();
        let vec_ty = self.vec_struct_type();

        let saved_bb = self.builder.get_insert_block();

        let drop_fn_ty = void_ty.fn_type(&[ptr_ty.into()], false);
        let drop_fn = self
            .module
            .add_function(&fn_name, drop_fn_ty, Some(Linkage::Internal));
        self.struct_drop_fns
            .insert(struct_name.to_string(), drop_fn);

        let entry_bb = self.context.append_basic_block(drop_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let p_arg = drop_fn.get_nth_param(0).unwrap().into_pointer_value();

        for (field_idx, kind) in kinds.iter().enumerate() {
            match kind {
                FieldDrop::None => {}
                FieldDrop::VecOrString => {
                    // GEP the Vec struct field within the parent struct.
                    let field_ptr = self
                        .builder
                        .build_struct_gep(
                            st,
                            p_arg,
                            field_idx as u32,
                            &format!("drop.field{field_idx}.p"),
                        )
                        .unwrap();
                    // Load cap (Vec struct field index 2).
                    let cap_ptr = self
                        .builder
                        .build_struct_gep(
                            vec_ty,
                            field_ptr,
                            2,
                            &format!("drop.field{field_idx}.cap.p"),
                        )
                        .unwrap();
                    let cap = self
                        .builder
                        .build_load(i64_t, cap_ptr, &format!("drop.field{field_idx}.cap"))
                        .unwrap()
                        .into_int_value();
                    let zero = i64_t.const_int(0, false);
                    let is_heap = self
                        .builder
                        .build_int_compare(
                            IntPredicate::UGT,
                            cap,
                            zero,
                            &format!("drop.field{field_idx}.is_heap"),
                        )
                        .unwrap();
                    let free_bb = self
                        .context
                        .append_basic_block(drop_fn, &format!("drop.field{field_idx}.free"));
                    let skip_bb = self
                        .context
                        .append_basic_block(drop_fn, &format!("drop.field{field_idx}.skip"));
                    self.builder
                        .build_conditional_branch(is_heap, free_bb, skip_bb)
                        .unwrap();
                    self.builder.position_at_end(free_bb);
                    let data_ptr_ptr = self
                        .builder
                        .build_struct_gep(
                            vec_ty,
                            field_ptr,
                            0,
                            &format!("drop.field{field_idx}.data.p"),
                        )
                        .unwrap();
                    let data = self
                        .builder
                        .build_load(ptr_ty, data_ptr_ptr, &format!("drop.field{field_idx}.data"))
                        .unwrap()
                        .into_pointer_value();
                    self.builder
                        .build_call(self.free_fn, &[data.into()], "")
                        .unwrap();
                    self.builder.build_unconditional_branch(skip_bb).unwrap();
                    self.builder.position_at_end(skip_bb);
                }
                FieldDrop::MapOrSet => {
                    // Map/Set field is a single opaque ptr stored inline.
                    // Load the handle; if non-null, route to
                    // `karac_map_free_with_drop_vec(handle, 1, 1)` —
                    // conservatively drop both sides. The runtime helper
                    // is a no-op for the side whose `cap == 0` or whose
                    // `_size == 0`, so over-flagging is correctness-safe
                    // even on `Map[i64, i64]` / `Set[i64]` fields (those
                    // never had a `data` ptr to free; `cap == 0` skips
                    // the free). When per-field K/V type info is wired
                    // through (slice δ), tighten the flags to the
                    // minimum needed.
                    //
                    // Shared-half rc_dec walks (item 4, 2026-05-16):
                    // when the field's K or V is a shared struct /
                    // shared enum, emit the per-bucket walk BEFORE
                    // the runtime free releases the bucket storage —
                    // mirrors the `CleanupAction::FreeMapHandle`
                    // ordering. Without this, a `struct Owner { m:
                    // Map[i64, Node] }` (with `Node` shared) drops
                    // the Map's bucket storage without ever dec'ing
                    // the shared values, leaking one ref per live
                    // entry. The runtime helper is type-erased and
                    // can't see the shared layout itself.
                    let field_ptr = self
                        .builder
                        .build_struct_gep(
                            st,
                            p_arg,
                            field_idx as u32,
                            &format!("drop.field{field_idx}.p"),
                        )
                        .unwrap();
                    let handle = self
                        .builder
                        .build_load(ptr_ty, field_ptr, &format!("drop.field{field_idx}.handle"))
                        .unwrap()
                        .into_pointer_value();
                    if let Some(field_te) = self
                        .struct_field_type_exprs
                        .get(struct_name)
                        .and_then(|v| v.get(field_idx))
                        .cloned()
                    {
                        // The walks build their own basic blocks via
                        // `self.current_fn.unwrap()`, which at this
                        // point still points at the outer function
                        // that triggered drop synthesis (e.g.,
                        // `main`). Swap to `drop_fn` so the blocks
                        // attach to the synthesized drop body, then
                        // restore. Save / restore matches the
                        // surrounding `saved_bb` discipline so the
                        // outer caller's builder position is
                        // untouched.
                        let saved_fn = self.current_fn;
                        self.current_fn = Some(drop_fn);
                        if let Some((k_te, v_te)) = super::helpers::map_kv_type_exprs(&field_te) {
                            if let Some(heap_ty) = self.shared_heap_type_for_type_expr(&v_te) {
                                self.emit_map_shared_half_rc_dec_walk(handle, heap_ty, true);
                            }
                            if let Some(heap_ty) = self.shared_heap_type_for_type_expr(&k_te) {
                                self.emit_map_shared_half_rc_dec_walk(handle, heap_ty, false);
                            }
                        } else if let Some(elem_te) = super::helpers::set_inner_type_expr(&field_te)
                        {
                            // `Set[T]` lowers to `Map[T, ()]`; the
                            // element occupies the key half.
                            if let Some(heap_ty) = self.shared_heap_type_for_type_expr(&elem_te) {
                                self.emit_map_shared_half_rc_dec_walk(handle, heap_ty, false);
                            }
                        }
                        self.current_fn = saved_fn;
                    }
                    let one = i32_t.const_int(1, false);
                    self.builder
                        .build_call(
                            self.karac_map_free_with_drop_vec_fn,
                            &[handle.into(), one.into(), one.into()],
                            "",
                        )
                        .unwrap();
                }
            }
        }

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        Some(drop_fn)
    }
}
