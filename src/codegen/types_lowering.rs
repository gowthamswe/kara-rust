//! AST-to-LLVM type lowering for the Codegen pass.
//!
//! Houses `llvm_type_for_type_expr` (the central `TypeExpr` → LLVM
//! type recursive walker) along with its supporting methods:
//! per-shape LLVM struct constructors (`vec_struct_type`,
//! `slice_struct_type`), primitive / float / int suffix-to-type
//! converters (`llvm_int_type_for_suffix`, `llvm_float_type_for_suffix`,
//! `const_int_for_suffix`, `const_float_for_suffix`,
//! `compile_primitive_const`), per-collection element-type extractors
//! (`extract_slice_elem_type`, `extract_vec_elem_type`,
//! `extract_map_key_name`, `extract_map_kv_types`,
//! `extract_set_elem_type`, `extract_set_elem_name`),
//! type-mangling (`mangled_type_name`), per-var registration
//! (`register_var_from_type_expr`, `register_for_loop_bindings`),
//! a handful of expression-kind recognisers used for early dispatch
//! (`is_map_new_call`, `is_set_new_call`, `is_vec_new_call`,
//! `is_string_new_call`, `is_string_binary_op`,
//! `is_runtime_introspection_call`, `first_operand_is_string`),
//! and the shared-type accessors (`is_shared_type`, `shared_heap_type`,
//! `shared_type_for_expr`).

use std::collections::HashMap;

use crate::ast::*;
use crate::token::{FloatSuffix, IntSuffix};

use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, StructType};
use inkwell::values::BasicValueEnum;
use inkwell::AddressSpace;

use super::helpers::{
    const_value_as_u32, map_kv_type_exprs, set_inner_type_expr, slice_inner_type_expr,
    vec_inner_type_expr,
};
use super::state::SharedTypeInfo;

impl<'ctx> super::Codegen<'ctx> {
    // ── Type resolution ───────────────────────────────────────────

    pub(super) fn llvm_type_for_type_expr(&self, ty: &TypeExpr) -> BasicTypeEnum<'ctx> {
        match &ty.kind {
            TypeKind::Path(path) => {
                let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if name == "Array" {
                    if let Some(arr_ty) = self.llvm_array_type(&path.generic_args) {
                        return arr_ty;
                    }
                }
                if name == "Vec" || name == "VecDeque" {
                    // `VecDeque[T]` shares `Vec[T]`'s `{ptr, len, cap}`
                    // codegen layout — see `extract_vec_elem_type` and
                    // the `Vec.new` / `VecDeque.new` arm in
                    // `compile_assoc_call`. Without this branch, the
                    // baked `struct VecDeque[T] { }` shape (an empty
                    // struct) isn't in `struct_types` either (codegen
                    // doesn't import baked stdlib items), so the
                    // fall-through reaches `llvm_type_for_name`'s
                    // unknown-name arm and returns `i64`. That would
                    // size every escaped-binding parent slot at 8
                    // bytes — but the par-branch then stores a real
                    // 24-byte vec aggregate at that slot, overflowing
                    // 16 bytes into the adjacent alloca and corrupting
                    // any neighbouring local (the Map+VecDeque
                    // co-existence repro).
                    return self.vec_struct_type().into();
                }
                if name == "Slice" {
                    return self.slice_struct_type().into();
                }
                // Map[K,V] and Set[T] are opaque heap pointers managed by the
                // karac_map_* runtime functions.
                if name == "Map" || name == "Set" || name == "SortedSet" {
                    return self.context.ptr_type(AddressSpace::default()).into();
                }
                // HTTP handler ABI trampoline (2026-05-09, F2): `Request` is
                // an opaque heap pointer wrapping the runtime's
                // `*const KaracHttpRequest`. The shim emitted at the
                // `Server.serve(handler)` dispatch site packs the FFI request
                // pointer into this slot before invoking the user handler;
                // `Request.path()` / `.method()` extract the pointer and
                // round-trip through the runtime externs.
                if name == "Request" {
                    return self.context.ptr_type(AddressSpace::default()).into();
                }
                self.llvm_type_for_name(name)
            }
            TypeKind::Tuple(elems) if elems.is_empty() => {
                // unit type → i64 zero
                self.context.i64_type().into()
            }
            TypeKind::Tuple(elems) => {
                let fields: Vec<BasicTypeEnum<'ctx>> = elems
                    .iter()
                    .map(|e| self.llvm_type_for_type_expr(e))
                    .collect();
                self.context.struct_type(&fields, false).into()
            }
            TypeKind::Ref(_) | TypeKind::MutRef(_) => {
                self.context.ptr_type(AddressSpace::default()).into()
            }
            TypeKind::MutSlice(_) => self.slice_struct_type().into(),
            _ => self.context.i64_type().into(),
        }
    }

    /// Extract the inner type from a ref/mut ref type expression.
    pub(super) fn inner_type_of_ref(&self, ty: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        match &ty.kind {
            TypeKind::Ref(inner) | TypeKind::MutRef(inner) => {
                Some(self.llvm_type_for_type_expr(inner))
            }
            _ => None,
        }
    }

    /// Lower `Array[T, N]` generic args to an LLVM `[N x T]` type.
    /// Mirrors typechecker::lower_array_type — accepts only positive integer-literal size.
    pub(super) fn llvm_array_type(
        &self,
        generic_args: &Option<Vec<GenericArg>>,
    ) -> Option<BasicTypeEnum<'ctx>> {
        let args = generic_args.as_ref()?;
        if args.len() != 2 {
            return None;
        }
        let elem_ty_expr = match &args[0] {
            GenericArg::Type(t) => t,
            GenericArg::Const(_) => return None,
        };
        let size = match &args[1] {
            GenericArg::Const(expr) => match &expr.kind {
                ExprKind::Integer(n, _) if *n >= 0 => *n as u32,
                // Const generics slice 4: const-param identifier ref
                // (`Array[T, N]` where `N` is a const-generic param
                // bound at the active monomorphization). Look up in
                // `const_subst` and recover the integer width.
                ExprKind::Identifier(name) => {
                    let cv = self.const_subst.get(name)?;
                    const_value_as_u32(cv)?
                }
                _ => return None,
            },
            // Slice 1c parser disambiguation: the generic-args parser
            // can't distinguish a type-param ref from a const-param
            // ref at parse time (no scope info), so `Array[T, N]`
            // routes N as `GenericArg::Type(Path(N))`. Recover the
            // const-param at codegen extraction.
            GenericArg::Type(te) => {
                if let TypeKind::Path(p) = &te.kind {
                    if p.segments.len() == 1 && p.generic_args.is_none() {
                        let name = &p.segments[0];
                        if let Some(cv) = self.const_subst.get(name) {
                            const_value_as_u32(cv)?
                        } else {
                            return None;
                        }
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
        };
        let elem_ty = self.llvm_type_for_type_expr(elem_ty_expr);
        Some(elem_ty.array_type(size).into())
    }

    /// Vec[T] runtime layout: `{ ptr data, i64 len, i64 capacity }`.
    pub(super) fn vec_struct_type(&self) -> StructType<'ctx> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default()).into();
        let i64_ty = self.context.i64_type().into();
        self.context.struct_type(&[ptr_ty, i64_ty, i64_ty], false)
    }

    /// True when `ty` is the runtime `{ptr, i64, i64}` shape used for
    /// `Vec[T]` and `String`. Used by the recursive-drop cleanup path to
    /// decide whether each element of an outer `Vec[Vec[…]]` needs its
    /// own data buffer freed before the outer buffer is released.
    /// Comparison is by LLVM type identity — every codegen call to
    /// `vec_struct_type()` returns the same context-uniqued struct, so
    /// pointer equality on the structs is sound.
    pub(super) fn llvm_ty_is_vec_struct(&self, ty: BasicTypeEnum<'ctx>) -> bool {
        match ty {
            BasicTypeEnum::StructType(st) => st == self.vec_struct_type(),
            _ => false,
        }
    }

    /// Slice[T] and `mut Slice[T]` runtime layout: `{ ptr data, i64 len }`.
    /// Mutability is a type-system concept — the physical layout is identical
    /// for read-only and mutable slices. 16 bytes on 64-bit platforms.
    pub(super) fn slice_struct_type(&self) -> StructType<'ctx> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default()).into();
        let i64_ty = self.context.i64_type().into();
        self.context.struct_type(&[ptr_ty, i64_ty], false)
    }

    /// Produce an LLVM integer type matching the source-level suffix.
    /// `None` defaults to `i64`. `I128`/`U128` map to LLVM `i128_type`
    /// (added 2026-05-11 alongside the `IntSize::I128`/`UIntSize::U128`
    /// typechecker extension that unblocks const generics slice 2b).
    pub(super) fn llvm_int_type_for_suffix(
        &self,
        sfx: Option<IntSuffix>,
    ) -> inkwell::types::IntType<'ctx> {
        match sfx {
            None => self.context.i64_type(),
            Some(IntSuffix::I8) | Some(IntSuffix::U8) => self.context.i8_type(),
            Some(IntSuffix::I16) | Some(IntSuffix::U16) => self.context.i16_type(),
            Some(IntSuffix::I32) | Some(IntSuffix::U32) => self.context.i32_type(),
            Some(IntSuffix::I64) | Some(IntSuffix::U64) => self.context.i64_type(),
            Some(IntSuffix::I128) | Some(IntSuffix::U128) => self.context.i128_type(),
        }
    }

    pub(super) fn llvm_float_type_for_suffix(
        &self,
        sfx: Option<FloatSuffix>,
    ) -> inkwell::types::FloatType<'ctx> {
        match sfx {
            None | Some(FloatSuffix::F64) => self.context.f64_type(),
            Some(FloatSuffix::F32) => self.context.f32_type(),
        }
    }

    pub(super) fn const_int_for_suffix(
        &self,
        n: i64,
        sfx: Option<IntSuffix>,
    ) -> inkwell::values::IntValue<'ctx> {
        let is_signed = matches!(
            sfx,
            None | Some(IntSuffix::I8)
                | Some(IntSuffix::I16)
                | Some(IntSuffix::I32)
                | Some(IntSuffix::I64)
                | Some(IntSuffix::I128)
        );
        self.llvm_int_type_for_suffix(sfx)
            .const_int(n as u64, is_signed)
    }

    pub(super) fn const_float_for_suffix(
        &self,
        f: f64,
        sfx: Option<FloatSuffix>,
    ) -> inkwell::values::FloatValue<'ctx> {
        self.llvm_float_type_for_suffix(sfx).const_float(f)
    }

    /// Lower a primitive-type associated constant (`i64.MAX`,
    /// `f64.INFINITY`, `usize.MAX`, etc.) to an LLVM constant of the
    /// matching width. The interpreter consumes the same `ConstValue`
    /// table but coerces signed / unsigned to a single `Value::Int(i64)`;
    /// codegen keeps the precise width so downstream arithmetic
    /// type-checks correctly at the LLVM level.
    pub(super) fn compile_primitive_const(
        &self,
        cv: &crate::prelude::ConstValue,
    ) -> BasicValueEnum<'ctx> {
        use crate::prelude::ConstValue::*;
        match cv {
            I8(v) => self.context.i8_type().const_int(*v as u64, true).into(),
            I16(v) => self.context.i16_type().const_int(*v as u64, true).into(),
            I32(v) => self.context.i32_type().const_int(*v as u64, true).into(),
            I64(v) => self.context.i64_type().const_int(*v as u64, true).into(),
            // const generics slice 2b — i128 / u128 use LLVM's native
            // i128 type. The const_int builder takes a u64; for 128-bit
            // values we lose the high bits — acceptable for v1's source
            // surface (parser literals are bounded to i64 / u64), and
            // round-trips clean for values that fit. A future widening
            // of `ExprKind::Integer` to carry i128 bits replaces this
            // truncation with `const_int_arbitrary_precision`.
            I128(v) => self.context.i128_type().const_int(*v as u64, true).into(),
            U8(v) => self.context.i8_type().const_int(*v as u64, false).into(),
            U16(v) => self.context.i16_type().const_int(*v as u64, false).into(),
            U32(v) => self.context.i32_type().const_int(*v as u64, false).into(),
            U64(v) => self.context.i64_type().const_int(*v, false).into(),
            U128(v) => self.context.i128_type().const_int(*v as u64, false).into(),
            // v1 is 64-bit only — usize is u64.
            Usize(v) => self.context.i64_type().const_int(*v, false).into(),
            // Float widths. `const_float` accepts an f64 input and
            // narrows for f32; for INFINITY / NEG_INFINITY / NAN this
            // round-trip is exact (the bit patterns survive the
            // f64→f32 conversion).
            F32(v) => self.context.f32_type().const_float(*v as f64).into(),
            F64(v) => self.context.f64_type().const_float(*v).into(),
            // Const generics slice 2 — Bool / Char / EnumVariant flow
            // through here only when a primitive-table constant uses
            // one of these variants (none do today). Slice 4 wires
            // const-param identifier lowering to call `compile_primitive_const`
            // for const-args at use sites; this arm prepares the surface.
            Bool(b) => self.context.bool_type().const_int(*b as u64, false).into(),
            Char(c) => self.context.i32_type().const_int(*c as u64, false).into(),
            // Fieldless-enum variant: tag-only payload as `i64` (matches
            // the existing enum-variant tag width).
            EnumVariant { discriminant, .. } => self
                .context
                .i64_type()
                .const_int(*discriminant as u64, true)
                .into(),
        }
    }

    /// Infer the slice element type from a let-binding RHS that produces
    /// a slice value. Recognizes `.as_slice()` / `.as_slice_mut()` on a
    /// known sequence variable and range-indexing `x[a..b]` on the same.
    /// Returns `None` when the RHS is not a slice-producing shape.
    pub(super) fn infer_slice_elem_from_rhs(&self, expr: &Expr) -> Option<BasicTypeEnum<'ctx>> {
        match &expr.kind {
            ExprKind::MethodCall { object, method, .. }
                if method == "as_slice" || method == "as_slice_mut" =>
            {
                self.infer_elem_from_source(object)
            }
            ExprKind::Index { object, index } => {
                if matches!(&index.kind, ExprKind::Range { .. }) {
                    self.infer_elem_from_source(object)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Look up the element LLVM type of a known sequence variable (Array,
    /// Vec, or Slice).
    pub(super) fn infer_elem_from_source(&self, object: &Expr) -> Option<BasicTypeEnum<'ctx>> {
        let name = if let ExprKind::Identifier(n) = &object.kind {
            n
        } else {
            return None;
        };
        if let Some(slot) = self.variables.get(name.as_str()) {
            if let BasicTypeEnum::ArrayType(at) = slot.ty {
                return Some(at.get_element_type());
            }
        }
        if let Some(&elem) = self.slice_elem_types.get(name.as_str()) {
            return Some(elem);
        }
        if let Some(&elem) = self.vec_elem_types.get(name.as_str()) {
            return Some(elem);
        }
        if let Some(&BasicTypeEnum::ArrayType(at)) = self.ref_params.get(name.as_str()) {
            return Some(at.get_element_type());
        }
        None
    }

    /// Extract the element LLVM type from a `Slice[T]` or `mut Slice[T]`
    /// type expression.
    pub(super) fn extract_slice_elem_type(&self, te: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        match &te.kind {
            TypeKind::Path(path) => {
                let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if name != "Slice" {
                    return None;
                }
                let args = path.generic_args.as_ref()?;
                if args.len() != 1 {
                    return None;
                }
                match &args[0] {
                    GenericArg::Type(t) => Some(self.llvm_type_for_type_expr(t)),
                    GenericArg::Const(_) => None,
                }
            }
            TypeKind::MutSlice(element) => Some(self.llvm_type_for_type_expr(element)),
            _ => None,
        }
    }

    /// Extract the element LLVM type from a `Vec[T]` type expression.
    pub(super) fn extract_vec_elem_type(&self, te: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        if let TypeKind::Path(path) = &te.kind {
            let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
            // `VecDeque[T]` shares `Vec[T]`'s `{ptr, len, cap}` codegen
            // layout — front-end ops (`push_front` / `pop_front`)
            // translate to memmove-shifted insert/remove at index 0
            // inside `compile_vec_method`. The mirror means a
            // `let q: VecDeque[i64] = ...` binding registers under
            // `vec_elem_types` and dispatches through the existing
            // Vec method-call surface unchanged.
            if name == "Vec" || name == "VecDeque" {
                if let Some(args) = &path.generic_args {
                    if let Some(GenericArg::Type(elem_te)) = args.first() {
                        return Some(self.llvm_type_for_type_expr(elem_te));
                    }
                }
            }
        }
        None
    }

    pub(super) fn is_string_type_expr(&self, te: &TypeExpr) -> bool {
        if let TypeKind::Path(path) = &te.kind {
            path.segments.first().map(|s| s.as_str()) == Some("String")
        } else {
            false
        }
    }

    /// Extract the key type name string from a `Map[K, V]` type expression.
    /// Returns a canonical mangled name suitable for `karac_hash_<name>` —
    /// path segment for named types, `tuple_T1_T2_…_Tn` for tuples (recursive).
    pub(super) fn extract_map_key_name(te: &TypeExpr) -> Option<String> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Map") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if let Some(GenericArg::Type(k_te)) = args.first() {
                return Some(Self::mangled_type_name(k_te));
            }
        }
        None
    }

    /// Produce a canonical, deterministic name for a `TypeExpr` suitable for
    /// use as a per-type function suffix (`karac_hash_<name>`, `karac_eq_<name>`).
    /// Path types collapse to their head segment; tuples mangle recursively as
    /// `tuple_T1_T2_…_Tn`. Unsupported shapes fall back to "unknown" — the
    /// typechecker's `K: Hash + Eq` enforcement prevents codegen from ever
    /// reaching such a key type.
    pub(super) fn mangled_type_name(te: &TypeExpr) -> String {
        match &te.kind {
            TypeKind::Path(p) => p
                .segments
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".to_string()),
            TypeKind::Tuple(elems) if elems.is_empty() => "unit".to_string(),
            TypeKind::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(Self::mangled_type_name).collect();
                format!("tuple_{}", parts.join("_"))
            }
            _ => "unknown".to_string(),
        }
    }

    /// Extract (K, V) LLVM types from a `Map[K, V]` type expression.
    pub(super) fn extract_map_kv_types(
        &self,
        te: &TypeExpr,
    ) -> Option<(BasicTypeEnum<'ctx>, BasicTypeEnum<'ctx>)> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Map") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if args.len() != 2 {
                return None;
            }
            let k = match &args[0] {
                GenericArg::Type(t) => self.llvm_type_for_type_expr(t),
                _ => return None,
            };
            let v = match &args[1] {
                GenericArg::Type(t) => self.llvm_type_for_type_expr(t),
                _ => return None,
            };
            Some((k, v))
        } else {
            None
        }
    }

    /// Extract the element LLVM type from a `Set[T]` type expression.
    /// Mirror of `extract_map_kv_types` for the single-type-parameter Set.
    pub(super) fn extract_set_elem_type(&self, te: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Set") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if args.is_empty() {
                return None;
            }
            if let GenericArg::Type(t) = &args[0] {
                return Some(self.llvm_type_for_type_expr(t));
            }
        }
        None
    }

    /// Extract the element shallow type-name (e.g. `"i64"`, `"String"`) from
    /// a `Set[T]` type expression. Used to drive hash/eq fn selection.
    /// Mirror of `extract_map_key_name`.
    pub(super) fn extract_set_elem_name(te: &TypeExpr) -> Option<String> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Set") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if let Some(GenericArg::Type(elem_te)) = args.first() {
                return Some(Self::mangled_type_name(elem_te));
            }
        }
        None
    }

    /// Register a variable's collection-type metadata in the side-tables
    /// driven off a Kāra `TypeExpr`. Mirrors the let-statement site at
    /// `compile_stmt(StmtKind::Let)` so for-loop bindings can inherit the
    /// same registrations from the source's stored element `TypeExpr`.
    ///
    /// Populates whichever subset of `vec_elem_types` / `slice_elem_types` /
    /// `map_key_types` / `map_val_types` / `map_key_type_names` /
    /// `var_elem_type_exprs` / `map_key_type_exprs` / `set_elem_types` /
    /// `set_elem_type_names` / `set_elem_type_exprs` matches the `TypeExpr`
    /// shape; primitives (and other shapes we don't track) are no-ops.
    pub(super) fn register_var_from_type_expr(&mut self, var_name: &str, te: &TypeExpr) {
        if let Some(elem_ty) = self.extract_vec_elem_type(te) {
            self.vec_elem_types.insert(var_name.to_string(), elem_ty);
            if let Some(inner) = vec_inner_type_expr(te) {
                self.var_elem_type_exprs.insert(var_name.to_string(), inner);
            }
            return;
        }
        if self.is_string_type_expr(te) {
            self.vec_elem_types
                .insert(var_name.to_string(), self.context.i8_type().into());
            self.string_vars.insert(var_name.to_string());
            return;
        }
        if let Some(elem_ty) = self.extract_slice_elem_type(te) {
            self.slice_elem_types.insert(var_name.to_string(), elem_ty);
            if let Some(inner) = slice_inner_type_expr(te) {
                self.var_elem_type_exprs.insert(var_name.to_string(), inner);
            }
            return;
        }
        if let Some((k_ty, v_ty)) = self.extract_map_kv_types(te) {
            self.map_key_types.insert(var_name.to_string(), k_ty);
            self.map_val_types.insert(var_name.to_string(), v_ty);
            if let Some(k_name) = Self::extract_map_key_name(te) {
                self.map_key_type_names.insert(var_name.to_string(), k_name);
            }
            if let Some((k_te, v_te)) = map_kv_type_exprs(te) {
                self.map_key_type_exprs.insert(var_name.to_string(), k_te);
                self.var_elem_type_exprs.insert(var_name.to_string(), v_te);
            }
            return;
        }
        if let Some(elem_ty) = self.extract_set_elem_type(te) {
            self.set_elem_types.insert(var_name.to_string(), elem_ty);
            if let Some(elem_name) = Self::extract_set_elem_name(te) {
                self.set_elem_type_names
                    .insert(var_name.to_string(), elem_name);
            }
            if let Some(elem_te) = set_inner_type_expr(te) {
                self.set_elem_type_exprs
                    .insert(var_name.to_string(), elem_te);
            }
            return;
        }
        // Bare user-type names (struct / shared struct / enum) — register
        // `var_type_names` so `field_index_for` / `shared_type_for_expr`
        // can resolve `.field` accesses on the binding. Without this, a
        // for-loop iterating `Vec[Node] / Vec[N]` binds `x` into the
        // value/pointer slot but the type is invisible to downstream
        // field-access codegen, which then silently returns `i64 0` for
        // every `x.field` read (cond_simple bug, 2026-05-16). Mirrors
        // `compile_stmt(StmtKind::Let)`'s `type_hint` registration path
        // (stmts.rs ~line 730) so for-loop bindings see the same
        // type-name visibility as a `let x: Foo = ...` binding.
        if let TypeKind::Path(path) = &te.kind {
            if let Some(seg) = path.segments.last().cloned() {
                if self.struct_types.contains_key(seg.as_str())
                    || self.shared_types.contains_key(seg.as_str())
                    || self.enum_layouts.contains_key(seg.as_str())
                {
                    self.var_type_names.insert(var_name.to_string(), seg);
                }
            }
        }
    }

    /// Register collection side-tables for the bindings produced by a
    /// for-loop's destructuring pattern, using the source variable's
    /// stored element `TypeExpr`. Without this, `for s in vec_of_strings`
    /// binds `s` only in `self.variables` — method dispatch in
    /// `compile_expr_method_call` then misses the Vec/Slice/Map side-table
    /// check and falls through to the silent-`0` default.
    pub(super) fn register_for_loop_bindings(&mut self, pattern: &Pattern, source_var: &str) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                if let Some(elem_te) = self.var_elem_type_exprs.get(source_var).cloned() {
                    self.register_var_from_type_expr(name, &elem_te);
                }
            }
            // `for (k, v) in m` — only legal tuple iteration shape today
            // (Map). `for (a, b) in vec_of_tuples` would fall through; the
            // tuple-element-classification follow-up would extend this arm.
            PatternKind::Tuple(pats) if pats.len() == 2 => {
                if let PatternKind::Binding(k_name) = &pats[0].kind {
                    if let Some(k_te) = self.map_key_type_exprs.get(source_var).cloned() {
                        self.register_var_from_type_expr(k_name, &k_te);
                    }
                }
                if let PatternKind::Binding(v_name) = &pats[1].kind {
                    if let Some(v_te) = self.var_elem_type_exprs.get(source_var).cloned() {
                        self.register_var_from_type_expr(v_name, &v_te);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn is_map_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "Map" && segments[1] == "new";
            }
        }
        false
    }

    pub(super) fn is_set_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "Set" && segments[1] == "new";
            }
        }
        false
    }

    pub(super) fn is_vec_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "Vec" && segments[1] == "new";
            }
        }
        false
    }

    /// Debugger Contract slice 5: `Runtime.list_par_blocks()` /
    /// `Runtime.list_tasks()` return `Vec[ParBlockInfo]` /
    /// `Vec[TaskInfo]`. Used by the let-binding registration to set up
    /// `vec_elem_types` so subsequent `.len()` / `.is_empty()` dispatches
    /// through `compile_vec_method`.
    pub(super) fn is_runtime_introspection_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2
                    && segments[0] == "Runtime"
                    && (segments[1] == "list_par_blocks" || segments[1] == "list_tasks");
            }
        }
        false
    }

    pub(super) fn is_string_binary_op(&self, expr: &Expr) -> bool {
        // Source-form `a + b` (pre-lowering).
        if let ExprKind::Binary {
            op: BinOp::Add,
            left,
            ..
        } = &expr.kind
        {
            return self.first_operand_is_string(left);
        }
        // Lowered form `Call(Path(["String", "add"]), [a, b])` — produced by
        // the operator lowering pass. Also recognize String + String here.
        if let ExprKind::Call { callee, args } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                if segments.len() == 2 && segments[0] == "String" && segments[1] == "add" {
                    if let Some(first) = args.first() {
                        return self.first_operand_is_string(&first.value);
                    }
                }
            }
        }
        false
    }

    /// Helper: is this expression a string literal or a known string variable?
    pub(super) fn first_operand_is_string(&self, expr: &Expr) -> bool {
        if matches!(&expr.kind, ExprKind::StringLit(_)) {
            return true;
        }
        if let ExprKind::Identifier(name) = &expr.kind {
            return self
                .vec_elem_types
                .get(name.as_str())
                .map(|t| t.is_int_type() && t.into_int_type().get_bit_width() == 8)
                .unwrap_or(false);
        }
        false
    }

    pub(super) fn is_string_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "String" && segments[1] == "new";
            }
        }
        false
    }

    /// Look up the Vec element type for a variable, defaulting to i64.
    pub(super) fn vec_elem_type_for_var(&self, name: &str) -> BasicTypeEnum<'ctx> {
        self.vec_elem_types
            .get(name)
            .copied()
            .unwrap_or_else(|| self.context.i64_type().into())
    }

    pub(super) fn llvm_type_for_name(&self, name: &str) -> BasicTypeEnum<'ctx> {
        // Active monomorphization substitution takes priority.
        if let Some(&ty) = self.type_subst.get(name) {
            return ty;
        }
        match name {
            "i8" | "u8" => self.context.i8_type().into(),
            "i16" | "u16" => self.context.i16_type().into(),
            "i32" | "u32" => self.context.i32_type().into(),
            "i64" | "u64" | "isize" | "usize" => self.context.i64_type().into(),
            "f32" => self.context.f32_type().into(),
            "f64" => self.context.f64_type().into(),
            "bool" => self.context.bool_type().into(),
            // `char` is a Unicode scalar value — 32 bits, same LLVM
            // type as `i32` / `u32`. Without this arm, `char` falls
            // through to the i64 default below, which caused
            // `Map[char, V].new()` to allocate `key_size = 8` byte
            // slots for 4-byte char keys; the runtime would then
            // copy 4 bytes of stack-neighbor garbage alongside
            // every char into the kv table. Hash/eq only read the
            // first 4 bytes so it worked accidentally as long as
            // the garbage was consistent between insert and get
            // sites — Slice 1b.3 sidestepped that fragility with a
            // forced alloca-order in the get arm; this fix
            // addresses the root cause. Surfaced by the
            // monomorphized-collections Slice 2 investigation.
            "char" => self.context.i32_type().into(),
            // `VecDeque` shares `Vec[T]`'s `{ptr, len, cap}` layout (see
            // the parallel arm in `llvm_type_for_type_expr`). The baked
            // `struct VecDeque[T] { }` isn't in `struct_types` from
            // codegen's perspective (baked stdlib items aren't fed in),
            // so calls that reach `llvm_type_for_name` directly without
            // generic args would otherwise fall to the i64 default.
            "String" | "str" | "VecDeque" | "Vec" => self.vec_struct_type().into(),
            name => {
                // Shared types are heap-allocated pointers.
                if self.shared_types.contains_key(name) {
                    return self.context.ptr_type(AddressSpace::default()).into();
                }
                if let Some(st) = self.struct_types.get(name) {
                    (*st).into()
                } else if let Some(layout) = self.enum_layouts.get(name) {
                    // Enum types are represented as tagged-union structs.
                    layout.llvm_type.into()
                } else {
                    self.context.i64_type().into()
                }
            }
        }
    }

    pub(super) fn llvm_return_type(&self, ty: &Option<TypeExpr>) -> Option<BasicTypeEnum<'ctx>> {
        let te = ty.as_ref()?;
        match &te.kind {
            TypeKind::Path(path) => {
                let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if name.is_empty() {
                    return None;
                }
                // Delegate to llvm_type_for_type_expr so generic types
                // (Array[T, N], Vec[T], Slice[T], Map[K,V], …) are honored
                // — bare llvm_type_for_name drops generic args.
                Some(self.llvm_type_for_type_expr(te))
            }
            TypeKind::Tuple(elems) if elems.is_empty() => None,
            _ => Some(self.llvm_type_for_type_expr(te)),
        }
    }

    pub(super) fn llvm_param_type(&self, param: &Param) -> BasicMetadataTypeEnum<'ctx> {
        BasicMetadataTypeEnum::from(self.llvm_type_for_type_expr(&param.ty))
    }

    // ── Shared type helpers ─────────────────────────────────────────

    /// Check if a type name refers to a shared (RC) type.
    #[allow(dead_code)]
    pub(super) fn is_shared_type(&self, name: &str) -> bool {
        self.shared_types.contains_key(name)
    }

    /// Get the heap struct type for a shared type, if it exists.
    #[allow(dead_code)]
    pub(super) fn shared_heap_type(&self, name: &str) -> Option<StructType<'ctx>> {
        self.shared_types.get(name).map(|info| info.heap_type)
    }

    /// If the expression refers to a variable of shared type, return the type name and info.
    pub(super) fn shared_type_for_expr(
        &self,
        expr: &Expr,
    ) -> Option<(String, SharedTypeInfo<'ctx>)> {
        if let ExprKind::Identifier(var_name) = &expr.kind {
            if let Some(type_name) = self.var_type_names.get(var_name.as_str()) {
                if let Some(info) = self.shared_types.get(type_name.as_str()) {
                    return Some((type_name.clone(), info.clone()));
                }
            }
        }
        None
    }

    /// If the expression is a call-shaped node (`Call`, `MethodCall`,
    /// or 2-segment `Path` assoc-call) whose static return type names a
    /// known shared struct / enum, return that type name and info.
    /// Companion to `shared_type_for_expr` which only handles variable
    /// references. Used by `compile_field_access` for the call-chain
    /// shape `helper().field` — see bug #8 (call-chain field access on
    /// shared-struct return).
    pub(super) fn shared_type_for_call_like(
        &self,
        expr: &Expr,
    ) -> Option<(String, SharedTypeInfo<'ctx>)> {
        let fn_name: String = match &expr.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Identifier(n) => n.clone(),
                ExprKind::Path { segments, .. } if segments.len() == 2 => {
                    format!("{}.{}", segments[0], segments[1])
                }
                ExprKind::Path { segments, .. } if segments.len() == 1 => segments[0].clone(),
                _ => return None,
            },
            // Method-call return-type recovery isn't wired through the
            // existing side-tables (`method_callee_types` keys to the
            // callee identity, not its return type). Defer to a future
            // slice; the bug-#8 minimal repro uses a free-fn call.
            _ => return None,
        };
        let type_name = self.fn_return_type_names.get(&fn_name)?.clone();
        let info = self.shared_types.get(&type_name)?.clone();
        Some((type_name, info))
    }

    /// Resolve the heap layout for a Map variable's *value* when V is a
    /// shared struct / shared enum. Returns `None` when V is anything
    /// else (primitive, Vec, String, owned struct, …). Used by
    /// `track_map_var` / cleanup-action wiring to decide whether the
    /// per-bucket rc_dec walk in `emit_map_shared_half_rc_dec_walk` needs
    /// to fire at scope exit. Source of truth is the `TypeExpr` stored
    /// in `var_elem_type_exprs[var_name]` (the value-side TypeExpr
    /// recorded at let-binding registration) — its head segment is the
    /// shared type's surface name, looked up in `shared_types`.
    pub(super) fn map_val_shared_heap_type_for(&self, var_name: &str) -> Option<StructType<'ctx>> {
        let v_te = self.var_elem_type_exprs.get(var_name)?;
        let head = match &v_te.kind {
            TypeKind::Path(p) => p.segments.first()?.as_str(),
            _ => return None,
        };
        let info = self.shared_types.get(head)?;
        Some(info.heap_type)
    }

    /// Resolve the heap layout for a Map / Set variable's *key* when
    /// K is a shared struct / shared enum. Mirrors
    /// `map_val_shared_heap_type_for` on the K side. For `Set[shared
    /// T]`, the element T occupies the key half of the underlying
    /// `Map[T, ()]` bucket — `set_elem_type_exprs` provides the
    /// TypeExpr, parallel to `map_key_type_exprs` for Maps. Returns
    /// `None` when K is anything else (primitive, Vec, String, owned
    /// struct, …); the `FreeMapHandle` cleanup then skips the
    /// key-side rc_dec walk.
    pub(super) fn map_key_shared_heap_type_for(&self, var_name: &str) -> Option<StructType<'ctx>> {
        let k_te = self
            .map_key_type_exprs
            .get(var_name)
            .or_else(|| self.set_elem_type_exprs.get(var_name))?;
        Self::shared_heap_type_for_type_expr_with(&self.shared_types, k_te)
    }

    /// Resolve the heap layout for an arbitrary `TypeExpr` whose head
    /// is a shared struct / shared enum. Returns `None` when the head
    /// isn't a `Path` (tuple, ref, fn type, …) or when the head name
    /// isn't a known shared type. Used by struct-field drop synthesis
    /// to inspect a `Map[K, sharedV]` / `Set[sharedT]` field's K / V
    /// halves without needing a per-binding side-table — the field
    /// type carries the K/V `TypeExpr`s directly.
    pub(super) fn shared_heap_type_for_type_expr(&self, te: &TypeExpr) -> Option<StructType<'ctx>> {
        Self::shared_heap_type_for_type_expr_with(&self.shared_types, te)
    }

    fn shared_heap_type_for_type_expr_with(
        shared_types: &HashMap<String, SharedTypeInfo<'ctx>>,
        te: &TypeExpr,
    ) -> Option<StructType<'ctx>> {
        let head = match &te.kind {
            TypeKind::Path(p) => p.segments.first()?.as_str(),
            _ => return None,
        };
        let info = shared_types.get(head)?;
        Some(info.heap_type)
    }
}
