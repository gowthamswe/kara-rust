//! Slice / array pattern lowering (phase-5 § Slice and array patterns
//! sub-item 4).
//!
//! Houses the four-helper cluster that powers slice-pattern arms in
//! `compile_match`:
//!
//! - `resolve_slice_source` — resolve a slice-pattern scrutinee to
//!   a uniform `SliceSource` (data pointer + runtime length +
//!   element type), unifying Array / Slice / Vec source shapes.
//! - `load_slice_pattern_element` — element-position load (handles
//!   wide-pointer Slice / Vec sources + the small-form Array source).
//! - `compile_slice_pattern_condition` — per-arm slice-shape +
//!   element-match condition lowering.
//! - `bind_slice_pattern` — push bindings introduced by the slice
//!   pattern (`prefix` / `rest` / `suffix` plus elements).
//!
//! Lives in a sibling `impl<'ctx> super::Codegen<'ctx>` block.

use crate::ast::*;

use inkwell::types::BasicTypeEnum;
use inkwell::values::{BasicValueEnum, IntValue};
use inkwell::AddressSpace;
use inkwell::IntPredicate;

use super::state::{SliceSource, VarSlot};

impl<'ctx> super::Codegen<'ctx> {
    // ── Slice / array patterns (phase-5 § Slice and array patterns sub-item 4)

    /// Resolve a slice-pattern scrutinee expression to a uniform
    /// `SliceSource` — a `T*` data pointer + runtime length + element
    /// type. Handles three identifier-rooted source shapes:
    ///   - `Array[T, N]` (alloca of `[N x T]`) → GEP to elem 0 + const length
    ///   - `Slice[T]` / `mut Slice[T]` (alloca of `{ptr, i64}`) → load data + len
    ///   - `Vec[T]` (alloca of `{ptr, i64, i64}`) → load data + len
    ///
    /// Returns `None` for non-identifier scrutinees or untracked variables —
    /// the typechecker rejects slice patterns against non-sequence
    /// scrutinees, so this is a defensive fallback.
    pub(super) fn resolve_slice_source(&mut self, expr: &Expr) -> Option<SliceSource<'ctx>> {
        let ExprKind::Identifier(name) = &expr.kind else {
            return None;
        };
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        // Array source — alloca holds a [N x T] aggregate. Element pointer is
        // the alloca itself viewed as T* (GEP [0, 0]); length is the static N.
        if let Some(slot) = self.variables.get(name.as_str()).copied() {
            if let BasicTypeEnum::ArrayType(at) = slot.ty {
                let zero = i64_t.const_int(0, false);
                let data_ptr = unsafe {
                    self.builder
                        .build_gep(slot.ty, slot.ptr, &[zero, zero], "sp.ar.data")
                        .unwrap()
                };
                return Some(SliceSource {
                    data_ptr,
                    len: i64_t.const_int(at.len() as u64, false),
                    elem_ty: at.get_element_type(),
                    mutable: false,
                });
            }
            // Slice[T] source.
            if let Some(&elem_ty) = self.slice_elem_types.get(name.as_str()) {
                let slice_ty = self.slice_struct_type();
                let data_pp = self
                    .builder
                    .build_struct_gep(slice_ty, slot.ptr, 0, "sp.sl.dpp")
                    .unwrap();
                let data_ptr = self
                    .builder
                    .build_load(ptr_ty, data_pp, "sp.sl.data")
                    .unwrap()
                    .into_pointer_value();
                let len_p = self
                    .builder
                    .build_struct_gep(slice_ty, slot.ptr, 1, "sp.sl.lp")
                    .unwrap();
                let len = self
                    .builder
                    .build_load(i64_t, len_p, "sp.sl.len")
                    .unwrap()
                    .into_int_value();
                return Some(SliceSource {
                    data_ptr,
                    len,
                    elem_ty,
                    mutable: false,
                });
            }
            // Vec[T] source.
            if let Some(&elem_ty) = self.vec_elem_types.get(name.as_str()) {
                let vec_ty = self.vec_struct_type();
                let data_pp = self
                    .builder
                    .build_struct_gep(vec_ty, slot.ptr, 0, "sp.v.dpp")
                    .unwrap();
                let data_ptr = self
                    .builder
                    .build_load(ptr_ty, data_pp, "sp.v.data")
                    .unwrap()
                    .into_pointer_value();
                let len_p = self
                    .builder
                    .build_struct_gep(vec_ty, slot.ptr, 1, "sp.v.lp")
                    .unwrap();
                let len = self
                    .builder
                    .build_load(i64_t, len_p, "sp.v.len")
                    .unwrap()
                    .into_int_value();
                return Some(SliceSource {
                    data_ptr,
                    len,
                    elem_ty,
                    mutable: false,
                });
            }
        }
        None
    }

    /// Load element `T` at `idx` from a slice source — GEP with the element
    /// type then load.
    pub(super) fn load_slice_pattern_element(
        &self,
        src: &SliceSource<'ctx>,
        idx: IntValue<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        let elem_ptr = unsafe {
            self.builder
                .build_gep(src.elem_ty, src.data_ptr, &[idx], "sp.elem.ptr")
                .unwrap()
        };
        self.builder
            .build_load(src.elem_ty, elem_ptr, "sp.elem")
            .unwrap()
    }

    /// Compile the i1 condition for `[prefix..., rest?, suffix...]` against
    /// a `SliceSource`. The length check fires first; sub-pattern checks
    /// run only when the length passes (guarded via a "check_elems" block
    /// so OOB GEPs don't emit when the length is wrong). Returns a phi-ed
    /// i1 that is false on length-mismatch and the AND of sub-pattern
    /// conditions otherwise.
    pub(super) fn compile_slice_pattern_condition(
        &mut self,
        prefix: &[Pattern],
        rest: &Option<RestPattern>,
        suffix: &[Pattern],
        src: &SliceSource<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_t = self.context.i64_type();
        let bool_t = self.context.bool_type();
        let fn_val = self.current_fn.unwrap();

        let min_len = i64_t.const_int((prefix.len() + suffix.len()) as u64, false);
        let len_ok = if rest.is_none() {
            self.builder
                .build_int_compare(IntPredicate::EQ, src.len, min_len, "sp.len.eq")
                .unwrap()
        } else {
            self.builder
                .build_int_compare(IntPredicate::UGE, src.len, min_len, "sp.len.ge")
                .unwrap()
        };

        // Fast path when there are no sub-patterns to check: condition is
        // just the length test.
        if prefix.is_empty() && suffix.is_empty() {
            return Ok(len_ok.into());
        }

        let check_bb = self.context.append_basic_block(fn_val, "sp.check");
        let done_bb = self.context.append_basic_block(fn_val, "sp.done");
        let len_fail_bb = self.builder.get_insert_block().unwrap();
        self.builder
            .build_conditional_branch(len_ok, check_bb, done_bb)
            .unwrap();

        self.builder.position_at_end(check_bb);
        let mut cond: IntValue<'ctx> = bool_t.const_int(1, false);
        for (i, sub) in prefix.iter().enumerate() {
            let idx = i64_t.const_int(i as u64, false);
            let elem = self.load_slice_pattern_element(src, idx);
            let sub_cond = self.compile_pattern_condition(sub, elem)?.into_int_value();
            cond = self.builder.build_and(cond, sub_cond, "sp.and").unwrap();
        }
        for (i, sub) in suffix.iter().enumerate() {
            let back_off = i64_t.const_int((suffix.len() - i) as u64, false);
            let idx = self
                .builder
                .build_int_sub(src.len, back_off, "sp.suf.idx")
                .unwrap();
            let elem = self.load_slice_pattern_element(src, idx);
            let sub_cond = self.compile_pattern_condition(sub, elem)?.into_int_value();
            cond = self.builder.build_and(cond, sub_cond, "sp.and").unwrap();
        }
        let check_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(done_bb).unwrap();

        self.builder.position_at_end(done_bb);
        let phi = self.builder.build_phi(bool_t, "sp.cond").unwrap();
        let len_false = bool_t.const_int(0, false);
        phi.add_incoming(&[(&len_false, len_fail_bb), (&cond, check_end)]);
        Ok(phi.as_basic_value())
    }

    /// Bind sub-patterns of a slice pattern against the source. Prefix
    /// elements bind at `data_ptr[i]`, suffix at `data_ptr[len-j+i]`,
    /// and `RestPattern::Bound(name)` materializes a `Slice[T]` view
    /// over `data_ptr[k..len-j]` registered under `name` so user code
    /// can dispatch slice methods (`rest.len()`, `rest[0]`, etc.).
    /// `for_match` toggles the sub-pattern binder between the match-arm
    /// helper (`bind_pattern_values`) and the let helper (`bind_pattern`),
    /// matching the two call sites' surrounding semantics.
    pub(super) fn bind_slice_pattern(
        &mut self,
        prefix: &[Pattern],
        rest: &Option<RestPattern>,
        suffix: &[Pattern],
        src: &SliceSource<'ctx>,
        for_match: bool,
    ) -> Result<(), String> {
        let i64_t = self.context.i64_type();

        for (i, sub) in prefix.iter().enumerate() {
            let idx = i64_t.const_int(i as u64, false);
            let elem = self.load_slice_pattern_element(src, idx);
            if for_match {
                self.bind_pattern_values(sub, elem)?;
            } else {
                self.bind_pattern(sub, elem)?;
            }
        }
        for (i, sub) in suffix.iter().enumerate() {
            let back_off = i64_t.const_int((suffix.len() - i) as u64, false);
            let idx = self
                .builder
                .build_int_sub(src.len, back_off, "sp.suf.bind.idx")
                .unwrap();
            let elem = self.load_slice_pattern_element(src, idx);
            if for_match {
                self.bind_pattern_values(sub, elem)?;
            } else {
                self.bind_pattern(sub, elem)?;
            }
        }

        if let Some(RestPattern::Bound(name)) = rest {
            let fn_val = self.current_fn.unwrap();
            let slice_ty = self.slice_struct_type();
            let prefix_off = i64_t.const_int(prefix.len() as u64, false);
            let suffix_len = i64_t.const_int(suffix.len() as u64, false);
            let rest_data_ptr = unsafe {
                self.builder
                    .build_gep(src.elem_ty, src.data_ptr, &[prefix_off], "sp.rest.dp")
                    .unwrap()
            };
            let after_prefix = self
                .builder
                .build_int_sub(src.len, prefix_off, "sp.rest.lp1")
                .unwrap();
            let rest_len = self
                .builder
                .build_int_sub(after_prefix, suffix_len, "sp.rest.len")
                .unwrap();
            let slice_val = self.build_slice_header(slice_ty, rest_data_ptr, rest_len);
            let alloca = self.create_entry_alloca(fn_val, name, slice_ty.into());
            self.builder.build_store(alloca, slice_val).unwrap();
            self.variables.insert(
                name.clone(),
                VarSlot {
                    ptr: alloca,
                    ty: slice_ty.into(),
                },
            );
            self.slice_elem_types.insert(name.clone(), src.elem_ty);
        }
        // `mutable` is a typechecker-level concept — codegen layout is
        // identical for read-only and mut slices; ownership tracking is
        // handled separately.
        let _ = src.mutable;
        Ok(())
    }
}
