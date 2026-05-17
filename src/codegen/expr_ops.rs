//! Expression-operator codegen: tuples, field access/store, casts,
//! binops, unaryops, and slice coercion.
//!
//! Houses tuple construction (`compile_tuple`, `compile_tuple_index`),
//! field access/store (`compile_field_access`, `compile_field_store`,
//! `field_index_for`, `type_name_of_expr`, `type_name_of`),
//! `compile_cast`, short-circuit / binary / struct-eq / string-binop /
//! float-binop emission (`compile_short_circuit`, `compile_binop`,
//! `compile_struct_eq`, `compile_string_binop`, `compile_float_binop`,
//! `to_float`), `compile_unaryop`, and slice coercion + range slicing
//! (`coerce_to_slice`, `build_slice_header`, `compile_range_slice`).

use crate::ast::*;

use inkwell::types::{BasicTypeEnum, StructType};
use inkwell::values::{BasicValueEnum, PointerValue};
use inkwell::AddressSpace;
use inkwell::{FloatPredicate, IntPredicate};

impl<'ctx> super::Codegen<'ctx> {
    pub(super) fn compile_tuple(&mut self, elems: &[Expr]) -> Result<BasicValueEnum<'ctx>, String> {
        let vals: Vec<BasicValueEnum<'ctx>> = elems
            .iter()
            .map(|e| self.compile_expr(e))
            .collect::<Result<_, _>>()?;
        let types: Vec<BasicTypeEnum<'ctx>> = vals.iter().map(|v| v.get_type()).collect();
        let st = self.context.struct_type(&types, false);
        let mut agg = st.get_undef();
        for (idx, val) in vals.iter().enumerate() {
            agg = self
                .builder
                .build_insert_value(agg, *val, idx as u32, "elem")
                .unwrap()
                .into_struct_value();
        }
        Ok(agg.into())
    }

    pub(super) fn compile_field_access(
        &mut self,
        object: &Expr,
        field: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Primitive-type associated constants — `i64.MAX` /
        // `f64.INFINITY` / `usize.MAX` etc. parse as
        // `FieldAccess(Identifier("i64"), "MAX")`. Intercept before the
        // normal field-access path so the bare primitive identifier
        // doesn't fall through to a generic compile_expr that would
        // either panic or produce wrong codegen. Mirrors the typechecker
        // and interpreter early-intercepts for the same expression
        // shape.
        if let ExprKind::Identifier(name) = &object.kind {
            if let Some(cv) = crate::prelude::lookup_primitive_const(name, field) {
                return Ok(self.compile_primitive_const(cv));
            }
        }
        // Indexed-shared-struct receiver: `nodes[i].field` where
        // `nodes: Vec[Shared(N)]`. Mirror of `compile_field_store`'s
        // Index branch — load the heap pointer at `nodes[i]`, GEP into
        // the heap struct's field, return the typed load. Without this,
        // the access falls through to the generic Struct-value extract
        // path which returns `i64 0` for any shared-struct receiver.
        if let ExprKind::Index {
            object: inner,
            index,
        } = &object.kind
        {
            if let ExprKind::Identifier(outer_name) = &inner.kind {
                if let Some(elem_te) = self.var_elem_type_exprs.get(outer_name.as_str()).cloned() {
                    if let TypeKind::Path(path) = &elem_te.kind {
                        if let Some(seg) = path.segments.first() {
                            if let Some(info) = self.shared_types.get(seg.as_str()).cloned() {
                                if !info.is_enum {
                                    let outer_name = outer_name.clone();
                                    let (elem_ptr, _) =
                                        if self.vec_elem_types.contains_key(outer_name.as_str()) {
                                            self.lower_indexed_elem_ptr_vec(&outer_name, index)?
                                        } else if self
                                            .slice_elem_types
                                            .contains_key(outer_name.as_str())
                                        {
                                            self.lower_indexed_elem_ptr_slice(&outer_name, index)?
                                        } else {
                                            let zero = self.context.i64_type().const_zero();
                                            return Ok(zero.into());
                                        };
                                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                                    let heap_ptr = self
                                        .builder
                                        .build_load(ptr_ty, elem_ptr, "idx.shared.read")
                                        .unwrap()
                                        .into_pointer_value();
                                    if let Some(names) = self.struct_field_names.get(seg) {
                                        if let Some(idx) = names.iter().position(|n| n == field) {
                                            let field_ptr = self
                                                .builder
                                                .build_struct_gep(
                                                    info.heap_type,
                                                    heap_ptr,
                                                    (idx + 1) as u32,
                                                    &format!("sh_idx_{}", field),
                                                )
                                                .unwrap();
                                            let field_ty = info
                                                .heap_type
                                                .get_field_type_at_index((idx + 1) as u32)
                                                .unwrap();
                                            return Ok(self
                                                .builder
                                                .build_load(field_ty, field_ptr, field)
                                                .unwrap());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Call-chain field access on a shared-struct return — bug #8.
        // `helper().val` where `helper() -> SharedT` lowers the call to
        // a pointer to the heap object; we GEP into the field, load, then
        // rc_dec the temp so the returned RC=1 doesn't leak. The bug #7
        // fix (move-out rc_inc in the callee) ensures the returned ptr is
        // RC≥1, so the caller owns one ref that must be released after
        // the field load. Without this branch the access falls through to
        // the generic `StructValue` path which sees a `PointerValue` and
        // silently returns `i64 0`. Covers `Call`, `MethodCall`, and
        // `Path`-based associated calls (`T.builder()`) — any shape whose
        // static return type names a known shared struct.
        if let Some((type_name, info)) = self.shared_type_for_call_like(object) {
            if !info.is_enum {
                if let Some(names) = self.struct_field_names.get(&type_name).cloned() {
                    if let Some(idx) = names.iter().position(|n| n == field) {
                        let ptr = self.compile_expr(object)?.into_pointer_value();
                        // Fields start at heap index 1 (index 0 is refcount).
                        let field_ptr = self
                            .builder
                            .build_struct_gep(
                                info.heap_type,
                                ptr,
                                (idx + 1) as u32,
                                &format!("sh_call_{}", field),
                            )
                            .unwrap();
                        let field_ty = info
                            .heap_type
                            .get_field_type_at_index((idx + 1) as u32)
                            .unwrap();
                        let loaded = self.builder.build_load(field_ty, field_ptr, field).unwrap();
                        // `Option[shared T]` field: the upcoming
                        // `emit_rc_dec(ptr)` runs the outer Node's
                        // recursive drop fn, which walks the
                        // `next: Option[shared T]` field and dec's
                        // its inner ptr. The loaded SSA register
                        // would then alias freed memory — a
                        // use-after-free for any subsequent access
                        // through the returned value
                        // (`match v { Some(n) => n.val }`, `v.next.val`,
                        // etc.). Bump the inner ptr's RC here so the
                        // recursive drop's dec brings it back to the
                        // original count; the caller (let-stmt
                        // path's `shared_option_info` detection, the
                        // match-scrutinee binding, or the next
                        // FieldAccess hop in a chain) takes ownership
                        // of the +1 with its own RcDecOption.
                        // Mirrors how the let-stmt's
                        // `shared_option_info` arm doesn't inc for
                        // an aliasing source — the +1 emitted here
                        // *is* the alias source's transfer of one
                        // owned ref into the field-access result.
                        if let Some(field_te) = self
                            .struct_field_type_exprs
                            .get(&type_name)
                            .and_then(|v| v.get(idx))
                            .cloned()
                        {
                            if let Some((_, inner_info)) =
                                self.option_inner_shared_type_for_type_expr(&field_te)
                            {
                                self.emit_option_inner_rc_inc_for_loaded(
                                    loaded,
                                    inner_info.heap_type,
                                );
                            }
                        }
                        // The call-result temp owns one ref (RC=1 from the
                        // callee's move-out inc + scope-exit dec under bug
                        // #7). Release it now — the field value has been
                        // read into a register and no longer depends on
                        // the heap object.
                        self.emit_rc_dec(info.heap_type, ptr);
                        return Ok(loaded);
                    }
                }
            }
        }

        // Shared type: object compiles to a pointer; field access via GEP.
        if let Some((type_name, info)) = self.shared_type_for_expr(object) {
            if !info.is_enum {
                let ptr = self.compile_expr(object)?.into_pointer_value();
                if let Some(names) = self.struct_field_names.get(&type_name) {
                    if let Some(idx) = names.iter().position(|n| n == field) {
                        // Fields start at heap index 1 (index 0 is refcount).
                        let field_ptr = self
                            .builder
                            .build_struct_gep(
                                info.heap_type,
                                ptr,
                                (idx + 1) as u32,
                                &format!("sh_{}", field),
                            )
                            .unwrap();
                        let field_ty = info
                            .heap_type
                            .get_field_type_at_index((idx + 1) as u32)
                            .unwrap();
                        return Ok(self.builder.build_load(field_ty, field_ptr, field).unwrap());
                    }
                }
            }
        }

        let obj_val = self.compile_expr(object)?;
        if let BasicValueEnum::StructValue(sv) = obj_val {
            // Look up field index from struct type name in object's identifier
            let field_idx = self.field_index_for(object, field);
            if let Some(idx) = field_idx {
                return Ok(self.builder.build_extract_value(sv, idx, field).unwrap());
            }
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn compile_field_store(
        &mut self,
        object: &Expr,
        field: &str,
        new_val: BasicValueEnum<'ctx>,
        rhs_is_fresh: bool,
    ) -> Result<(), String> {
        // Indexed-shared-struct receiver: `nodes[i].field = X` where
        // `nodes: Vec[Shared(N)]`. Load the heap pointer at `nodes[i]`
        // (the element slot stores the RC pointer cast to its LLVM
        // type), then GEP into the heap struct and store. Without this
        // branch the assignment silently falls through to the no-op
        // `Ok(())` exit at the function tail — the field write compiles
        // clean but does not persist, so a subsequent `nodes[i].field`
        // read returns the stale value.
        if let ExprKind::Index {
            object: inner,
            index,
        } = &object.kind
        {
            if let ExprKind::Identifier(outer_name) = &inner.kind {
                if let Some(elem_te) = self.var_elem_type_exprs.get(outer_name.as_str()).cloned() {
                    if let TypeKind::Path(path) = &elem_te.kind {
                        if let Some(seg) = path.segments.first() {
                            if let Some(info) = self.shared_types.get(seg.as_str()).cloned() {
                                if !info.is_enum {
                                    let outer_name = outer_name.clone();
                                    let (elem_ptr, _) =
                                        if self.vec_elem_types.contains_key(outer_name.as_str()) {
                                            self.lower_indexed_elem_ptr_vec(&outer_name, index)?
                                        } else if self
                                            .slice_elem_types
                                            .contains_key(outer_name.as_str())
                                        {
                                            self.lower_indexed_elem_ptr_slice(&outer_name, index)?
                                        } else {
                                            return Ok(());
                                        };
                                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                                    let heap_ptr = self
                                        .builder
                                        .build_load(ptr_ty, elem_ptr, "idx.shared.ptr")
                                        .unwrap()
                                        .into_pointer_value();
                                    if let Some(names) = self.struct_field_names.get(seg) {
                                        if let Some(idx) = names.iter().position(|n| n == field) {
                                            let field_ptr = self
                                                .builder
                                                .build_struct_gep(
                                                    info.heap_type,
                                                    heap_ptr,
                                                    (idx + 1) as u32,
                                                    &format!("sh_idx_{}_ptr", field),
                                                )
                                                .unwrap();
                                            self.builder.build_store(field_ptr, new_val).unwrap();
                                        }
                                    }
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        }

        // `self.field = …` parses as `FieldAccess { object: SelfValue, … }`,
        // and `self` is bound as a regular local named "self" — same lookup
        // path as a plain Identifier. Treat both shapes uniformly so
        // ref-self method bodies can mutate through the receiver.
        let var_name_owned: Option<String> = match &object.kind {
            ExprKind::Identifier(n) => Some(n.clone()),
            ExprKind::SelfValue => Some("self".to_string()),
            _ => None,
        };
        if let Some(var_name) = var_name_owned.as_deref() {
            // Shared type: store directly into the heap object via GEP.
            if let Some(type_name) = self.var_type_names.get(var_name).cloned() {
                if let Some(info) = self.shared_types.get(&type_name).cloned() {
                    if !info.is_enum {
                        if let Some(slot) = self.variables.get(var_name).copied() {
                            let ptr = self
                                .builder
                                .build_load(
                                    self.context.ptr_type(AddressSpace::default()),
                                    slot.ptr,
                                    var_name,
                                )
                                .unwrap()
                                .into_pointer_value();
                            if let Some(names) = self.struct_field_names.get(&type_name) {
                                if let Some(idx) = names.iter().position(|n| n == field) {
                                    let field_ptr = self
                                        .builder
                                        .build_struct_gep(
                                            info.heap_type,
                                            ptr,
                                            (idx + 1) as u32,
                                            &format!("sh_{}_ptr", field),
                                        )
                                        .unwrap();
                                    // `Option[shared T]` field-store: dec
                                    // the OLD inner ref (if Some) before
                                    // clobbering the slot, then store the
                                    // new value, then inc the NEW inner
                                    // ref (if Some and RHS isn't fresh).
                                    // Mirrors the Assign-arm's
                                    // `var_option_shared_heap` dispatch
                                    // for plain Option[shared T] bindings,
                                    // scaled up to a struct-field slot:
                                    // `field_ptr` is the per-field GEP
                                    // into the heap struct rather than a
                                    // per-binding alloca. Without this,
                                    // `tail.next = Some(node);` followed
                                    // by `tail.next = Some(new_node);`
                                    // strands the first node's whole
                                    // chain — the leak shape the 79a7db8
                                    // follow-up notes called out.
                                    //
                                    // Returns Ok(()) after the
                                    // refcount-aware path completes so
                                    // the plain `build_store` below
                                    // doesn't re-fire. No-op for
                                    // non-Option-shared fields, which
                                    // fall through to the plain store.
                                    if let Some(field_te) = self
                                        .struct_field_type_exprs
                                        .get(&type_name)
                                        .and_then(|v| v.get(idx))
                                        .cloned()
                                    {
                                        if let Some((_, inner_info)) =
                                            self.option_inner_shared_type_for_type_expr(&field_te)
                                        {
                                            self.emit_option_shared_field_store(
                                                field_ptr,
                                                new_val,
                                                inner_info.heap_type,
                                                rhs_is_fresh,
                                                field,
                                            );
                                            return Ok(());
                                        }
                                    }
                                    self.builder.build_store(field_ptr, new_val).unwrap();
                                }
                            }
                        }
                        return Ok(());
                    }
                }
            }

            // Ref / mut-ref struct param: write through the pointer so the
            // caller's storage observes the update. The owned-param path
            // below would mutate a local copy of the struct value, so the
            // caller never sees the change — the `mut ref self` mutation
            // bug fixed in this slice. `get_data_ptr` returns the alloca
            // for owned bindings and the dereferenced pointer for ref
            // params, so we use it uniformly when GEP'ing into a struct.
            if let Some(&BasicTypeEnum::StructType(struct_ty)) = self.ref_params.get(var_name) {
                if let Some(idx) = self.field_index_for(object, field) {
                    if let Some(ptr) = self.get_data_ptr(var_name) {
                        let field_ptr = self
                            .builder
                            .build_struct_gep(struct_ty, ptr, idx, &format!("ref_{}_ptr", field))
                            .unwrap();
                        self.builder.build_store(field_ptr, new_val).unwrap();
                        return Ok(());
                    }
                }
            }

            if let Some(slot) = self.variables.get(var_name).copied() {
                let obj_val = self
                    .builder
                    .build_load(slot.ty, slot.ptr, var_name)
                    .unwrap();
                if let BasicValueEnum::StructValue(sv) = obj_val {
                    let field_idx = self.field_index_for(object, field);
                    if let Some(idx) = field_idx {
                        let updated = self
                            .builder
                            .build_insert_value(sv, new_val, idx, field)
                            .unwrap();
                        self.builder.build_store(slot.ptr, updated).unwrap();
                    }
                }
            }
        }
        Ok(())
    }

    /// Inc the inner shared-T ref of a just-loaded `Option[shared T]`
    /// SSA value, when its tag is Some and its inner ptr is non-null.
    /// Used by `compile_field_access`'s call-chain branch to balance
    /// the upcoming `emit_rc_dec` on the outer call temp — the outer
    /// temp's recursive drop walks the Option field and dec's the
    /// inner ptr, which would free the chain before the caller can
    /// read it. The inc emitted here brings the net effect to "the
    /// caller owns the inner ref"; the caller's let-stmt / match-arm
    /// binding registration takes ownership of the +1.
    ///
    /// Operates on a struct-valued SSA register rather than a slot
    /// pointer — the parent's `RcDecOption`-style cleanup paths read
    /// from a slot, but here the field value lives only in a register
    /// at the time the temp's RC is released, so the tag / w0 come
    /// from `build_extract_value`. Same Some-tag + null-guard
    /// structure as `emit_option_shared_field_store`'s old-side
    /// path, just consuming a register rather than a GEP slot.
    pub(super) fn emit_option_inner_rc_inc_for_loaded(
        &self,
        loaded: BasicValueEnum<'ctx>,
        inner_heap_type: StructType<'ctx>,
    ) {
        let sv = match loaded {
            BasicValueEnum::StructValue(sv) => sv,
            _ => return,
        };
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let some_tag = self
            .enum_layouts
            .get("Option")
            .and_then(|l| l.tags.get("Some").copied())
            .unwrap_or(1);
        let some_tag_const = i64_t.const_int(some_tag, false);
        let Some(fn_val) = self.current_fn else {
            return;
        };
        // Tag is field 0 of the Option layout; w0 (inner ptr as i64)
        // is field 1. Anything else (w1 / w2) is unused for the
        // shared-ref payload — Option[shared T]'s payload is the
        // single ptr-as-i64 in w0 per `coerce_to_payload_words`'s
        // primitive fast path.
        let tag = self
            .builder
            .build_extract_value(sv, 0, "opt.chain.tag")
            .unwrap()
            .into_int_value();
        let is_some = self
            .builder
            .build_int_compare(IntPredicate::EQ, tag, some_tag_const, "opt.chain.is_some")
            .unwrap();
        let do_bb = self.context.append_basic_block(fn_val, "opt.chain.inc.do");
        let skip_bb = self
            .context
            .append_basic_block(fn_val, "opt.chain.inc.skip");
        self.builder
            .build_conditional_branch(is_some, do_bb, skip_bb)
            .unwrap();
        self.builder.position_at_end(do_bb);
        let w0 = self
            .builder
            .build_extract_value(sv, 1, "opt.chain.w0")
            .unwrap()
            .into_int_value();
        let inner = self
            .builder
            .build_int_to_ptr(w0, ptr_ty, "opt.chain.inner")
            .unwrap();
        let inner_is_null = self
            .builder
            .build_is_null(inner, "opt.chain.inner.is_null")
            .unwrap();
        let real_do_bb = self
            .context
            .append_basic_block(fn_val, "opt.chain.inc.real");
        self.builder
            .build_conditional_branch(inner_is_null, skip_bb, real_do_bb)
            .unwrap();
        self.builder.position_at_end(real_do_bb);
        self.emit_rc_inc(inner_heap_type, inner);
        self.builder.build_unconditional_branch(skip_bb).unwrap();
        self.builder.position_at_end(skip_bb);
    }

    /// Refcount-aware store into an `Option[shared T]` struct field.
    /// `field_ptr` is the GEP'd address of the Option slot inside the
    /// heap struct; `new_val` is the just-compiled RHS Option struct
    /// SSA; `inner_heap_type` is the heap layout of `T`; `rhs_is_fresh`
    /// matches the parent commit's `rhs_yields_fresh_ref` semantics.
    /// Mirrors the Assign-arm's `var_option_shared_heap` dispatch in
    /// `compile_stmt`:
    ///
    ///   1. Load the old slot's tag; if Some, dec the old inner ptr.
    ///   2. Store the new Option value.
    ///   3. If RHS is not fresh, branch on the new tag; if Some, inc
    ///      the new inner ptr.
    ///
    /// Without this, `obj.next = some_other_opt;` strands the old
    /// inner ref; `obj.next = Some(node);` over the same field
    /// across iterations leaks one chain per overwrite. The kata's
    /// `tail.next = Some(node); tail = node;` shape doesn't surface
    /// the leak (each iter creates a fresh `tail` so each field
    /// store sees a None old value), but mutations against a
    /// long-lived holder (`head.next = Some(...);` on a persistent
    /// `head`) hit it.
    pub(super) fn emit_option_shared_field_store(
        &self,
        field_ptr: PointerValue<'ctx>,
        new_val: BasicValueEnum<'ctx>,
        inner_heap_type: StructType<'ctx>,
        rhs_is_fresh: bool,
        field_name: &str,
    ) {
        let option_ty = self.enum_layouts["Option"].llvm_type;
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let some_tag = self
            .enum_layouts
            .get("Option")
            .and_then(|l| l.tags.get("Some").copied())
            .unwrap_or(1);
        let some_tag_const = i64_t.const_int(some_tag, false);
        let Some(fn_val) = self.current_fn else {
            // Defensive: outside a function body, fall back to a plain
            // store. The let-stmt / Assign sites are inside fn bodies
            // by construction, so this branch is unreachable today.
            let _ = self.builder.build_store(field_ptr, new_val);
            return;
        };
        // ── Step 1: dec old inner if old is Some. ──
        let old_tag_ptr = self
            .builder
            .build_struct_gep(
                option_ty,
                field_ptr,
                0,
                &format!("opt.fld.{field_name}.old.tag.p"),
            )
            .unwrap();
        let old_tag = self
            .builder
            .build_load(i64_t, old_tag_ptr, &format!("opt.fld.{field_name}.old.tag"))
            .unwrap()
            .into_int_value();
        let old_is_some = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                old_tag,
                some_tag_const,
                &format!("opt.fld.{field_name}.old.is_some"),
            )
            .unwrap();
        let old_do_bb = self
            .context
            .append_basic_block(fn_val, &format!("opt.fld.{field_name}.old.do"));
        let old_skip_bb = self
            .context
            .append_basic_block(fn_val, &format!("opt.fld.{field_name}.old.skip"));
        self.builder
            .build_conditional_branch(old_is_some, old_do_bb, old_skip_bb)
            .unwrap();
        self.builder.position_at_end(old_do_bb);
        let old_w0_ptr = self
            .builder
            .build_struct_gep(
                option_ty,
                field_ptr,
                1,
                &format!("opt.fld.{field_name}.old.w0.p"),
            )
            .unwrap();
        let old_w0 = self
            .builder
            .build_load(i64_t, old_w0_ptr, &format!("opt.fld.{field_name}.old.w0"))
            .unwrap()
            .into_int_value();
        let old_inner = self
            .builder
            .build_int_to_ptr(old_w0, ptr_ty, &format!("opt.fld.{field_name}.old.inner"))
            .unwrap();
        let old_is_null = self
            .builder
            .build_is_null(
                old_inner,
                &format!("opt.fld.{field_name}.old.inner.is_null"),
            )
            .unwrap();
        let old_real_do_bb = self
            .context
            .append_basic_block(fn_val, &format!("opt.fld.{field_name}.old.real_do"));
        self.builder
            .build_conditional_branch(old_is_null, old_skip_bb, old_real_do_bb)
            .unwrap();
        self.builder.position_at_end(old_real_do_bb);
        self.emit_rc_dec(inner_heap_type, old_inner);
        self.builder
            .build_unconditional_branch(old_skip_bb)
            .unwrap();
        self.builder.position_at_end(old_skip_bb);
        // ── Step 2: store the new Option value. ──
        self.builder.build_store(field_ptr, new_val).unwrap();
        // ── Step 3: inc new inner if RHS is an aliasing source. ──
        if !rhs_is_fresh {
            let new_tag_ptr = self
                .builder
                .build_struct_gep(
                    option_ty,
                    field_ptr,
                    0,
                    &format!("opt.fld.{field_name}.new.tag.p"),
                )
                .unwrap();
            let new_tag = self
                .builder
                .build_load(i64_t, new_tag_ptr, &format!("opt.fld.{field_name}.new.tag"))
                .unwrap()
                .into_int_value();
            let new_is_some = self
                .builder
                .build_int_compare(
                    IntPredicate::EQ,
                    new_tag,
                    some_tag_const,
                    &format!("opt.fld.{field_name}.new.is_some"),
                )
                .unwrap();
            let new_do_bb = self
                .context
                .append_basic_block(fn_val, &format!("opt.fld.{field_name}.new.do"));
            let new_skip_bb = self
                .context
                .append_basic_block(fn_val, &format!("opt.fld.{field_name}.new.skip"));
            self.builder
                .build_conditional_branch(new_is_some, new_do_bb, new_skip_bb)
                .unwrap();
            self.builder.position_at_end(new_do_bb);
            let new_w0_ptr = self
                .builder
                .build_struct_gep(
                    option_ty,
                    field_ptr,
                    1,
                    &format!("opt.fld.{field_name}.new.w0.p"),
                )
                .unwrap();
            let new_w0 = self
                .builder
                .build_load(i64_t, new_w0_ptr, &format!("opt.fld.{field_name}.new.w0"))
                .unwrap()
                .into_int_value();
            let new_inner = self
                .builder
                .build_int_to_ptr(new_w0, ptr_ty, &format!("opt.fld.{field_name}.new.inner"))
                .unwrap();
            let new_is_null = self
                .builder
                .build_is_null(
                    new_inner,
                    &format!("opt.fld.{field_name}.new.inner.is_null"),
                )
                .unwrap();
            let new_real_do_bb = self
                .context
                .append_basic_block(fn_val, &format!("opt.fld.{field_name}.new.real_do"));
            self.builder
                .build_conditional_branch(new_is_null, new_skip_bb, new_real_do_bb)
                .unwrap();
            self.builder.position_at_end(new_real_do_bb);
            self.emit_rc_inc(inner_heap_type, new_inner);
            self.builder
                .build_unconditional_branch(new_skip_bb)
                .unwrap();
            self.builder.position_at_end(new_skip_bb);
        }
    }

    pub(super) fn compile_tuple_index(
        &mut self,
        object: &Expr,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let obj_val = self.compile_expr(object)?;
        if let BasicValueEnum::StructValue(sv) = obj_val {
            return Ok(self
                .builder
                .build_extract_value(sv, index as u32, "tidx")
                .unwrap());
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    pub(super) fn field_index_for(&self, object: &Expr, field: &str) -> Option<u32> {
        // Try to resolve by walking the object expression to its
        // user-type name, then looking up `field` in that struct's
        // field registry. Chained `o.inner.name` requires walking the
        // inner FieldAccess to recover `o.inner`'s declared type from
        // `struct_field_type_names`. See `type_name_of_expr`.
        if let Some(type_name) = self.type_name_of_expr(object) {
            if let Some(names) = self.struct_field_names.get(type_name.as_str()) {
                if let Some(idx) = names.iter().position(|n| n == field) {
                    return Some(idx as u32);
                }
            }
        }
        // Fall back: numeric index for tuple fields like `.0`, `.1`
        field.parse::<u32>().ok()
    }

    /// Resolve the user-type name of an arbitrary expression by walking
    /// `Identifier` / `SelfValue` / `FieldAccess` chains. Returns
    /// `None` for primitive-typed expressions, calls whose return type
    /// isn't a known struct, or any shape outside this trio. Companion
    /// to `type_name_of` (which only handles direct identifiers and
    /// struct literals).
    pub(super) fn type_name_of_expr(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(n) => self.var_type_names.get(n.as_str()).cloned(),
            ExprKind::SelfValue => self.var_type_names.get("self").cloned(),
            ExprKind::StructLiteral { path, .. } => path.last().cloned(),
            ExprKind::FieldAccess { object, field } => {
                let obj_ty = self.type_name_of_expr(object)?;
                let field_names = self.struct_field_names.get(obj_ty.as_str())?;
                let idx = field_names.iter().position(|n| n == field)?;
                let field_ty_names = self.struct_field_type_names.get(obj_ty.as_str())?;
                field_ty_names.get(idx).and_then(|n| n.clone())
            }
            _ => None,
        }
    }

    /// Return the Kāra type name for a compiled expression, if known.
    pub(super) fn type_name_of(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(n) => self.var_type_names.get(n.as_str()).cloned(),
            ExprKind::StructLiteral { path, .. } => path.last().cloned(),
            // `let x = vec_of_chars[i]` — recover the element type's name
            // from `var_elem_type_exprs` so downstream consumers (notably
            // `expr_is_char` for the print/f-string glyph rendering)
            // pick up `x: char`. Generalises to any single-path element
            // type, e.g. `let p = points[0]` → "Point" — same shape the
            // shared-struct field-access machinery already relies on.
            ExprKind::Index { object, .. } => {
                if let ExprKind::Identifier(n) = &object.kind {
                    if let Some(te) = self.var_elem_type_exprs.get(n.as_str()) {
                        if let TypeKind::Path(p) = &te.kind {
                            return p.segments.last().cloned();
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// True when `expr` has source-level type `char` per the codegen-side
    /// type tracking. Drives the print and f-string char-arms: a true
    /// result routes the value through `emit_codepoint_to_utf8` so the
    /// glyph is rendered rather than the integer codepoint.
    pub(super) fn expr_is_char(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::CharLit(_) => true,
            ExprKind::Identifier(n) => self
                .var_type_names
                .get(n.as_str())
                .map(|s| s == "char")
                .unwrap_or(false),
            // `vec_of_chars[i]` / `array_of_chars[i]` — check the
            // collection's element TypeExpr is a bare `char` path.
            ExprKind::Index { object, .. } => {
                if let ExprKind::Identifier(n) = &object.kind {
                    if let Some(te) = self.var_elem_type_exprs.get(n.as_str()) {
                        if let TypeKind::Path(p) = &te.kind {
                            return p.segments.last().map(|s| s == "char").unwrap_or(false);
                        }
                    }
                }
                false
            }
            _ => false,
        }
    }

    // ── Cast ──────────────────────────────────────────────────────

    pub(super) fn compile_cast(
        &self,
        val: BasicValueEnum<'ctx>,
        target: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match (val, target) {
            (BasicValueEnum::IntValue(iv), BasicTypeEnum::IntType(tt)) => {
                let result = self.builder.build_int_cast(iv, tt, "cast").unwrap();
                Ok(result.into())
            }
            (BasicValueEnum::IntValue(iv), BasicTypeEnum::FloatType(ft)) => {
                let result = self
                    .builder
                    .build_signed_int_to_float(iv, ft, "cast")
                    .unwrap();
                Ok(result.into())
            }
            (BasicValueEnum::FloatValue(fv), BasicTypeEnum::IntType(it)) => {
                let result = self
                    .builder
                    .build_float_to_signed_int(fv, it, "cast")
                    .unwrap();
                Ok(result.into())
            }
            (BasicValueEnum::FloatValue(fv), BasicTypeEnum::FloatType(ft)) => {
                let result = self.builder.build_float_cast(fv, ft, "cast").unwrap();
                Ok(result.into())
            }
            _ => Ok(val),
        }
    }

    // ── Binary / unary operators ──────────────────────────────────

    /// Emit short-circuit `and` / `or` per documented design intent
    /// (roadmap.md:425, 429): the RHS is only compiled into a basic
    /// block reachable when the LHS doesn't already determine the
    /// result. Without this, the RHS would emit unconditionally and
    /// its side-effects (panicking index, dropped fn call) would fire
    /// even when short-circuited — same shape as the interpreter's
    /// eager-eval bug fixed in lockstep.
    pub(super) fn compile_short_circuit(
        &mut self,
        op: &BinOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let lhs_val = self.compile_expr(left)?.into_int_value();
        let lhs_end_bb = self.builder.get_insert_block().unwrap();

        let rhs_bb = self.context.append_basic_block(fn_val, "sc.rhs");
        let merge_bb = self.context.append_basic_block(fn_val, "sc.merge");

        // `and`: lhs true → eval rhs; lhs false → short-circuit to false.
        // `or`:  lhs true → short-circuit to true; lhs false → eval rhs.
        let (true_dest, false_dest) = match op {
            BinOp::And => (rhs_bb, merge_bb),
            BinOp::Or => (merge_bb, rhs_bb),
            _ => unreachable!("compile_short_circuit only handles And/Or"),
        };
        self.builder
            .build_conditional_branch(lhs_val, true_dest, false_dest)
            .unwrap();

        // Bounds-check-elision propagation: when the RHS of `lhs and rhs`
        // fires, we've branch-proved that lhs holds. Any index-safety fact
        // asserted by lhs is in scope for rhs's compilation. This is how
        // the kata's `while lo >= 0 and hi < n and chars[lo] == chars[hi]`
        // pattern lets the indexing in the third conjunct skip its
        // bounds check — `lo >= 0` and `hi < n` are conjuncts evaluated
        // first under short-circuit, so by the time chars[lo] / chars[hi]
        // lower (in compile_vec_index), the facts are on the stack.
        let pushed = if matches!(op, BinOp::And) {
            let facts = self.collect_asserted_bounds_from_guard(left);
            let n = facts.len();
            self.asserted_index_bounds.extend(facts);
            n
        } else {
            0
        };

        self.builder.position_at_end(rhs_bb);
        let rhs_val = self.compile_expr(right)?.into_int_value();
        let rhs_end_bb = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge_bb).unwrap();

        // Pop the temporarily-asserted facts so the merge / surrounding
        // scope sees only its own bounds. Compile_while's body-entry push
        // re-establishes them for body code on the long-lived path.
        for _ in 0..pushed {
            self.asserted_index_bounds.pop();
        }

        self.builder.position_at_end(merge_bb);
        let bool_ty = self.context.bool_type();
        let short_const = match op {
            BinOp::And => bool_ty.const_int(0, false),
            BinOp::Or => bool_ty.const_int(1, false),
            _ => unreachable!(),
        };
        let phi = self.builder.build_phi(bool_ty, "sc.result").unwrap();
        phi.add_incoming(&[(&short_const, lhs_end_bb), (&rhs_val, rhs_end_bb)]);
        Ok(phi.as_basic_value())
    }

    pub(super) fn compile_binop(
        &mut self,
        op: &BinOp,
        lhs: BasicValueEnum<'ctx>,
        rhs: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Struct path: strings or user-defined structs.
        if lhs.is_struct_value() && rhs.is_struct_value() {
            let ls = lhs.into_struct_value();
            let rs = rhs.into_struct_value();
            let field_count = ls.get_type().count_fields();
            let vec_fields = self.vec_struct_type().count_fields();
            // String/Vec layout ({ ptr, i64, i64 }) — 3 fields.
            if field_count == vec_fields {
                return self.compile_string_binop(op, ls, rs);
            }
            // User struct equality: field-by-field comparison.
            if matches!(op, BinOp::Eq | BinOp::NotEq) {
                return self.compile_struct_eq(op, ls, rs);
            }
            return Err(format!("Unsupported struct binary op: {:?}", op));
        }

        // Float path
        if lhs.is_float_value() || rhs.is_float_value() {
            let lf = self.to_float(lhs)?;
            let rf = self.to_float(rhs)?;
            return self.compile_float_binop(op, lf, rf);
        }

        let lv = lhs.into_int_value();
        let rv = rhs.into_int_value();
        let result = match op {
            BinOp::Add => self.builder.build_int_nsw_add(lv, rv, "add").unwrap(),
            BinOp::Sub => self.builder.build_int_nsw_sub(lv, rv, "sub").unwrap(),
            BinOp::Mul => self.builder.build_int_nsw_mul(lv, rv, "mul").unwrap(),
            BinOp::Div => self.builder.build_int_signed_div(lv, rv, "div").unwrap(),
            BinOp::Mod => self.builder.build_int_signed_rem(lv, rv, "mod").unwrap(),
            BinOp::Eq => self
                .builder
                .build_int_compare(IntPredicate::EQ, lv, rv, "eq")
                .unwrap(),
            BinOp::NotEq => self
                .builder
                .build_int_compare(IntPredicate::NE, lv, rv, "ne")
                .unwrap(),
            BinOp::Lt => self
                .builder
                .build_int_compare(IntPredicate::SLT, lv, rv, "lt")
                .unwrap(),
            BinOp::LtEq => self
                .builder
                .build_int_compare(IntPredicate::SLE, lv, rv, "le")
                .unwrap(),
            BinOp::Gt => self
                .builder
                .build_int_compare(IntPredicate::SGT, lv, rv, "gt")
                .unwrap(),
            BinOp::GtEq => self
                .builder
                .build_int_compare(IntPredicate::SGE, lv, rv, "ge")
                .unwrap(),
            BinOp::And => self.builder.build_and(lv, rv, "and").unwrap(),
            BinOp::Or => self.builder.build_or(lv, rv, "or").unwrap(),
            BinOp::BitAnd => self.builder.build_and(lv, rv, "bitand").unwrap(),
            BinOp::BitOr => self.builder.build_or(lv, rv, "bitor").unwrap(),
            BinOp::BitXor => self.builder.build_xor(lv, rv, "bitxor").unwrap(),
            BinOp::Shl => self.builder.build_left_shift(lv, rv, "shl").unwrap(),
            BinOp::Shr => self.builder.build_right_shift(lv, rv, true, "shr").unwrap(),
            _ => return Err(format!("Unsupported binary op: {:?}", op)),
        };
        Ok(result.into())
    }

    pub(super) fn compile_struct_eq(
        &mut self,
        op: &BinOp,
        lhs: inkwell::values::StructValue<'ctx>,
        rhs: inkwell::values::StructValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let field_count = lhs.get_type().count_fields();
        let bool_t = self.context.bool_type();
        let mut result = bool_t.const_int(1, false); // start true (all equal)

        for i in 0..field_count {
            let l_field = self
                .builder
                .build_extract_value(lhs, i, &format!("l.f{}", i))
                .unwrap();
            let r_field = self
                .builder
                .build_extract_value(rhs, i, &format!("r.f{}", i))
                .unwrap();
            // Recursively compare the field.
            let field_eq = self.compile_binop(&BinOp::Eq, l_field, r_field)?;
            result = self
                .builder
                .build_and(result, field_eq.into_int_value(), &format!("eq.f{}", i))
                .unwrap();
        }

        if matches!(op, BinOp::NotEq) {
            Ok(self.builder.build_not(result, "struct_ne").unwrap().into())
        } else {
            Ok(result.into())
        }
    }

    pub(super) fn compile_string_binop(
        &self,
        op: &BinOp,
        lhs: inkwell::values::StructValue<'ctx>,
        rhs: inkwell::values::StructValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i32_t = self.context.i32_type();

        // Extract ptr and len from each string struct.
        let l_ptr = self
            .builder
            .build_extract_value(lhs, 0, "l.ptr")
            .unwrap()
            .into_pointer_value();
        let l_len = self
            .builder
            .build_extract_value(lhs, 1, "l.len")
            .unwrap()
            .into_int_value();
        let r_ptr = self
            .builder
            .build_extract_value(rhs, 0, "r.ptr")
            .unwrap()
            .into_pointer_value();
        let r_len = self
            .builder
            .build_extract_value(rhs, 1, "r.len")
            .unwrap()
            .into_int_value();

        match op {
            BinOp::Eq | BinOp::NotEq => {
                // Fast reject: if lengths differ, strings are not equal.
                let len_eq = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, l_len, r_len, "len_eq")
                    .unwrap();
                // memcmp the data.
                let cmp_result = self
                    .builder
                    .build_call(
                        self.memcmp_fn,
                        &[l_ptr.into(), r_ptr.into(), l_len.into()],
                        "memcmp",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let data_eq = self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        cmp_result,
                        i32_t.const_int(0, false),
                        "data_eq",
                    )
                    .unwrap();
                let is_eq = self.builder.build_and(len_eq, data_eq, "str_eq").unwrap();
                if matches!(op, BinOp::NotEq) {
                    Ok(self.builder.build_not(is_eq, "str_ne").unwrap().into())
                } else {
                    Ok(is_eq.into())
                }
            }
            BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                // Lexicographic comparison: memcmp on min(l_len, r_len), then compare lengths.
                let cmp_lens = self
                    .builder
                    .build_int_compare(IntPredicate::ULT, l_len, r_len, "l_shorter")
                    .unwrap();
                let min_len = self
                    .builder
                    .build_select(cmp_lens, l_len, r_len, "min_len")
                    .unwrap()
                    .into_int_value();
                let cmp_result = self
                    .builder
                    .build_call(
                        self.memcmp_fn,
                        &[l_ptr.into(), r_ptr.into(), min_len.into()],
                        "memcmp",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let zero = i32_t.const_int(0, false);
                // If memcmp != 0, use its sign. If memcmp == 0, shorter string is "less".
                let cmp_is_zero = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, cmp_result, zero, "cmp_zero")
                    .unwrap();
                // When cmp == 0, compare lengths as signed i64 difference.
                let len_diff = self
                    .builder
                    .build_int_sub(l_len, r_len, "len_diff")
                    .unwrap();
                let len_diff_i32 = self
                    .builder
                    .build_int_truncate(len_diff, i32_t, "len_diff32")
                    .unwrap();
                let effective_cmp = self
                    .builder
                    .build_select(cmp_is_zero, len_diff_i32, cmp_result, "eff_cmp")
                    .unwrap()
                    .into_int_value();
                let pred = match op {
                    BinOp::Lt => IntPredicate::SLT,
                    BinOp::LtEq => IntPredicate::SLE,
                    BinOp::Gt => IntPredicate::SGT,
                    BinOp::GtEq => IntPredicate::SGE,
                    _ => unreachable!(),
                };
                let result = self
                    .builder
                    .build_int_compare(pred, effective_cmp, zero, "str_cmp")
                    .unwrap();
                Ok(result.into())
            }
            BinOp::Add => {
                // String concatenation: allocate new buffer, copy both, return new string.
                let new_len = self.builder.build_int_add(l_len, r_len, "cat_len").unwrap();
                let new_buf = self
                    .builder
                    .build_call(self.malloc_fn, &[new_len.into()], "cat_buf")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_pointer_value();
                // Copy left.
                self.builder
                    .build_memcpy(new_buf, 1, l_ptr, 1, l_len)
                    .unwrap();
                // Copy right after left.
                let i8_ty = self.context.i8_type();
                let dest2 = unsafe {
                    self.builder
                        .build_gep(i8_ty, new_buf, &[l_len], "cat_dest2")
                        .unwrap()
                };
                self.builder
                    .build_memcpy(dest2, 1, r_ptr, 1, r_len)
                    .unwrap();
                // Build result string struct.
                let str_ty = self.vec_struct_type();
                let mut agg = str_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, new_buf, 0, "cat.ptr")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, new_len, 1, "cat.len")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, new_len, 2, "cat.cap")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            _ => Err(format!("Unsupported string binary op: {:?}", op)),
        }
    }

    pub(super) fn compile_float_binop(
        &self,
        op: &BinOp,
        lf: inkwell::values::FloatValue<'ctx>,
        rf: inkwell::values::FloatValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match op {
            BinOp::Add => Ok(self.builder.build_float_add(lf, rf, "fadd").unwrap().into()),
            BinOp::Sub => Ok(self.builder.build_float_sub(lf, rf, "fsub").unwrap().into()),
            BinOp::Mul => Ok(self.builder.build_float_mul(lf, rf, "fmul").unwrap().into()),
            BinOp::Div => Ok(self.builder.build_float_div(lf, rf, "fdiv").unwrap().into()),
            BinOp::Mod => Ok(self.builder.build_float_rem(lf, rf, "frem").unwrap().into()),
            BinOp::Eq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OEQ, lf, rf, "feq")
                .unwrap()
                .into()),
            BinOp::NotEq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::ONE, lf, rf, "fne")
                .unwrap()
                .into()),
            BinOp::Lt => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OLT, lf, rf, "flt")
                .unwrap()
                .into()),
            BinOp::LtEq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OLE, lf, rf, "fle")
                .unwrap()
                .into()),
            BinOp::Gt => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OGT, lf, rf, "fgt")
                .unwrap()
                .into()),
            BinOp::GtEq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OGE, lf, rf, "fge")
                .unwrap()
                .into()),
            _ => Err(format!("Unsupported float binary op: {:?}", op)),
        }
    }

    pub(super) fn to_float(
        &self,
        val: BasicValueEnum<'ctx>,
    ) -> Result<inkwell::values::FloatValue<'ctx>, String> {
        match val {
            BasicValueEnum::FloatValue(f) => Ok(f),
            BasicValueEnum::IntValue(i) => Ok(self
                .builder
                .build_signed_int_to_float(i, self.context.f64_type(), "itof")
                .unwrap()),
            _ => Err(format!("Cannot convert {:?} to float", val.get_type())),
        }
    }

    pub(super) fn compile_unaryop(
        &mut self,
        op: &UnaryOp,
        val: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match op {
            UnaryOp::Neg => {
                if val.is_float_value() {
                    Ok(self
                        .builder
                        .build_float_neg(val.into_float_value(), "fneg")
                        .unwrap()
                        .into())
                } else {
                    Ok(self
                        .builder
                        .build_int_neg(val.into_int_value(), "neg")
                        .unwrap()
                        .into())
                }
            }
            UnaryOp::Not | UnaryOp::BitNot => Ok(self
                .builder
                .build_not(val.into_int_value(), "not")
                .unwrap()
                .into()),
            // Deref is handled in compile_expr before reaching here.
            UnaryOp::Deref => Err("unreachable: Deref handled in compile_expr".into()),
        }
    }

    // ── Slice coercion ────────────────────────────────────────────

    /// Synthesize a `{ptr, i64}` slice header at a call site when the
    /// argument is an Array, Vec, or Slice value and the callee parameter
    /// expects `Slice[T]` / `mut Slice[T]`.
    ///
    /// Returns `Ok(None)` when the argument is not a recognized
    /// sequence source, signalling the caller to fall back to the
    /// default argument-passing path.
    pub(super) fn coerce_to_slice(
        &mut self,
        arg: &Expr,
        elem_ty: BasicTypeEnum<'ctx>,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();

        // Fast path: the argument is a named local variable whose
        // representation we already understand.
        if let ExprKind::Identifier(var_name) = &arg.kind {
            if let Some(slot) = self.variables.get(var_name.as_str()).copied() {
                // Owned Array[T, N]: point at the alloca, length is N.
                if let BasicTypeEnum::ArrayType(at) = slot.ty {
                    let len = i64_t.const_int(at.len() as u64, false);
                    return Ok(Some(self.build_slice_header(slice_ty, slot.ptr, len)));
                }
                // Already a slice: load and pass through.
                if self.slice_elem_types.contains_key(var_name.as_str()) {
                    let loaded = self
                        .builder
                        .build_load(slice_ty, slot.ptr, "slice.arg")
                        .unwrap();
                    return Ok(Some(loaded));
                }
                // Owned Vec[T]: the alloca holds the 3-field struct; load
                // its data-ptr and len fields, rebuild as a 2-field slice.
                if self.vec_elem_types.contains_key(var_name.as_str()) {
                    let vec_ty = self.vec_struct_type();
                    let data_ptr_ptr = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 0, "coerce.v.data.ptr")
                        .unwrap();
                    let data = self
                        .builder
                        .build_load(ptr_ty, data_ptr_ptr, "coerce.v.data")
                        .unwrap()
                        .into_pointer_value();
                    let len_ptr = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 1, "coerce.v.len.ptr")
                        .unwrap();
                    let len = self
                        .builder
                        .build_load(i64_t, len_ptr, "coerce.v.len")
                        .unwrap()
                        .into_int_value();
                    return Ok(Some(self.build_slice_header(slice_ty, data, len)));
                }
            }
            // Ref parameter: pointer-to-data is in ref_params.
            if let Some(&BasicTypeEnum::ArrayType(at)) = self.ref_params.get(var_name.as_str()) {
                let data = self.get_data_ptr(var_name).unwrap();
                let len = i64_t.const_int(at.len() as u64, false);
                return Ok(Some(self.build_slice_header(slice_ty, data, len)));
            }
        }

        // Range-indexing at a call boundary — e.g. `sum(a[1..4])`. Produce
        // a slice header with pointer-into-source and length `end - start`.
        if let ExprKind::Index { object, index } = &arg.kind {
            if let ExprKind::Range {
                start,
                end,
                inclusive,
            } = &index.kind
            {
                return self
                    .compile_range_slice(object, start, end, *inclusive, elem_ty)
                    .map(Some);
            }
        }

        let _ = elem_ty;
        Ok(None)
    }

    /// Assemble a two-field slice struct value from a data pointer and an
    /// i64 length.
    pub(super) fn build_slice_header(
        &self,
        slice_ty: StructType<'ctx>,
        data_ptr: PointerValue<'ctx>,
        len: inkwell::values::IntValue<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        let mut agg = slice_ty.get_undef();
        agg = self
            .builder
            .build_insert_value(agg, data_ptr, 0, "slice.ptr")
            .unwrap()
            .into_struct_value();
        agg = self
            .builder
            .build_insert_value(agg, len, 1, "slice.len")
            .unwrap()
            .into_struct_value();
        agg.into()
    }

    /// Construct a slice from a `collection[start..end]` expression —
    /// emits a bounds check and produces a `{ptr + start*stride, end - start}`
    /// slice header.
    pub(super) fn compile_range_slice(
        &mut self,
        object: &Expr,
        start: &Option<Box<Expr>>,
        end: &Option<Box<Expr>>,
        inclusive: bool,
        elem_ty: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();

        let start_val = if let Some(s) = start {
            self.compile_expr(s)?.into_int_value()
        } else {
            i64_t.const_int(0, false)
        };
        // Compile end expression now if present; defer to src_len for open-end
        // forms (`a..` and `..`). Inclusive adjustment applied after src_len
        // is resolved.
        let compiled_end = if let Some(e) = end {
            Some(self.compile_expr(e)?.into_int_value())
        } else {
            None
        };

        // Resolve the object to (base_ptr, length).
        let (base_ptr, src_len) = if let ExprKind::Identifier(name) = &object.kind {
            if let Some(slot) = self.variables.get(name.as_str()).copied() {
                if let BasicTypeEnum::ArrayType(at) = slot.ty {
                    (slot.ptr, i64_t.const_int(at.len() as u64, false))
                } else if self.slice_elem_types.contains_key(name.as_str()) {
                    let data_pp = self
                        .builder
                        .build_struct_gep(slice_ty, slot.ptr, 0, "rs.s.data.pp")
                        .unwrap();
                    let data = self
                        .builder
                        .build_load(ptr_ty, data_pp, "rs.s.data")
                        .unwrap()
                        .into_pointer_value();
                    let len_p = self
                        .builder
                        .build_struct_gep(slice_ty, slot.ptr, 1, "rs.s.len.p")
                        .unwrap();
                    let len = self
                        .builder
                        .build_load(i64_t, len_p, "rs.s.len")
                        .unwrap()
                        .into_int_value();
                    (data, len)
                } else if self.vec_elem_types.contains_key(name.as_str()) {
                    let vec_ty = self.vec_struct_type();
                    let data_pp = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 0, "rs.v.data.pp")
                        .unwrap();
                    let data = self
                        .builder
                        .build_load(ptr_ty, data_pp, "rs.v.data")
                        .unwrap()
                        .into_pointer_value();
                    let len_p = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 1, "rs.v.len.p")
                        .unwrap();
                    let len = self
                        .builder
                        .build_load(i64_t, len_p, "rs.v.len")
                        .unwrap()
                        .into_int_value();
                    (data, len)
                } else {
                    return Err(format!(
                        "range-slice requires Array, Vec, or Slice source; variable '{}' is neither",
                        name
                    ));
                }
            } else if self.ref_params.contains_key(name.as_str()) {
                // Ref-parameter path: pointer to inner data.
                let inner = *self.ref_params.get(name.as_str()).unwrap();
                if let BasicTypeEnum::ArrayType(at) = inner {
                    let data = self.get_data_ptr(name).unwrap();
                    (data, i64_t.const_int(at.len() as u64, false))
                } else {
                    return Err("range-slice on ref parameter requires ref Array".into());
                }
            } else {
                return Err(format!("Undefined variable '{}' in range-slice", name));
            }
        } else {
            return Err("range-slice requires a named source variable".into());
        };

        // Resolve end: open-end (`a..`, `..`) uses src_len; inclusive adds 1.
        let mut end_val = compiled_end.unwrap_or(src_len);
        if inclusive {
            end_val = self
                .builder
                .build_int_add(end_val, i64_t.const_int(1, false), "end.incl")
                .unwrap();
        }

        // Bounds check: start <= end && end <= len.
        let fn_val = self.current_fn.unwrap();
        let oob_bb = self.context.append_basic_block(fn_val, "slice.oob");
        let ok_bb = self.context.append_basic_block(fn_val, "slice.ok");
        let se_bad = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGT, start_val, end_val, "s.le.e")
            .unwrap();
        let el_bad = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGT, end_val, src_len, "e.le.len")
            .unwrap();
        let any_bad = self.builder.build_or(se_bad, el_bad, "slice.bad").unwrap();
        self.builder
            .build_conditional_branch(any_bad, oob_bb, ok_bb)
            .unwrap();
        self.builder.position_at_end(oob_bb);
        self.emit_panic("slice range out of bounds");
        self.builder.build_unreachable().unwrap();
        self.builder.position_at_end(ok_bb);

        // For an Array source, `base_ptr` is the alloca of `[N x T]` —
        // compute the element pointer via two-index GEP. For a Vec / Slice
        // source, `base_ptr` is already an element pointer, so we use a
        // one-index GEP. We distinguish by asking whether the source var is
        // an array alloca (known type) or a loaded data pointer.
        let source_is_array = if let ExprKind::Identifier(name) = &object.kind {
            if let Some(slot) = self.variables.get(name.as_str()) {
                matches!(slot.ty, BasicTypeEnum::ArrayType(_))
            } else if let Some(&inner) = self.ref_params.get(name.as_str()) {
                matches!(inner, BasicTypeEnum::ArrayType(_))
            } else {
                false
            }
        } else {
            false
        };

        let elem_ptr = if source_is_array {
            // GEP into `[N x T]*` using [0, start].
            let arr_ty = if let ExprKind::Identifier(name) = &object.kind {
                if let Some(slot) = self.variables.get(name.as_str()).copied() {
                    slot.ty
                } else if let Some(&inner) = self.ref_params.get(name.as_str()) {
                    inner
                } else {
                    return Err("range-slice: lost array type".into());
                }
            } else {
                return Err("range-slice: non-identifier array source".into());
            };
            let zero = i64_t.const_int(0, false);
            unsafe {
                self.builder
                    .build_gep(arr_ty, base_ptr, &[zero, start_val], "slice.elem.ptr")
                    .unwrap()
            }
        } else {
            // GEP into `T*` using [start].
            unsafe {
                self.builder
                    .build_gep(elem_ty, base_ptr, &[start_val], "slice.elem.ptr")
                    .unwrap()
            }
        };

        let new_len = self
            .builder
            .build_int_sub(end_val, start_val, "slice.new.len")
            .unwrap();
        Ok(self.build_slice_header(slice_ty, elem_ptr, new_len))
    }
}
