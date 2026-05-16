//! Synthesized per-type helper functions: hash, eq, drop, and display.
//!
//! Houses the emit_*_for_type / emit_*_for_type_expr / emit_*_for_tuple
//! family of methods that lazily synthesize per-type LLVM functions
//! for hashing, equality, dropping, and display rendering. These
//! functions are emitted on first demand and cached in the matching
//! `hash_fn_cache` / `eq_fn_cache` / `enum_drop_fns` / `struct_drop_fns`
//! / `display_fn_cache` field on `Codegen`.
//!
//! Includes the FxHash byte-loop primitive `emit_fxhash_over_bytes`
//! consumed by every `emit_hash_fn_*` site, plus the `display_mangle_te`
//! type-name mangler used to key the display cache.

use crate::ast::*;

use inkwell::basic_block::BasicBlock;
use inkwell::module::Linkage;
use inkwell::types::{BasicType, BasicTypeEnum};
use inkwell::values::{FunctionValue, IntValue, PointerValue};
use inkwell::AddressSpace;
use inkwell::IntPredicate;

use super::state::EnumDropKind;

impl<'ctx> super::Codegen<'ctx> {
    // ── Map codegen ───────────────────────────────────────────────

    /// FxHash multiplier — rustc-hash style. Picked by the
    /// `bench/hash_quality/` investigation (2026-05-15) as the
    /// fastest non-cryptographic hash on karac's per-K hash bench
    /// matrix (4-8× faster than FNV-1a on common workloads;
    /// geometric mean 0.56× of FNV-1a baseline across 18 cells).
    /// Mixed via rotate-left-5 + XOR + multiply per chunk.
    const FXHASH_SEED: u64 = 0x517c_c1b7_2722_0a95;
    const FXHASH_ROTATE: u64 = 5;

    /// Emit an FxHash byte loop over `byte_count` bytes starting at
    /// `data_ptr`. Per-byte step is `h = h.rotate_left(5) ^ byte;
    /// h = h * FXHASH_SEED`. Appends basic blocks to `hash_fn_val`.
    /// Builder must be positioned just before the first block of
    /// the loop; on return it is positioned at the exit block.
    /// Returns the accumulated hash `IntValue` (i64).
    ///
    /// For fixed-size `≤8`-byte primitive keys, prefer the inline
    /// fast-path in `emit_hash_fn_for_type` (one zext + one
    /// multiply, no loop) — it produces the same hash output as
    /// this byte loop when the loop runs the same byte count from
    /// an all-zero initial accumulator, because `rotate_left(0, 5)
    /// = 0` and the loop body collapses to `h = byte * SEED` on
    /// iteration 0. Wider primitives and variable-length keys
    /// (Vec, String, Slice) fall through to this byte loop.
    pub(super) fn emit_fxhash_over_bytes(
        &mut self,
        hash_fn_val: FunctionValue<'ctx>,
        data_ptr: PointerValue<'ctx>,
        byte_count: IntValue<'ctx>,
    ) -> IntValue<'ctx> {
        let i64_t = self.context.i64_type();
        let i8_t = self.context.i8_type();
        let seed = i64_t.const_int(Self::FXHASH_SEED, false);
        let rotate_amt = i64_t.const_int(Self::FXHASH_ROTATE, false);
        let rotate_inv = i64_t.const_int(64 - Self::FXHASH_ROTATE, false);

        let pre_bb = self.builder.get_insert_block().unwrap();
        let hdr_bb = self.context.append_basic_block(hash_fn_val, "fx.hdr");
        let bdy_bb = self.context.append_basic_block(hash_fn_val, "fx.bdy");
        let exit_bb = self.context.append_basic_block(hash_fn_val, "fx.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let i_phi = self.builder.build_phi(i64_t, "fx.i").unwrap();
        let hash_phi = self.builder.build_phi(i64_t, "fx.hash").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), pre_bb)]);
        hash_phi.add_incoming(&[(&i64_t.const_zero(), pre_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let hash_val = hash_phi.as_basic_value().into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, i_val, byte_count, "fx.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        let byte_ptr = unsafe {
            self.builder
                .build_gep(i8_t, data_ptr, &[i_val], "fx.bp")
                .unwrap()
        };
        let byte = self
            .builder
            .build_load(i8_t, byte_ptr, "fx.b")
            .unwrap()
            .into_int_value();
        let byte64 = self
            .builder
            .build_int_z_extend(byte, i64_t, "fx.b64")
            .unwrap();
        // rotate_left(h, 5) == (h << 5) | (h >> 59)
        let shl = self
            .builder
            .build_left_shift(hash_val, rotate_amt, "fx.shl")
            .unwrap();
        let shr = self
            .builder
            .build_right_shift(hash_val, rotate_inv, false, "fx.shr")
            .unwrap();
        let rotated = self.builder.build_or(shl, shr, "fx.rot").unwrap();
        let xored = self.builder.build_xor(rotated, byte64, "fx.xor").unwrap();
        let new_hash = self.builder.build_int_mul(xored, seed, "fx.mul").unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "fx.i1")
            .unwrap();
        i_phi.add_incoming(&[(&i_next, bdy_bb)]);
        hash_phi.add_incoming(&[(&new_hash, bdy_bb)]);
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        hash_val
    }

    /// Emit (or reuse) a module-level `karac_hash_{type_name}(ptr) -> i64` function.
    ///
    /// Per the `bench/hash_quality/` investigation (2026-05-15),
    /// karac's per-K hash is **FxHash** (rustc-hash style
    /// rotate-xor-multiply over 8-byte chunks). Geometric mean
    /// across 18 bench cells: 0.56× of the prior FNV-1a baseline
    /// (1.8× faster overall, up to 4-8× faster on integer keys).
    ///
    /// - **Integer primitives `≤8` bytes** (i8, i16, i32, i64,
    ///   char, bool): inline fast path — load value, zero-extend
    ///   to i64, multiply by `FXHASH_SEED`. One zext + one mul,
    ///   no loop. The initial accumulator is 0, so the per-byte
    ///   shape `h.rotate_left(5) ^ byte; h * SEED` collapses to
    ///   `value * SEED` when processed as a single chunk.
    /// - **`String`**: loads `{ ptr data, i64 len }` from the
    ///   struct and runs the FxHash byte loop over `data[0..len]`.
    /// - **Float primitives** (f32, f64) and **wider integers**
    ///   (i128, u128): byte loop over `sizeof(K)` raw bytes.
    /// - **Structs / other**: byte loop over raw struct bytes
    ///   (correct for value-only structs; tuple combiner in
    ///   `emit_hash_fn_for_tuple` per-field-recurses).
    pub(super) fn emit_hash_fn_for_type(
        &mut self,
        type_name: &str,
        key_ty: BasicTypeEnum<'ctx>,
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_hash_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let saved_bb = self.builder.get_insert_block();

        let hash_fn_ty = i64_t.fn_type(&[ptr_ty.into()], false);
        let hash_fn = self
            .module
            .add_function(&fn_name, hash_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(hash_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let key_ptr = hash_fn.get_nth_param(0).unwrap().into_pointer_value();

        if type_name == "String" || type_name == "str" {
            // String struct: { ptr data, i64 len, i64 cap }
            let str_ty = self.vec_struct_type();
            let data_pp = self
                .builder
                .build_struct_gep(str_ty, key_ptr, 0, "s.data.pp")
                .unwrap();
            let data_ptr = self
                .builder
                .build_load(ptr_ty, data_pp, "s.data")
                .unwrap()
                .into_pointer_value();
            let len_p = self
                .builder
                .build_struct_gep(str_ty, key_ptr, 1, "s.len.p")
                .unwrap();
            let len = self
                .builder
                .build_load(i64_t, len_p, "s.len")
                .unwrap()
                .into_int_value();
            let hash = self.emit_fxhash_over_bytes(hash_fn, data_ptr, len);
            self.builder.build_return(Some(&hash)).unwrap();
        } else if let BasicTypeEnum::IntType(int_ty) = key_ty {
            // Integer primitive fast path: load value, zext to
            // i64, multiply by FXHASH_SEED. Matches the byte-loop
            // output for the i==0 case from an all-zero
            // accumulator (rotate(0, 5) = 0 → 0 ^ value = value;
            // value * SEED).
            let bit_width = int_ty.get_bit_width();
            if bit_width <= 64 {
                let raw = self
                    .builder
                    .build_load(int_ty, key_ptr, "fx.prim.raw")
                    .unwrap()
                    .into_int_value();
                let value64 = if bit_width == 64 {
                    raw
                } else {
                    self.builder
                        .build_int_z_extend(raw, i64_t, "fx.prim.zext")
                        .unwrap()
                };
                let seed = i64_t.const_int(Self::FXHASH_SEED, false);
                let hash = self
                    .builder
                    .build_int_mul(value64, seed, "fx.prim.mul")
                    .unwrap();
                self.builder.build_return(Some(&hash)).unwrap();
            } else {
                // Wider integers (i128 / u128): fall back to byte loop.
                let raw_size = key_ty
                    .size_of()
                    .unwrap_or_else(|| i64_t.const_int(8, false));
                let size64 = if raw_size.get_type().get_bit_width() == 64 {
                    raw_size
                } else {
                    self.builder
                        .build_int_z_extend(raw_size, i64_t, "ksz64")
                        .unwrap()
                };
                let hash = self.emit_fxhash_over_bytes(hash_fn, key_ptr, size64);
                self.builder.build_return(Some(&hash)).unwrap();
            }
        } else {
            // Float primitives, structs, other compound types:
            // FxHash byte loop over `sizeof(K)` raw bytes.
            let raw_size = key_ty
                .size_of()
                .unwrap_or_else(|| i64_t.const_int(8, false));
            let size64 = if raw_size.get_type().get_bit_width() == 64 {
                raw_size
            } else {
                self.builder
                    .build_int_z_extend(raw_size, i64_t, "ksz64")
                    .unwrap()
            };
            let hash = self.emit_fxhash_over_bytes(hash_fn, key_ptr, size64);
            self.builder.build_return(Some(&hash)).unwrap();
        }

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        hash_fn
    }

    /// Emit (or reuse) a module-level `karac_eq_{type_name}(ptr, ptr) -> i1` function.
    ///
    /// - Integer primitives: load both values and `icmp eq`.
    /// - `String`: compare lengths then byte-by-byte.
    /// - Structs/other: byte-by-byte over raw `sizeof(K)` bytes.
    pub(super) fn emit_eq_fn_for_type(
        &mut self,
        type_name: &str,
        key_ty: BasicTypeEnum<'ctx>,
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_eq_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let i8_t = self.context.i8_type();
        let bool_t = self.context.bool_type();

        let saved_bb = self.builder.get_insert_block();

        let eq_fn_ty = bool_t.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let eq_fn = self
            .module
            .add_function(&fn_name, eq_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(eq_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let a_ptr = eq_fn.get_nth_param(0).unwrap().into_pointer_value();
        let b_ptr = eq_fn.get_nth_param(1).unwrap().into_pointer_value();

        if type_name == "String" || type_name == "str" {
            // String: compare lengths first, then byte-by-byte on content.
            let str_ty = self.vec_struct_type();
            let la_p = self
                .builder
                .build_struct_gep(str_ty, a_ptr, 1, "la.p")
                .unwrap();
            let lb_p = self
                .builder
                .build_struct_gep(str_ty, b_ptr, 1, "lb.p")
                .unwrap();
            let len_a = self
                .builder
                .build_load(i64_t, la_p, "la")
                .unwrap()
                .into_int_value();
            let len_b = self
                .builder
                .build_load(i64_t, lb_p, "lb")
                .unwrap()
                .into_int_value();

            let neq_bb = self.context.append_basic_block(eq_fn, "neq");
            let bytes_bb = self.context.append_basic_block(eq_fn, "bytes");

            let len_eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, len_a, len_b, "len.eq")
                .unwrap();
            self.builder
                .build_conditional_branch(len_eq, bytes_bb, neq_bb)
                .unwrap();

            // neq_bb: return false
            self.builder.position_at_end(neq_bb);
            self.builder
                .build_return(Some(&bool_t.const_int(0, false)))
                .unwrap();

            // bytes_bb: load data ptrs, enter byte loop
            self.builder.position_at_end(bytes_bb);
            let da_p = self
                .builder
                .build_struct_gep(str_ty, a_ptr, 0, "da.p")
                .unwrap();
            let db_p = self
                .builder
                .build_struct_gep(str_ty, b_ptr, 0, "db.p")
                .unwrap();
            let data_a = self
                .builder
                .build_load(ptr_ty, da_p, "da")
                .unwrap()
                .into_pointer_value();
            let data_b = self
                .builder
                .build_load(ptr_ty, db_p, "db")
                .unwrap()
                .into_pointer_value();

            let loop_hdr = self.context.append_basic_block(eq_fn, "eq.hdr");
            let loop_bdy = self.context.append_basic_block(eq_fn, "eq.bdy");
            let loop_exit = self.context.append_basic_block(eq_fn, "eq.exit");

            self.builder.build_unconditional_branch(loop_hdr).unwrap();

            self.builder.position_at_end(loop_hdr);
            let i_phi = self.builder.build_phi(i64_t, "eq.i").unwrap();
            i_phi.add_incoming(&[(&i64_t.const_zero(), bytes_bb)]);
            let i_val = i_phi.as_basic_value().into_int_value();
            let cond = self
                .builder
                .build_int_compare(IntPredicate::ULT, i_val, len_a, "eq.cond")
                .unwrap();
            self.builder
                .build_conditional_branch(cond, loop_bdy, loop_exit)
                .unwrap();

            self.builder.position_at_end(loop_bdy);
            let bpa = unsafe {
                self.builder
                    .build_gep(i8_t, data_a, &[i_val], "bpa")
                    .unwrap()
            };
            let bpb = unsafe {
                self.builder
                    .build_gep(i8_t, data_b, &[i_val], "bpb")
                    .unwrap()
            };
            let ba = self
                .builder
                .build_load(i8_t, bpa, "ba")
                .unwrap()
                .into_int_value();
            let bb_v = self
                .builder
                .build_load(i8_t, bpb, "bb")
                .unwrap()
                .into_int_value();
            let bytes_eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, ba, bb_v, "beq")
                .unwrap();
            let i_next = self
                .builder
                .build_int_add(i_val, i64_t.const_int(1, false), "eq.i1")
                .unwrap();
            i_phi.add_incoming(&[(&i_next, loop_bdy)]);
            self.builder
                .build_conditional_branch(bytes_eq, loop_hdr, neq_bb)
                .unwrap();

            self.builder.position_at_end(loop_exit);
            self.builder
                .build_return(Some(&bool_t.const_int(1, false)))
                .unwrap();
        } else if let BasicTypeEnum::IntType(int_ty) = key_ty {
            // Integer primitives: load and compare directly.
            let va = self
                .builder
                .build_load(int_ty, a_ptr, "va")
                .unwrap()
                .into_int_value();
            let vb = self
                .builder
                .build_load(int_ty, b_ptr, "vb")
                .unwrap()
                .into_int_value();
            let eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, va, vb, "eq")
                .unwrap();
            self.builder.build_return(Some(&eq)).unwrap();
        } else {
            // Structs and other fixed-size types: byte-by-byte comparison.
            let raw_size = key_ty
                .size_of()
                .unwrap_or_else(|| i64_t.const_int(8, false));
            let size64 = if raw_size.get_type().get_bit_width() == 64 {
                raw_size
            } else {
                self.builder
                    .build_int_z_extend(raw_size, i64_t, "ksz64")
                    .unwrap()
            };

            let neq_bb = self.context.append_basic_block(eq_fn, "neq");
            let loop_hdr = self.context.append_basic_block(eq_fn, "eq.hdr");
            let loop_bdy = self.context.append_basic_block(eq_fn, "eq.bdy");
            let loop_exit = self.context.append_basic_block(eq_fn, "eq.exit");

            self.builder.build_unconditional_branch(loop_hdr).unwrap();

            self.builder.position_at_end(neq_bb);
            self.builder
                .build_return(Some(&bool_t.const_int(0, false)))
                .unwrap();

            self.builder.position_at_end(loop_hdr);
            let i_phi = self.builder.build_phi(i64_t, "eq.i").unwrap();
            i_phi.add_incoming(&[(&i64_t.const_zero(), entry_bb)]);
            let i_val = i_phi.as_basic_value().into_int_value();
            let cond = self
                .builder
                .build_int_compare(IntPredicate::ULT, i_val, size64, "eq.cond")
                .unwrap();
            self.builder
                .build_conditional_branch(cond, loop_bdy, loop_exit)
                .unwrap();

            self.builder.position_at_end(loop_bdy);
            let bpa = unsafe {
                self.builder
                    .build_gep(i8_t, a_ptr, &[i_val], "bpa")
                    .unwrap()
            };
            let bpb = unsafe {
                self.builder
                    .build_gep(i8_t, b_ptr, &[i_val], "bpb")
                    .unwrap()
            };
            let ba = self
                .builder
                .build_load(i8_t, bpa, "ba")
                .unwrap()
                .into_int_value();
            let bb_v = self
                .builder
                .build_load(i8_t, bpb, "bb")
                .unwrap()
                .into_int_value();
            let bytes_eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, ba, bb_v, "beq")
                .unwrap();
            let i_next = self
                .builder
                .build_int_add(i_val, i64_t.const_int(1, false), "eq.i1")
                .unwrap();
            i_phi.add_incoming(&[(&i_next, loop_bdy)]);
            self.builder
                .build_conditional_branch(bytes_eq, loop_hdr, neq_bb)
                .unwrap();

            self.builder.position_at_end(loop_exit);
            self.builder
                .build_return(Some(&bool_t.const_int(1, false)))
                .unwrap();
        }

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        eq_fn
    }

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

    /// to the primitive `emit_hash_fn_for_type` path for everything else.
    ///
    /// Cache key is the mangled type name (`Self::mangled_type_name`), so a
    /// `(String, i32)` tuple key emits `karac_hash_tuple_String_i32` once per
    /// module and reuses it across all `Map[(String, i32), V]` / nested
    /// occurrences.
    pub(super) fn emit_hash_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::mangled_type_name(te);
        let fn_name = format!("karac_hash_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => {
                self.emit_hash_fn_for_tuple(&type_name, elems)
            }
            _ => {
                let key_ty = self.llvm_type_for_type_expr(te);
                self.emit_hash_fn_for_type(&type_name, key_ty)
            }
        }
    }

    /// TypeExpr-aware eq-fn wrapper. Mirror of `emit_hash_fn_for_type_expr`.
    pub(super) fn emit_eq_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::mangled_type_name(te);
        let fn_name = format!("karac_eq_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => {
                self.emit_eq_fn_for_tuple(&type_name, elems)
            }
            _ => {
                let key_ty = self.llvm_type_for_type_expr(te);
                self.emit_eq_fn_for_type(&type_name, key_ty)
            }
        }
    }

    /// Emit a per-field-recursive hash function for an n-tuple. Each field's
    /// hash is computed by recursing into `emit_hash_fn_for_type_expr` (so
    /// `(String, i64)` correctly hashes the String contents, not the struct
    /// bytes), then combined into a running state via the FxHash tail-mix
    /// `state = (state.rotate_left(5) ^ field_hash) * FXHASH_SEED`. Matches
    /// the per-K hash emission shape selected by the
    /// `bench/hash_quality/` investigation.
    pub(super) fn emit_hash_fn_for_tuple(
        &mut self,
        type_name: &str,
        elems: &[TypeExpr],
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_hash_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        let elems_owned: Vec<TypeExpr> = elems.to_vec();
        let child_fns: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_hash_fn_for_type_expr(e))
            .collect();
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let saved_bb = self.builder.get_insert_block();
        let hash_fn_ty = i64_t.fn_type(&[ptr_ty.into()], false);
        let hash_fn = self
            .module
            .add_function(&fn_name, hash_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(hash_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let key_ptr = hash_fn.get_nth_param(0).unwrap().into_pointer_value();

        // FxHash tail-mix: state = (state.rotate_left(5) ^
        // field_hash) * FXHASH_SEED. Initial state = 0 collapses
        // the first field's mix to `field_hash_0 * SEED`,
        // matching the inline primitive fast path for a 1-element
        // "tuple". For n>1 fields, subsequent fields rotate and
        // chain.
        let seed = i64_t.const_int(Self::FXHASH_SEED, false);
        let rotate_amt = i64_t.const_int(Self::FXHASH_ROTATE, false);
        let rotate_inv = i64_t.const_int(64 - Self::FXHASH_ROTATE, false);
        let mut state: IntValue<'ctx> = i64_t.const_zero();
        for (i, child_fn) in child_fns.iter().enumerate() {
            let field_ptr = self
                .builder
                .build_struct_gep(tuple_ty, key_ptr, i as u32, &format!("t.f{i}.p"))
                .unwrap();
            let elem_hash = self
                .builder
                .build_call(*child_fn, &[field_ptr.into()], &format!("t.f{i}.h"))
                .unwrap()
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value();
            let shl = self
                .builder
                .build_left_shift(state, rotate_amt, &format!("t.f{i}.shl"))
                .unwrap();
            let shr = self
                .builder
                .build_right_shift(state, rotate_inv, false, &format!("t.f{i}.shr"))
                .unwrap();
            let rotated = self
                .builder
                .build_or(shl, shr, &format!("t.f{i}.rot"))
                .unwrap();
            let xored = self
                .builder
                .build_xor(rotated, elem_hash, &format!("t.f{i}.xor"))
                .unwrap();
            state = self
                .builder
                .build_int_mul(xored, seed, &format!("t.f{i}.mul"))
                .unwrap();
        }
        self.builder.build_return(Some(&state)).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        hash_fn
    }

    /// Emit a per-field-recursive eq function for an n-tuple. Each field is
    /// compared via the recursively-emitted per-field eq fn; the function
    /// short-circuits to `false` on the first mismatch.
    pub(super) fn emit_eq_fn_for_tuple(
        &mut self,
        type_name: &str,
        elems: &[TypeExpr],
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_eq_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        let elems_owned: Vec<TypeExpr> = elems.to_vec();
        let child_fns: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_eq_fn_for_type_expr(e))
            .collect();
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let bool_t = self.context.bool_type();

        let saved_bb = self.builder.get_insert_block();
        let eq_fn_ty = bool_t.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let eq_fn = self
            .module
            .add_function(&fn_name, eq_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(eq_fn, "entry");
        let neq_bb = self.context.append_basic_block(eq_fn, "neq");
        self.builder.position_at_end(neq_bb);
        self.builder
            .build_return(Some(&bool_t.const_int(0, false)))
            .unwrap();

        self.builder.position_at_end(entry_bb);
        let a_ptr = eq_fn.get_nth_param(0).unwrap().into_pointer_value();
        let b_ptr = eq_fn.get_nth_param(1).unwrap().into_pointer_value();

        for (i, child_fn) in child_fns.iter().enumerate() {
            let fa = self
                .builder
                .build_struct_gep(tuple_ty, a_ptr, i as u32, &format!("t.fa{i}"))
                .unwrap();
            let fb = self
                .builder
                .build_struct_gep(tuple_ty, b_ptr, i as u32, &format!("t.fb{i}"))
                .unwrap();
            let r = self
                .builder
                .build_call(*child_fn, &[fa.into(), fb.into()], &format!("t.eq{i}"))
                .unwrap()
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value();
            let next_bb = self
                .context
                .append_basic_block(eq_fn, &format!("eq.next{i}"));
            self.builder
                .build_conditional_branch(r, next_bb, neq_bb)
                .unwrap();
            self.builder.position_at_end(next_bb);
        }
        self.builder
            .build_return(Some(&bool_t.const_int(1, false)))
            .unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        eq_fn
    }

    /// Emit (or reuse) a module-level Display function for the given type.
    ///
    /// Signature: `void karac_display_<type_name>(*const T)`. The function
    /// reads `*ptr` (or extracts struct fields, depending on the type) and
    /// writes a textual representation to stdout via `printf`. No trailing
    /// newline — callers append `\n` themselves for `println`.
    ///
    /// Subtask 1+2 scope: primitives (`i8`..`i64` / `u8`..`u64` / `f32`/`f64`
    /// / `bool` / `char` / `String`/`str`). Compound types (Vec/Map/Set/Tuple)
    /// land in subtasks 3-6, each as a new arm in this function that recurses
    /// into `emit_display_fn_for_type` for element/field types.
    ///
    /// Cache is keyed by the canonical `type_name` string — same convention
    /// used by `emit_hash_fn_for_type`. Caller is responsible for ensuring
    /// `type_name` uniquely identifies the type (for primitives this is
    /// trivial; for compound types the caller composes a mangled name).
    ///
    /// `dead_code` is allowed because subtasks 1+2 of the Display canonical
    /// bullet ship the machinery + primitive Display fns ahead of subtasks
    /// 3-7 which add the callers. Remove the allow when subtask 7 lands.
    #[allow(dead_code)]
    pub(super) fn emit_display_fn_for_type(
        &mut self,
        type_name: &str,
        ty: BasicTypeEnum<'ctx>,
    ) -> FunctionValue<'ctx> {
        if let Some(&f) = self.display_fn_cache.get(type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name.to_string(), f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i32_t = self.context.i32_type();
        let i64_t = self.context.i64_type();
        let f64_t = self.context.f64_type();

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache
            .insert(type_name.to_string(), display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        match type_name {
            "i8" | "i16" | "i32" | "i64" | "isize" => {
                // Sign-extend to i64, printf "%lld".
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let v64 = self.builder.build_int_s_extend(v, i64_t, "v64").unwrap();
                let fmt = self.builder.build_global_string_ptr("%lld", "fi").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v64.into()],
                        "p",
                    )
                    .unwrap();
            }
            "u8" | "u16" | "u32" | "u64" | "usize" => {
                // Zero-extend to i64, printf "%llu".
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let v64 = self.builder.build_int_z_extend(v, i64_t, "v64").unwrap();
                let fmt = self.builder.build_global_string_ptr("%llu", "fu").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v64.into()],
                        "p",
                    )
                    .unwrap();
            }
            "f32" => {
                // Widen to f64, printf "%g".
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_float_value();
                let v64 = self.builder.build_float_ext(v, f64_t, "v64").unwrap();
                let fmt = self.builder.build_global_string_ptr("%g", "ff").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v64.into()],
                        "p",
                    )
                    .unwrap();
            }
            "f64" => {
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_float_value();
                let fmt = self.builder.build_global_string_ptr("%g", "ff").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v.into()],
                        "p",
                    )
                    .unwrap();
            }
            "bool" => {
                // Select between "true" / "false" static strings.
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let true_s = self.builder.build_global_string_ptr("true", "ts").unwrap();
                let false_s = self.builder.build_global_string_ptr("false", "fs").unwrap();
                let sel = self
                    .builder
                    .build_select(
                        v,
                        true_s.as_pointer_value(),
                        false_s.as_pointer_value(),
                        "bsel",
                    )
                    .unwrap();
                let fmt = self.builder.build_global_string_ptr("%s", "fs").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), sel.into()],
                        "p",
                    )
                    .unwrap();
            }
            "char" => {
                // Char is a Unicode scalar (i32). For ASCII (the common case)
                // %c prints correctly. Non-ASCII codepoints get truncated to
                // i32 by printf — UTF-8 encoding refinement is a follow-up.
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let fmt = self.builder.build_global_string_ptr("%c", "fc").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v.into()],
                        "p",
                    )
                    .unwrap();
            }
            "String" | "str" => {
                // 24-byte struct {data, len, cap}. Use %.*s to bound by len —
                // String values are NOT NUL-terminated.
                let str_ty = self.vec_struct_type();
                let data_pp = self
                    .builder
                    .build_struct_gep(str_ty, val_ptr, 0, "s.data.pp")
                    .unwrap();
                let len_p = self
                    .builder
                    .build_struct_gep(str_ty, val_ptr, 1, "s.len.p")
                    .unwrap();
                let data = self
                    .builder
                    .build_load(ptr_ty, data_pp, "s.data")
                    .unwrap()
                    .into_pointer_value();
                let len = self
                    .builder
                    .build_load(i64_t, len_p, "s.len")
                    .unwrap()
                    .into_int_value();
                let len32 = self
                    .builder
                    .build_int_truncate(len, i32_t, "len32")
                    .unwrap();
                let fmt = self.builder.build_global_string_ptr("%.*s", "fs").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), len32.into(), data.into()],
                        "p",
                    )
                    .unwrap();
            }
            other if other.starts_with("Vec_") => {
                // Vec[T]'s element TypeExpr can't be unambiguously recovered
                // from the mangled cache name once nested compound shapes
                // (e.g. `Vec_tuple_i64_String`) are in play — string-splitting
                // on `_` is brittle. Callers should hold the element
                // `TypeExpr` and dispatch via `emit_display_fn_for_type_expr`,
                // which routes Vec to `emit_vec_display_fn_te(elem_te)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_vec_display_fn_te(elem_te) (or emit_display_fn_for_type_expr)"
                );
            }
            other if other.starts_with("Map_") => {
                // Map types have two type parameters and so cannot recover
                // (key_ty, val_ty) by string-splitting the cache key. Callers
                // that already hold K and V `TypeExpr`s should dispatch via
                // `emit_display_fn_for_type_expr`, which routes Map to
                // `emit_map_display_fn(key_te, val_te)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_map_display_fn(key_te, val_te) (or emit_display_fn_for_type_expr)"
                );
            }
            other if other.starts_with("Set_") => {
                // Set's element TypeExpr can't be unambiguously recovered
                // from a mangled cache name once nested compound shapes are
                // in play. Callers should hold the element `TypeExpr` and
                // dispatch via `emit_display_fn_for_type_expr`, which
                // routes Set to `emit_set_display_fn(elem_te)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_set_display_fn(elem_te) (or emit_display_fn_for_type_expr)"
                );
            }
            other if other.starts_with("tuple_") => {
                // n-tuples cannot recover their per-field TypeExprs from the
                // mangled name alone. Callers that already hold the field
                // `TypeExpr`s should dispatch via
                // `emit_display_fn_for_type_expr`, which routes Tuple to
                // `emit_tuple_display_fn(elems)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_tuple_display_fn(elems) (or emit_display_fn_for_type_expr)"
                );
            }
            other => {
                // Set_*, user structs not yet supported.
                // Subtask 5 of the Display canonical bullet
                // (phase-7-codegen.md § Phase 7.2) extends this match for Set.
                panic!("emit_display_fn_for_type: type_name '{other}' not yet supported");
            }
        }

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Emit the body of a `Vec[T]` Display function. Reads `data`/`len` from
    /// the 24-byte Vec struct at `val_ptr`, prints `[`, walks elements with
    /// `, ` separators recursing into the element Display fn, prints `]`.
    ///
    /// `elem_te` describes the element type. Recursion into the per-element
    /// Display fn goes through the TypeExpr-aware dispatcher
    /// (`emit_display_fn_for_type_expr`) so compound elements (`Vec[Vec[T]]`,
    /// `Vec[(i64, String)]`, `Vec[Map[K, V]]`) compose correctly without the
    /// by-name path having to recover `TypeExpr`s from a mangled string.
    ///
    /// Caller is expected to have positioned the builder at the entry block
    /// of `display_fn` and to emit the trailing `ret void` after this returns.
    pub(super) fn emit_vec_display_body(
        &mut self,
        display_fn: FunctionValue<'ctx>,
        val_ptr: PointerValue<'ctx>,
        elem_te: &TypeExpr,
    ) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i8_t = self.context.i8_type();
        let i64_t = self.context.i64_type();
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);

        // Materialize (or fetch) the element Display fn first — the recursive
        // emit may switch the builder's insert block, so do it before the
        // remaining body emission positions us at `display_fn`'s entry. The
        // dispatcher saves/restores so the caller's position is preserved.
        let elem_disp = self.emit_display_fn_for_type_expr(elem_te);

        // Print "[".
        let lb = self.builder.build_global_string_ptr("[", "vd.lb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[lb.as_pointer_value().into()], "p")
            .unwrap();

        // Load data (i8*) and len (i64) from the Vec struct.
        let data_pp = self
            .builder
            .build_struct_gep(vec_ty, val_ptr, 0, "v.data.pp")
            .unwrap();
        let len_p = self
            .builder
            .build_struct_gep(vec_ty, val_ptr, 1, "v.len.p")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "v.data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_p, "v.len")
            .unwrap()
            .into_int_value();

        // Element size in bytes — drives the GEP stride.
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

        // Loop: i in 0..len, with ", " separator before every elem after first.
        let pre_bb = self.builder.get_insert_block().unwrap();
        let hdr_bb = self.context.append_basic_block(display_fn, "vec.hdr");
        let bdy_bb = self.context.append_basic_block(display_fn, "vec.bdy");
        let sep_bb = self.context.append_basic_block(display_fn, "vec.sep");
        let elem_bb = self.context.append_basic_block(display_fn, "vec.elem");
        let exit_bb = self.context.append_basic_block(display_fn, "vec.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let i_phi = self.builder.build_phi(i64_t, "vec.i").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), pre_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, i_val, len, "vec.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, bdy_bb, exit_bb)
            .unwrap();

        // bdy: branch to sep if i > 0, else straight to elem.
        self.builder.position_at_end(bdy_bb);
        let is_first = self
            .builder
            .build_int_compare(IntPredicate::EQ, i_val, i64_t.const_zero(), "is.first")
            .unwrap();
        self.builder
            .build_conditional_branch(is_first, elem_bb, sep_bb)
            .unwrap();

        // sep: print ", ", then fall to elem.
        self.builder.position_at_end(sep_bb);
        let sep = self
            .builder
            .build_global_string_ptr(", ", "vd.sep")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
            .unwrap();
        self.builder.build_unconditional_branch(elem_bb).unwrap();

        // elem: GEP to data + i * elem_size, call element Display fn.
        self.builder.position_at_end(elem_bb);
        let offset = self.builder.build_int_mul(i_val, elem_size, "off").unwrap();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(i8_t, data, &[offset], "elem.p")
                .unwrap()
        };
        self.builder
            .build_call(elem_disp, &[elem_ptr.into()], "ed")
            .unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "vec.i1")
            .unwrap();
        i_phi.add_incoming(&[(&i_next, elem_bb)]);
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        // exit: print "]".
        self.builder.position_at_end(exit_bb);
        let rb = self.builder.build_global_string_ptr("]", "vd.rb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rb.as_pointer_value().into()], "p")
            .unwrap();
    }

    /// Emit (or reuse) a Display function for `Map[K, V]`. Typed entry point —
    /// distinct from `emit_display_fn_for_type` because Map's two type
    /// parameters can't be recovered from a single mangled name string.
    ///
    /// The emitted function is named `karac_display_Map_<key>_<val>` (deeply
    /// mangled via `display_mangle_te`) and is shared with the generic Display
    /// cache under the same key, so a later `emit_display_fn_for_type` cache
    /// hit returns the same function (the catch-all `Map_*` arm panics on
    /// cache miss to steer callers here).
    ///
    /// Calling convention: `void karac_display_Map_K_V(ptr slot)` where `slot`
    /// is the address of a slot holding the opaque map handle (matches the
    /// shape produced by `compile_map_new_stmt`). Body loads the handle,
    /// drives `karac_map_iter_*` (mirroring `compile_for_map_var`),
    /// per-iteration recurses into `emit_display_fn_for_type_expr` for K and
    /// V (so `Map[(i64, String), Vec[bool]]` etc. compose correctly), and
    /// frees the iterator before returning. Iteration order is unspecified
    /// per `design.md` line 1588 — tests must not assert order.
    pub(super) fn emit_map_display_fn(
        &mut self,
        key_te: &TypeExpr,
        val_te: &TypeExpr,
    ) -> FunctionValue<'ctx> {
        let key_name = Self::display_mangle_te(key_te);
        let val_name = Self::display_mangle_te(val_te);
        let type_name = format!("Map_{key_name}_{val_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let slot_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        self.emit_map_display_body(display_fn, slot_ptr, key_te, val_te);

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Emit the body of a `Map[K, V]` Display function. Loads the map handle
    /// from `slot_ptr`, prints `"{"`, drives `karac_map_iter_new` /
    /// `karac_map_iter_next` to walk pairs, per-iteration recurses via
    /// `emit_display_fn_for_type_expr` for K and V with `": "` between
    /// key/value and `", "` between pairs, frees the iterator in the exit
    /// block, and prints `"}"`.
    ///
    /// `is_first` flag is tracked via an i1 alloca because the iterator-driven
    /// loop has no scalar counter (unlike Vec where `i == 0` works).
    ///
    /// Caller positions the builder at `display_fn`'s entry block and is
    /// responsible for emitting the trailing `ret void`.
    pub(super) fn emit_map_display_body(
        &mut self,
        display_fn: FunctionValue<'ctx>,
        slot_ptr: PointerValue<'ctx>,
        key_te: &TypeExpr,
        val_te: &TypeExpr,
    ) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let bool_t = self.context.bool_type();
        let key_ty = self.llvm_type_for_type_expr(key_te);
        let val_ty = self.llvm_type_for_type_expr(val_te);

        // Print "{".
        let lb = self.builder.build_global_string_ptr("{", "md.lb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[lb.as_pointer_value().into()], "p")
            .unwrap();

        // Load the opaque map handle from slot_ptr.
        let map_handle = self
            .builder
            .build_load(ptr_ty, slot_ptr, "md.handle")
            .unwrap()
            .into_pointer_value();

        // Allocas for the loop's iterator handle, the is_first flag, and the
        // out_key / out_val staging slots. Place them in the entry block via
        // `create_entry_alloca` so they dominate the loop.
        let iter_slot = self.create_entry_alloca(display_fn, "md.iter.slot", ptr_ty.into());
        let first_slot = self.create_entry_alloca(display_fn, "md.first", bool_t.into());
        let out_key = self.create_entry_alloca(display_fn, "md.out_key", key_ty);
        let out_val = self.create_entry_alloca(display_fn, "md.out_val", val_ty);

        // Initialize iter, is_first.
        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[map_handle.into()], "md.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        self.builder.build_store(iter_slot, iter_ptr).unwrap();
        self.builder
            .build_store(first_slot, bool_t.const_int(1, false))
            .unwrap();

        // Materialize (or fetch) the per-key and per-value Display fns.
        let key_disp = self.emit_display_fn_for_type_expr(key_te);
        let val_disp = self.emit_display_fn_for_type_expr(val_te);

        let hdr_bb = self.context.append_basic_block(display_fn, "map.hdr");
        let bdy_bb = self.context.append_basic_block(display_fn, "map.bdy");
        let sep_bb = self.context.append_basic_block(display_fn, "map.sep");
        let pair_bb = self.context.append_basic_block(display_fn, "map.pair");
        let exit_bb = self.context.append_basic_block(display_fn, "map.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        // hdr: advance iterator; loop while it returns true.
        self.builder.position_at_end(hdr_bb);
        let iter_cur = self
            .builder
            .build_load(ptr_ty, iter_slot, "md.iter.cur")
            .unwrap()
            .into_pointer_value();
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_cur.into(), out_key.into(), out_val.into()],
                "md.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, bdy_bb, exit_bb)
            .unwrap();

        // bdy: branch on is_first — first iteration skips the ", " separator
        // and clears the flag; subsequent iterations print ", " first.
        self.builder.position_at_end(bdy_bb);
        let f = self
            .builder
            .build_load(bool_t, first_slot, "md.f")
            .unwrap()
            .into_int_value();
        self.builder
            .build_conditional_branch(f, pair_bb, sep_bb)
            .unwrap();

        // sep: print ", " then fall through to pair.
        self.builder.position_at_end(sep_bb);
        let sep = self
            .builder
            .build_global_string_ptr(", ", "md.sep")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
            .unwrap();
        self.builder.build_unconditional_branch(pair_bb).unwrap();

        // pair: clear is_first (idempotent on second+ iters), print key, ": ",
        // value, then loop back to hdr.
        self.builder.position_at_end(pair_bb);
        self.builder
            .build_store(first_slot, bool_t.const_int(0, false))
            .unwrap();
        self.builder
            .build_call(key_disp, &[out_key.into()], "md.kd")
            .unwrap();
        let colon = self
            .builder
            .build_global_string_ptr(": ", "md.col")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[colon.as_pointer_value().into()], "p")
            .unwrap();
        self.builder
            .build_call(val_disp, &[out_val.into()], "md.vd")
            .unwrap();
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        // exit: free iterator, print "}".
        self.builder.position_at_end(exit_bb);
        let iter_final = self
            .builder
            .build_load(ptr_ty, iter_slot, "md.iter.final")
            .unwrap()
            .into_pointer_value();
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_final.into()], "")
            .unwrap();
        let rb = self.builder.build_global_string_ptr("}", "md.rb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rb.as_pointer_value().into()], "p")
            .unwrap();
    }

    /// Emit (or reuse) a Display function for `Set[T]`. Typed entry point —
    /// shape mirrors `emit_map_display_fn` minus the value-side Display
    /// (Set lowers to `Map[T, ()]`; the iterator's value out-slot is sized
    /// 0 and the contents are discarded).
    ///
    /// The emitted function is named `karac_display_Set_<elem>` (deeply
    /// mangled via `display_mangle_te`) and shares the generic Display
    /// cache. Format `Set{a, b, c}` with the literal `Set` prefix matches
    /// the interpreter at `src/interpreter.rs:292`. Iteration order is
    /// unspecified per `design.md` line 1588 — tests must not assert order.
    pub(super) fn emit_set_display_fn(&mut self, elem_te: &TypeExpr) -> FunctionValue<'ctx> {
        let elem_name = Self::display_mangle_te(elem_te);
        let type_name = format!("Set_{elem_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let slot_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        self.emit_set_display_body(display_fn, slot_ptr, elem_te);

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Body of the Set Display fn. Loads the opaque map handle (Set lowers
    /// to `Map[T, ()]`), prints `Set{`, walks `karac_map_iter_*` printing
    /// each element via the per-type Display fn with `, ` between, frees
    /// the iterator, prints `}`. The val out-slot is sized 0 — a single
    /// shared `i8` alloca — and its contents are discarded.
    pub(super) fn emit_set_display_body(
        &mut self,
        display_fn: FunctionValue<'ctx>,
        slot_ptr: PointerValue<'ctx>,
        elem_te: &TypeExpr,
    ) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let bool_t = self.context.bool_type();
        let i8_t = self.context.i8_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);

        // Print "Set{" — literal prefix matches the interpreter format at
        // `src/interpreter.rs:292`.
        let lb = self
            .builder
            .build_global_string_ptr("Set{", "sd.lb")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[lb.as_pointer_value().into()], "p")
            .unwrap();

        // Load the opaque set/map handle from slot_ptr.
        let set_handle = self
            .builder
            .build_load(ptr_ty, slot_ptr, "sd.handle")
            .unwrap()
            .into_pointer_value();

        let iter_slot = self.create_entry_alloca(display_fn, "sd.iter.slot", ptr_ty.into());
        let first_slot = self.create_entry_alloca(display_fn, "sd.first", bool_t.into());
        let out_elem = self.create_entry_alloca(display_fn, "sd.out_elem", elem_ty);
        // val_size = 0 — a single shared i8 alloca for the discarded
        // value out-slot. Runtime stores zero bytes regardless.
        let dummy_val = self.create_entry_alloca(display_fn, "sd.dummy", i8_t.into());

        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[set_handle.into()], "sd.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        self.builder.build_store(iter_slot, iter_ptr).unwrap();
        self.builder
            .build_store(first_slot, bool_t.const_int(1, false))
            .unwrap();

        let elem_disp = self.emit_display_fn_for_type_expr(elem_te);

        let hdr_bb = self.context.append_basic_block(display_fn, "set.hdr");
        let bdy_bb = self.context.append_basic_block(display_fn, "set.bdy");
        let sep_bb = self.context.append_basic_block(display_fn, "set.sep");
        let elem_bb = self.context.append_basic_block(display_fn, "set.elem");
        let exit_bb = self.context.append_basic_block(display_fn, "set.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let iter_cur = self
            .builder
            .build_load(ptr_ty, iter_slot, "sd.iter.cur")
            .unwrap()
            .into_pointer_value();
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_cur.into(), out_elem.into(), dummy_val.into()],
                "sd.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        let f = self
            .builder
            .build_load(bool_t, first_slot, "sd.f")
            .unwrap()
            .into_int_value();
        self.builder
            .build_conditional_branch(f, elem_bb, sep_bb)
            .unwrap();

        self.builder.position_at_end(sep_bb);
        let sep = self
            .builder
            .build_global_string_ptr(", ", "sd.sep")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
            .unwrap();
        self.builder.build_unconditional_branch(elem_bb).unwrap();

        self.builder.position_at_end(elem_bb);
        self.builder
            .build_store(first_slot, bool_t.const_int(0, false))
            .unwrap();
        self.builder
            .build_call(elem_disp, &[out_elem.into()], "sd.ed")
            .unwrap();
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        let iter_final = self
            .builder
            .build_load(ptr_ty, iter_slot, "sd.iter.final")
            .unwrap()
            .into_pointer_value();
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_final.into()], "")
            .unwrap();
        let rb = self.builder.build_global_string_ptr("}", "sd.rb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rb.as_pointer_value().into()], "p")
            .unwrap();
    }

    /// Deeply mangled type name suitable for Display cache keys. Unlike
    /// `mangled_type_name` (which is shallow on `Path` types — used for
    /// hash/eq, where `Map[Vec[T], V]` is unreachable so deep mangling is
    /// unnecessary), this walks generic args so `Vec[i64]` → `Vec_i64`,
    /// `Map[String, i64]` → `Map_String_i64`, and nested shapes compose.
    /// Tuples use the same `tuple_T1_T2_...` form `mangled_type_name`
    /// produces — the recursive shapes match.
    pub(super) fn display_mangle_te(te: &TypeExpr) -> String {
        match &te.kind {
            TypeKind::Tuple(elems) if elems.is_empty() => "unit".to_string(),
            TypeKind::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(Self::display_mangle_te).collect();
                format!("tuple_{}", parts.join("_"))
            }
            TypeKind::Path(p) => {
                let head = p
                    .segments
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                if let Some(args) = p.generic_args.as_ref() {
                    let parts: Vec<String> = args
                        .iter()
                        .filter_map(|a| match a {
                            GenericArg::Type(t) => Some(Self::display_mangle_te(t)),
                            _ => None,
                        })
                        .collect();
                    if !parts.is_empty() {
                        return format!("{head}_{}", parts.join("_"));
                    }
                }
                head
            }
            _ => "unknown".to_string(),
        }
    }

    /// TypeExpr-aware Display dispatcher. Canonical entry point for any
    /// caller that holds a source-level `TypeExpr`: routes by shape to the
    /// typed Vec / Map / Tuple entry points, and falls through to the
    /// by-name `emit_display_fn_for_type` for primitives. Mirror of
    /// `emit_hash_fn_for_type_expr` / `emit_eq_fn_for_type_expr`.
    ///
    /// Cache-key check up front so the dispatcher itself is cheap on repeat
    /// calls — every typed entry point (`emit_*_display_fn_te` /
    /// `emit_tuple_display_fn`) also re-checks before emitting, but doing it
    /// here avoids the per-shape branching cost when the function already
    /// exists.
    pub(super) fn emit_display_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::display_mangle_te(te);
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => self.emit_tuple_display_fn(elems),
            TypeKind::Path(p) => {
                let head = p.segments.first().map(String::as_str);
                if head == Some("Vec") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        return self.emit_vec_display_fn_te(&elem_te);
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
                        return self.emit_map_display_fn(&k, &v);
                    }
                }
                if head == Some("Set") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        return self.emit_set_display_fn(&elem_te);
                    }
                }
                // Primitive (or unsupported path) — fall through to by-name.
                let llvm_ty = self.llvm_type_for_type_expr(te);
                self.emit_display_fn_for_type(&type_name, llvm_ty)
            }
            _ => {
                let llvm_ty = self.llvm_type_for_type_expr(te);
                self.emit_display_fn_for_type(&type_name, llvm_ty)
            }
        }
    }

    /// Emit (or reuse) a typed Display function for `Vec[T]`. The function
    /// is named `karac_display_Vec_<elem_mangled>` and shares the generic
    /// `display_fn_cache` keyed on the same mangled name; the catch-all
    /// `Vec_*` arm in `emit_display_fn_for_type` panics on cache miss to
    /// steer callers here. Body delegates to `emit_vec_display_body` which
    /// recurses via `emit_display_fn_for_type_expr` for the element type.
    pub(super) fn emit_vec_display_fn_te(&mut self, elem_te: &TypeExpr) -> FunctionValue<'ctx> {
        let elem_name = Self::display_mangle_te(elem_te);
        let type_name = format!("Vec_{elem_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        self.emit_vec_display_body(display_fn, val_ptr, elem_te);

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Emit (or reuse) a typed Display function for an n-tuple
    /// `(T1, T2, …, Tn)`. Typed entry point — distinct from the by-name
    /// `emit_display_fn_for_type` because per-field `TypeExpr`s can't be
    /// recovered from a single mangled name string once nested compound
    /// shapes (`((i64, i64), String)`) are in play. Mirror of the
    /// `emit_map_display_fn` pattern.
    ///
    /// Cache key (and function name suffix) is the deeply-mangled name —
    /// `tuple_T1_T2_..._Tn`. Shares the generic `display_fn_cache` so a
    /// later `emit_display_fn_for_type` cache hit on the same name returns
    /// this function (the catch-all `tuple_*` arm panics on cache miss to
    /// steer callers here).
    ///
    /// Calling convention: `void karac_display_tuple_T1_T2_..._Tn(ptr p)`
    /// where `p` points to the LLVM tuple struct value (one alloca'd or
    /// in-struct field address). Body reads each field via `getelementptr`
    /// on the tuple's LLVM struct type, recurses via
    /// `emit_display_fn_for_type_expr` for each field, and prints
    /// `(field0, field1, ...)` with `, ` between fields. Format matches
    /// the interpreter's tuple Display at `src/interpreter.rs:215`.
    pub(super) fn emit_tuple_display_fn(&mut self, elems: &[TypeExpr]) -> FunctionValue<'ctx> {
        // Cache lookup. Compute the canonical name first so module + cache
        // checks share one key.
        let parts: Vec<String> = elems.iter().map(Self::display_mangle_te).collect();
        let type_name = format!("tuple_{}", parts.join("_"));
        let fn_name = format!("karac_display_{type_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let elems_owned: Vec<TypeExpr> = elems.to_vec();

        // Materialize per-field Display fns first. Each recursive emit
        // saves and restores the builder position, so calling them before
        // we open this function's body is safe — the alternative (calling
        // mid-emission) would require careful position management.
        let field_disps: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_display_fn_for_type_expr(e))
            .collect();

        // Compute the tuple's LLVM struct type. Must match exactly what
        // `llvm_type_for_type_expr(Tuple(...))` produces so callers can pass
        // their tuple value's address directly to this function.
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();
        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        // Print "(".
        let lp = self.builder.build_global_string_ptr("(", "td.lp").unwrap();
        self.builder
            .build_call(self.printf_fn, &[lp.as_pointer_value().into()], "p")
            .unwrap();

        for (i, fd) in field_disps.iter().enumerate() {
            if i > 0 {
                let sep = self
                    .builder
                    .build_global_string_ptr(", ", "td.sep")
                    .unwrap();
                self.builder
                    .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
                    .unwrap();
            }
            let field_ptr = self
                .builder
                .build_struct_gep(tuple_ty, val_ptr, i as u32, &format!("t.f{i}.p"))
                .unwrap();
            self.builder
                .build_call(*fd, &[field_ptr.into()], &format!("t.f{i}.d"))
                .unwrap();
        }

        // Print ")".
        let rp = self.builder.build_global_string_ptr(")", "td.rp").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rp.as_pointer_value().into()], "p")
            .unwrap();

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        display_fn
    }
}
