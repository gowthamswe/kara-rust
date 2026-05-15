// src/typechecker.rs

//! Type checking for the Kāra language.
//!
//! Walks the AST with resolved names, builds a type environment from
//! top-level definitions, then type-checks function bodies. Produces
//! typed expression info and diagnostics.

// The const-expression evaluator's `Result<ConstValue, ConstEvalError>`
// shape carries `Type` / `ConstValue` payloads in the error variants,
// pushing the Err variant size above clippy's default threshold. Boxing
// every error site would clutter the eval-and-emit happy path; the
// extra stack width is acceptable since const-eval Results don't fan out.
#![allow(clippy::result_large_err)]

use crate::ast::*;
use crate::resolver::{ResolveResult, SpanKey, SymbolKind};
use crate::token::{FloatSuffix, IntSuffix, Span};
use std::collections::{HashMap, HashSet};

// ── Internal Type Representation ────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int(IntSize),
    UInt(UIntSize),
    Float(FloatSize),
    Bool,
    Char,
    Str,
    Unit,
    Never,

    Tuple(Vec<Type>),
    Array {
        element: Box<Type>,
        /// Const generics slice 3 (fork G4): `size` widened from `usize`
        /// to `ConstArg` so the Type carries the const-arg shape through
        /// inference. Literal sizes flow through `ConstArg::Literal(n)`;
        /// const-param references flow through `ConstArg::ConstParam(name)`;
        /// the call-site solver mints `ConstArg::ConstVar(id)` and resolves
        /// to one of the other variants. Codegen and interpreter consumers
        /// reach for the literal via `ConstArg::as_literal`.
        size: ConstArg,
    },
    Slice {
        element: Box<Type>,
        mutable: bool,
    },

    /// A user-defined struct or enum, referenced by name.
    Named {
        name: String,
        args: Vec<Type>,
    },

    /// A `shared struct S { ... }` value type — RC-tracked struct with
    /// reference semantics. Carries the struct name only; shared structs
    /// are non-generic at v1 (no `shared struct S[T]`) per design.md
    /// § Part 5: Shared Types. Distinct from `Type::Named { name: "S" }`
    /// so consumers can match shared-ness off the type directly without
    /// consulting `StructDef.is_shared` in the item table.
    Shared(String),

    /// `Rc[T]` — explicit reference-counted wrapper, single-task only.
    /// Not assignable to `Arc[T]`; the `Rc → Arc` migration story is
    /// manual (per design.md § RC integration). The auto-promotion in
    /// `OwnershipChecker::promote_rc_to_arc` rewrites the value site,
    /// not the type, so the typechecker compat rule and the
    /// ownership-checker's promotion are orthogonal.
    Rc(Box<Type>),

    /// `Arc[T]` — atomically-reference-counted wrapper, cross-task safe.
    /// Not assignable to `Rc[T]`; see `Type::Rc` for the migration note.
    Arc(Box<Type>),

    Function {
        params: Vec<Type>,
        return_type: Box<Type>,
    },

    /// A once-callable closure type: a closure that consumes a captured
    /// owned non-Copy value and therefore can only be invoked one time.
    /// Distinct from `Function` because `OnceFunction` cannot substitute
    /// into a `Function` slot (or a `ref Function` slot) — the slot would
    /// permit multiple invocations, which the once-callable contract
    /// forbids. Identity-compatible with itself only at this stage; later
    /// rounds may add a `Function ⇒ OnceFunction` widening at slot
    /// boundaries.
    OnceFunction {
        params: Vec<Type>,
        return_type: Box<Type>,
    },

    Ref(Box<Type>),
    MutRef(Box<Type>),
    Weak(Box<Type>),
    Pointer {
        is_mut: bool,
        inner: Box<Type>,
    },

    TypeParam(String),
    TypeVar(TypeVarId),

    /// `T.Item` — an associated type projection. `param` is the generic type
    /// parameter name (e.g. `"I"`); `assoc` is the associated type name
    /// (e.g. `"Item"`). Resolved to a concrete type when the parameter is
    /// instantiated via `resolve_assoc_projections`.
    AssocProjection {
        param: String,
        assoc: String,
    },

    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntSize {
    I8,
    I16,
    I32,
    I64,
    I128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UIntSize {
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatSize {
    F32,
    F64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeVarId(pub u32);

/// Const-arg metavariable id (const generics slice 3 — fork G1). Mirrors
/// `TypeVarId` but for const-arg unification at call sites. Minted by
/// `instantiate_signature_with_fresh_vars` per unique const-param name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConstVarId(pub u32);

/// Const-generic argument carried inside a `Type` (currently `Type::Array.size`;
/// slice 3 ships this surface for `Type::Array` only — `Type::Named.args`
/// const-arg representation is the deferred-F carve-out per phase-5).
///
/// At type lowering time the parser's `GenericArg::Const(Expr)` payload
/// becomes either `Literal(i64)` (when the const-arg is a literal /
/// fold-through-evaluator) or `ConstParam(name)` (when the const-arg
/// references a const-generic param in scope). The inference solver
/// substitutes `ConstParam → ConstVar(id)` at signature instantiation;
/// `resolve_type_vars` walks back through the const_substitutions map
/// to swap `ConstVar` for its bound value.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstArg {
    Literal(i64),
    ConstParam(String),
    ConstVar(ConstVarId),
}

impl ConstArg {
    /// Extract the integer literal value if this is a `Literal` variant.
    /// Returns `None` for `ConstParam` / `ConstVar` (which carry
    /// unresolved or symbolic references). Used by codegen / interpreter
    /// / typechecker consumers that care about concrete sizes.
    pub fn as_literal(&self) -> Option<i64> {
        match self {
            ConstArg::Literal(n) => Some(*n),
            _ => None,
        }
    }

    /// Extract the integer literal value as a `usize`. Returns `None`
    /// for negative literals or non-literal variants. Used at places
    /// where the legacy `Type::Array.size: usize` representation was
    /// consumed directly (length checks, repeat-literal arity checks).
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            ConstArg::Literal(n) if *n >= 0 => usize::try_from(*n).ok(),
            _ => None,
        }
    }
}

/// Borrow form of a match scrutinee, captured once at match entry and
/// propagated transitively into every sub-pattern via
/// `check_pattern_against`. The classifier strips a single outer
/// `ref` / `mut ref` so variant / struct / tuple dispatch keeps matching
/// the unwrapped shape; each leaf binding's type is then re-wrapped
/// through `wrap_binding_ty` so the bindings carry the borrow back —
/// design.md § Match Arm Binding Modes.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum ScrutineeMode {
    Owned,
    Ref,
    MutRef,
}

impl ScrutineeMode {
    /// Classify `scrut_ty` into `(mode, dispatch_ty)`. A single layer of
    /// `Type::Ref` / `Type::MutRef` is stripped — deeper nesting is rare
    /// (Kāra has no `&&T`-style syntax; `ref ref T` only arises from
    /// generic instantiation) and the outer-most borrow already governs
    /// what arm bindings may do, so the simpler shallow strip suffices.
    pub(crate) fn classify(scrut_ty: &Type) -> (Self, &Type) {
        match scrut_ty {
            Type::Ref(inner) => (ScrutineeMode::Ref, inner.as_ref()),
            Type::MutRef(inner) => (ScrutineeMode::MutRef, inner.as_ref()),
            _ => (ScrutineeMode::Owned, scrut_ty),
        }
    }

    /// Wrap a leaf binding's declared type with the appropriate borrow
    /// form for this mode. `Owned` is the identity. Types that are
    /// **already** a borrow shape (`Ref`, `MutRef`, `Slice` /
    /// `mut Slice`) are left alone so a `ref Foo` field through a
    /// `ref Container` scrutinee stays `ref Foo` instead of becoming
    /// `ref ref Foo`, and a struct field declared `Slice[T]`
    /// (immutable) is not silently elevated to `mut Slice[T]` by a
    /// `mut ref Container` scrutinee — the field's declared mutability
    /// is its own ceiling. Mutability propagation for **slice rest
    /// bindings** (Array[T, K] / Slice[T] freshly synthesized from a
    /// `mut ref Vec[T]` scrutinee per design.md § Slice patterns —
    /// Mutability propagation) is handled at the Slice arm of
    /// `check_pattern_against`, not here.
    pub(crate) fn wrap_binding_ty(self, ty: Type) -> Type {
        match self {
            ScrutineeMode::Owned => ty,
            ScrutineeMode::Ref => match ty {
                Type::Ref(_) | Type::MutRef(_) | Type::Slice { .. } => ty,
                _ => Type::Ref(Box::new(ty)),
            },
            ScrutineeMode::MutRef => match ty {
                Type::Ref(_) | Type::MutRef(_) | Type::Slice { .. } => ty,
                _ => Type::MutRef(Box::new(ty)),
            },
        }
    }
}

/// Const generics slice 3 sub-step (g): substitute `ConstArg::ConstParam(name)`
/// against the typechecker's `SubstValue` map (slice 1 fork F1). When the
/// map binds `name → Const(cv)`, the result becomes a `Literal` carrying
/// the resolved value coerced through `i64`. `Literal` and `ConstVar`
/// pass through unchanged.
fn substitute_const_arg(arg: &ConstArg, subs: &HashMap<String, SubstValue>) -> ConstArg {
    match arg {
        ConstArg::ConstParam(name) => match subs.get(name) {
            Some(SubstValue::Const(cv)) => match const_value_to_i64(cv) {
                Some(n) => ConstArg::Literal(n),
                None => arg.clone(),
            },
            _ => arg.clone(),
        },
        _ => arg.clone(),
    }
}

/// Best-effort coercion of a `ConstValue` to `i64` for the slice 3
/// `ConstArg::Literal` shape. Integer variants widen / narrow into i64;
/// bool / char / enum-variant become their underlying numeric (false=0,
/// true=1, char-codepoint, enum-discriminant); float variants return None.
fn const_value_to_i64(cv: &crate::prelude::ConstValue) -> Option<i64> {
    use crate::prelude::ConstValue::*;
    match cv {
        I8(v) => Some(*v as i64),
        I16(v) => Some(*v as i64),
        I32(v) => Some(*v as i64),
        I64(v) => Some(*v),
        I128(v) => i64::try_from(*v).ok(),
        U8(v) => Some(*v as i64),
        U16(v) => Some(*v as i64),
        U32(v) => Some(*v as i64),
        U64(v) => i64::try_from(*v).ok(),
        U128(v) => i64::try_from(*v).ok(),
        Usize(v) => i64::try_from(*v).ok(),
        Bool(b) => Some(*b as i64),
        Char(c) => Some(*c as i64),
        EnumVariant { discriminant, .. } => Some(*discriminant),
        F32(_) | F64(_) => None,
    }
}

/// User-facing rendering of a `ConstArg` for diagnostic messages.
/// `Literal(4)` → `"4"`; `ConstParam(N)` → `"N"`; `ConstVar(id)` →
/// `"?C{id}"` (parallel to `?M{id}` for type metavars).
pub fn const_arg_display(arg: &ConstArg) -> String {
    match arg {
        ConstArg::Literal(n) => n.to_string(),
        ConstArg::ConstParam(name) => name.clone(),
        ConstArg::ConstVar(id) => format!("?C{}", id.0),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VariantTypeInfo {
    Unit,
    Tuple(Vec<Type>),
    Struct(Vec<(String, Type)>),
}

/// Generic-substitution payload (F1 — Const generics slice 1). A single
/// `HashMap<String, SubstValue>` carries both type and const substitutions
/// so the typechecker's structural substitution and the call-site solver
/// can flow type-args and literal const-args through the same context.
/// Slice 2's const-expression evaluator and slice 4's codegen consume the
/// `Const` variant; slice 1 binds only literal const-args via step (e).
#[derive(Debug, Clone)]
pub enum SubstValue {
    Type(Type),
    Const(crate::prelude::ConstValue),
}

impl SubstValue {
    /// Extract the inner `Type` if this is a `Type` substitution; `None`
    /// for `Const`. Used by structural type substitution paths that should
    /// ignore const-arg bindings (e.g. `substitute_type_params` walking a
    /// `Type::TypeParam(name)`).
    pub fn as_type(&self) -> Option<&Type> {
        match self {
            SubstValue::Type(t) => Some(t),
            SubstValue::Const(_) => None,
        }
    }
}

// ── Const-expression evaluator ──────────────────────────────────
//
// Const generics slice 2 (2026-05-11). `eval_const_expr` evaluates a
// const-expression `Expr` against a target `Type`, returning either a
// `ConstValue` (the resolved compile-time value) or a `ConstEvalError`
// describing why evaluation failed. Used by:
// - `lower_array_type` for non-literal `Array[T, N + 1]` const-args
// - default-parameter-value validation (retires `find_non_const_span`)
// - slice 3's where-clause discharge engine (entry point only at this
//   slice; the actual call lands when slice 3 ships)

/// Failure modes for `eval_const_expr`. Each variant carries enough
/// context for `emit_const_eval_error` to render a focused diagnostic
/// at the spec-mandated span.
#[derive(Debug, Clone)]
pub enum ConstEvalError {
    /// Expression shape isn't recognized as constant-evaluable (e.g. a
    /// function call, closure, method call, or a non-literal Path).
    NonConstShape(Span),
    /// `checked_*` returned `None` for a binary integer operation.
    Overflow {
        op: BinOp,
        lhs: crate::prelude::ConstValue,
        rhs: crate::prelude::ConstValue,
        span: Span,
    },
    /// `checked_neg` returned `None` for a unary operation.
    UnaryOverflow {
        op: UnaryOp,
        operand: crate::prelude::ConstValue,
        span: Span,
    },
    /// `/` or `%` with a literal-zero right-hand operand.
    DivByZero { span: Span },
    /// Integer literal doesn't fit in the inferred target type.
    OutOfRange {
        value: i128,
        target_ty: Type,
        span: Span,
    },
    /// A `ConstDecl` reference whose value type doesn't match the
    /// surrounding context.
    TypeMismatch {
        expected: Type,
        found: Type,
        span: Span,
    },
    /// Identifier reference that didn't resolve to a const-param,
    /// `ConstDecl`, or known fieldless-enum variant.
    UndefinedConst { name: String, span: Span },
    /// Arithmetic operator applied to a non-integer operand (e.g.
    /// `'a' + 'b'`, `true + false`).
    ArithOnNonInt { ty: Type, op: BinOp, span: Span },
    /// Logical operator (`and` / `or` / `!`) applied to a non-bool
    /// operand.
    LogicalOnNonBool { ty: Type, op: BinOp, span: Span },
    /// Comparison between two values whose `ConstValue` discriminants
    /// don't share a comparable family (int/int, bool/bool, char/char,
    /// enum/enum from the same enum type).
    CompareIncomparable {
        lhs_ty: Type,
        rhs_ty: Type,
        span: Span,
    },
    /// `ConstDecl` self-reference cycle: `const A: i64 = B; const B: i64 = A;`
    CyclicConstDef { chain: Vec<String>, span: Span },
}

/// Extract a non-negative array size from a `ConstValue`. Returns `None`
/// for non-integer variants, negative integers, or values that would
/// exceed `usize` range. Used by `lower_array_type` when routing
/// non-literal const-arg expressions through the const-expression
/// evaluator (slice 2).
fn const_value_to_array_size(cv: &crate::prelude::ConstValue) -> Option<usize> {
    use crate::prelude::ConstValue::*;
    let n: i128 = match cv {
        I8(v) => *v as i128,
        I16(v) => *v as i128,
        I32(v) => *v as i128,
        I64(v) => *v as i128,
        I128(v) => *v,
        U8(v) => *v as i128,
        U16(v) => *v as i128,
        U32(v) => *v as i128,
        U64(v) => *v as i128,
        U128(v) => *v as i128,
        Usize(v) => *v as i128,
        Bool(_) | Char(_) | EnumVariant { .. } | F32(_) | F64(_) => return None,
    };
    if n < 0 {
        return None;
    }
    usize::try_from(n).ok()
}

/// User-facing rendering of a `ConstValue` for diagnostic messages.
/// Integer variants include their suffix (`120i8`), booleans render as
/// `true`/`false`, char as `'c'`, enum variants as `EnumName.Variant`.
fn format_const_value(cv: &crate::prelude::ConstValue) -> String {
    use crate::prelude::ConstValue::*;
    match cv {
        I8(v) => format!("{}i8", v),
        I16(v) => format!("{}i16", v),
        I32(v) => format!("{}i32", v),
        I64(v) => format!("{}i64", v),
        I128(v) => format!("{}i128", v),
        U8(v) => format!("{}u8", v),
        U16(v) => format!("{}u16", v),
        U32(v) => format!("{}u32", v),
        U64(v) => format!("{}u64", v),
        U128(v) => format!("{}u128", v),
        Usize(v) => format!("{}usize", v),
        F32(v) => format!("{}f32", v),
        F64(v) => format!("{}f64", v),
        Bool(b) => b.to_string(),
        Char(c) => format!("'{}'", c),
        EnumVariant {
            enum_name,
            variant_name,
            ..
        } => format!("{}.{}", enum_name, variant_name),
    }
}

/// Source-text glyph for a `BinOp` (`Add` → `+`, `Eq` → `==`, etc.).
fn binop_glyph(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "==",
        BinOp::NotEq => "!=",
        BinOp::Lt => "<",
        BinOp::LtEq => "<=",
        BinOp::Gt => ">",
        BinOp::GtEq => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::Range => "..",
        BinOp::RangeInclusive => "..=",
    }
}

/// Source-text glyph for a `UnaryOp`.
fn unaryop_glyph(op: &UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "!",
        UnaryOp::BitNot => "~",
        UnaryOp::Deref => "*",
    }
}

/// Type-tag for a `ConstValue` payload. Mirrors `primitive_const_type` for
/// the const-eval path so error diagnostics carry the right surface type.
fn const_value_type(cv: &crate::prelude::ConstValue) -> Type {
    use crate::prelude::ConstValue::*;
    match cv {
        I8(_) => Type::Int(IntSize::I8),
        I16(_) => Type::Int(IntSize::I16),
        I32(_) => Type::Int(IntSize::I32),
        I64(_) => Type::Int(IntSize::I64),
        I128(_) => Type::Int(IntSize::I128),
        U8(_) => Type::UInt(UIntSize::U8),
        U16(_) => Type::UInt(UIntSize::U16),
        U32(_) => Type::UInt(UIntSize::U32),
        U64(_) => Type::UInt(UIntSize::U64),
        U128(_) => Type::UInt(UIntSize::U128),
        Usize(_) => Type::UInt(UIntSize::Usize),
        F32(_) => Type::Float(FloatSize::F32),
        F64(_) => Type::Float(FloatSize::F64),
        Bool(_) => Type::Bool,
        Char(_) => Type::Char,
        EnumVariant { enum_name, .. } => Type::Named {
            name: enum_name.clone(),
            args: Vec::new(),
        },
    }
}

/// Best-effort target-type inference for a binary comparison's operands.
/// Looks at literal suffixes and returns the first explicit suffix found
/// (left wins over right). Returns `None` when neither operand carries an
/// explicit suffix — the caller falls back to a default (typically i64).
fn infer_operand_target_ty(left: &Expr, right: &Expr) -> Option<Type> {
    fn from_literal(e: &Expr) -> Option<Type> {
        match &e.kind {
            ExprKind::Integer(_, Some(IntSuffix::I8)) => Some(Type::Int(IntSize::I8)),
            ExprKind::Integer(_, Some(IntSuffix::I16)) => Some(Type::Int(IntSize::I16)),
            ExprKind::Integer(_, Some(IntSuffix::I32)) => Some(Type::Int(IntSize::I32)),
            ExprKind::Integer(_, Some(IntSuffix::I64)) => Some(Type::Int(IntSize::I64)),
            ExprKind::Integer(_, Some(IntSuffix::I128)) => Some(Type::Int(IntSize::I128)),
            ExprKind::Integer(_, Some(IntSuffix::U8)) => Some(Type::UInt(UIntSize::U8)),
            ExprKind::Integer(_, Some(IntSuffix::U16)) => Some(Type::UInt(UIntSize::U16)),
            ExprKind::Integer(_, Some(IntSuffix::U32)) => Some(Type::UInt(UIntSize::U32)),
            ExprKind::Integer(_, Some(IntSuffix::U64)) => Some(Type::UInt(UIntSize::U64)),
            ExprKind::Integer(_, Some(IntSuffix::U128)) => Some(Type::UInt(UIntSize::U128)),
            ExprKind::Bool(_) => Some(Type::Bool),
            ExprKind::CharLit(_) => Some(Type::Char),
            _ => None,
        }
    }
    from_literal(left).or_else(|| from_literal(right))
}

/// Pack a literal integer `n` into a `ConstValue` of `target_ty`, returning
/// `OutOfRange` if the value doesn't fit the target's bit width.
fn integer_to_const_value(
    n: i64,
    target_ty: &Type,
    span: &Span,
) -> Result<crate::prelude::ConstValue, ConstEvalError> {
    use crate::prelude::ConstValue;
    let out_of_range = |target: &Type| ConstEvalError::OutOfRange {
        value: n as i128,
        target_ty: target.clone(),
        span: span.clone(),
    };
    match target_ty {
        Type::Int(IntSize::I8) => i8::try_from(n)
            .map(ConstValue::I8)
            .map_err(|_| out_of_range(target_ty)),
        Type::Int(IntSize::I16) => i16::try_from(n)
            .map(ConstValue::I16)
            .map_err(|_| out_of_range(target_ty)),
        Type::Int(IntSize::I32) => i32::try_from(n)
            .map(ConstValue::I32)
            .map_err(|_| out_of_range(target_ty)),
        Type::Int(IntSize::I64) => Ok(ConstValue::I64(n)),
        // Const generics slice 2b: AST `ExprKind::Integer(i64, _)`
        // already bounds the literal to i64 at parse time, so `n as
        // i128` is the full source-level value. Widening of the
        // `Integer` carrier to i128 bits is future work; pre-widening
        // this is exact for all current source-level literals.
        Type::Int(IntSize::I128) => Ok(ConstValue::I128(n as i128)),
        Type::UInt(UIntSize::U8) => u8::try_from(n)
            .map(ConstValue::U8)
            .map_err(|_| out_of_range(target_ty)),
        Type::UInt(UIntSize::U16) => u16::try_from(n)
            .map(ConstValue::U16)
            .map_err(|_| out_of_range(target_ty)),
        Type::UInt(UIntSize::U32) => u32::try_from(n)
            .map(ConstValue::U32)
            .map_err(|_| out_of_range(target_ty)),
        Type::UInt(UIntSize::U64) => u64::try_from(n)
            .map(ConstValue::U64)
            .map_err(|_| out_of_range(target_ty)),
        Type::UInt(UIntSize::U128) => u128::try_from(n)
            .map(ConstValue::U128)
            .map_err(|_| out_of_range(target_ty)),
        Type::UInt(UIntSize::Usize) => u64::try_from(n)
            .map(ConstValue::Usize)
            .map_err(|_| out_of_range(target_ty)),
        // Non-integer target — caller's responsibility; the evaluator's
        // dispatch should never route a non-int target here, but be safe.
        _ => Ok(ConstValue::I64(n)),
    }
}

/// Apply a unary operator to a `ConstValue`, returning `UnaryOverflow` on
/// `checked_neg` failure or a focused diagnostic on type mismatch.
fn apply_unary(
    op: UnaryOp,
    val: crate::prelude::ConstValue,
    span: &Span,
) -> Result<crate::prelude::ConstValue, ConstEvalError> {
    use crate::prelude::ConstValue::*;
    match op {
        UnaryOp::Neg => match val {
            I8(v) => v
                .checked_neg()
                .map(I8)
                .ok_or(ConstEvalError::UnaryOverflow {
                    op,
                    operand: I8(v),
                    span: span.clone(),
                }),
            I16(v) => v
                .checked_neg()
                .map(I16)
                .ok_or(ConstEvalError::UnaryOverflow {
                    op,
                    operand: I16(v),
                    span: span.clone(),
                }),
            I32(v) => v
                .checked_neg()
                .map(I32)
                .ok_or(ConstEvalError::UnaryOverflow {
                    op,
                    operand: I32(v),
                    span: span.clone(),
                }),
            I64(v) => v
                .checked_neg()
                .map(I64)
                .ok_or(ConstEvalError::UnaryOverflow {
                    op,
                    operand: I64(v),
                    span: span.clone(),
                }),
            I128(v) => v
                .checked_neg()
                .map(I128)
                .ok_or(ConstEvalError::UnaryOverflow {
                    op,
                    operand: I128(v),
                    span: span.clone(),
                }),
            // Unsigned negation isn't meaningful; reject as ArithOnNonInt
            // would be misleading — these are integers, just not negatable.
            // Use UnaryOverflow with a clear span pointing at the operand.
            other => Err(ConstEvalError::UnaryOverflow {
                op,
                operand: other,
                span: span.clone(),
            }),
        },
        UnaryOp::Not => match val {
            Bool(b) => Ok(Bool(!b)),
            I8(v) => Ok(I8(!v)),
            I16(v) => Ok(I16(!v)),
            I32(v) => Ok(I32(!v)),
            I64(v) => Ok(I64(!v)),
            I128(v) => Ok(I128(!v)),
            U8(v) => Ok(U8(!v)),
            U16(v) => Ok(U16(!v)),
            U32(v) => Ok(U32(!v)),
            U64(v) => Ok(U64(!v)),
            U128(v) => Ok(U128(!v)),
            Usize(v) => Ok(Usize(!v)),
            other => Err(ConstEvalError::LogicalOnNonBool {
                ty: const_value_type(&other),
                // `Not` is a UnaryOp; we reuse the LogicalOnNonBool error
                // with `BinOp::And` as a sentinel so the diagnostic shape
                // matches the binary case. The renderer keys on `ty` and
                // mentions the unary site via `span`.
                op: BinOp::And,
                span: span.clone(),
            }),
        },
        // Other unary ops (BitNot, future extensions) — not in scope for
        // const-eval at slice 2.
        _ => Err(ConstEvalError::NonConstShape(span.clone())),
    }
}

/// Apply a binary operator. Operand `ConstValue` variants must match for
/// arithmetic / bitwise / shift / comparison; logical operators require
/// `Bool`.
fn apply_binary(
    op: BinOp,
    lhs: crate::prelude::ConstValue,
    rhs: crate::prelude::ConstValue,
    span: &Span,
) -> Result<crate::prelude::ConstValue, ConstEvalError> {
    use crate::prelude::ConstValue::*;
    // Logical operators — operands must both be Bool.
    match op {
        BinOp::And => {
            return match (lhs, rhs) {
                (Bool(a), Bool(b)) => Ok(Bool(a && b)),
                (l, _) => Err(ConstEvalError::LogicalOnNonBool {
                    ty: const_value_type(&l),
                    op,
                    span: span.clone(),
                }),
            };
        }
        BinOp::Or => {
            return match (lhs, rhs) {
                (Bool(a), Bool(b)) => Ok(Bool(a || b)),
                (l, _) => Err(ConstEvalError::LogicalOnNonBool {
                    ty: const_value_type(&l),
                    op,
                    span: span.clone(),
                }),
            };
        }
        _ => {}
    }
    // Comparison operators — both operands must share a comparable family.
    if matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    ) {
        return apply_comparison(op, lhs, rhs, span);
    }
    // Arithmetic / bitwise / shift — operands must have matching int variants.
    apply_arithmetic(op, lhs, rhs, span)
}

fn apply_comparison(
    op: BinOp,
    lhs: crate::prelude::ConstValue,
    rhs: crate::prelude::ConstValue,
    span: &Span,
) -> Result<crate::prelude::ConstValue, ConstEvalError> {
    use crate::prelude::ConstValue::*;
    let cmp = |a: std::cmp::Ordering| -> bool {
        use std::cmp::Ordering::*;
        match op {
            BinOp::Eq => a == Equal,
            BinOp::NotEq => a != Equal,
            BinOp::Lt => a == Less,
            BinOp::LtEq => a != Greater,
            BinOp::Gt => a == Greater,
            BinOp::GtEq => a != Less,
            _ => unreachable!(),
        }
    };
    let incomparable = |l: &crate::prelude::ConstValue, r: &crate::prelude::ConstValue| {
        ConstEvalError::CompareIncomparable {
            lhs_ty: const_value_type(l),
            rhs_ty: const_value_type(r),
            span: span.clone(),
        }
    };
    let result = match (&lhs, &rhs) {
        (I8(a), I8(b)) => cmp(a.cmp(b)),
        (I16(a), I16(b)) => cmp(a.cmp(b)),
        (I32(a), I32(b)) => cmp(a.cmp(b)),
        (I64(a), I64(b)) => cmp(a.cmp(b)),
        (I128(a), I128(b)) => cmp(a.cmp(b)),
        (U8(a), U8(b)) => cmp(a.cmp(b)),
        (U16(a), U16(b)) => cmp(a.cmp(b)),
        (U32(a), U32(b)) => cmp(a.cmp(b)),
        (U64(a), U64(b)) => cmp(a.cmp(b)),
        (U128(a), U128(b)) => cmp(a.cmp(b)),
        (Usize(a), Usize(b)) => cmp(a.cmp(b)),
        (Bool(a), Bool(b)) => cmp(a.cmp(b)),
        (Char(a), Char(b)) => cmp(a.cmp(b)),
        (
            EnumVariant {
                enum_name: e1,
                discriminant: d1,
                ..
            },
            EnumVariant {
                enum_name: e2,
                discriminant: d2,
                ..
            },
        ) if e1 == e2 => cmp(d1.cmp(d2)),
        _ => return Err(incomparable(&lhs, &rhs)),
    };
    Ok(Bool(result))
}

fn apply_arithmetic(
    op: BinOp,
    lhs: crate::prelude::ConstValue,
    rhs: crate::prelude::ConstValue,
    span: &Span,
) -> Result<crate::prelude::ConstValue, ConstEvalError> {
    use crate::prelude::ConstValue::*;
    // Macro-style dispatch: for each matching pair (Ix, Ix) / (Ux, Ux) /
    // (Usize, Usize), apply the right `checked_*` op (or a panic-free bitwise
    // / shift fallback) and emit Overflow / DivByZero on `None`.
    macro_rules! apply_int {
        ($lv:ident, $rv:ident, $variant:ident, $ty:ty) => {{
            let result: Option<$ty> = match op {
                BinOp::Add => $lv.checked_add(*$rv),
                BinOp::Sub => $lv.checked_sub(*$rv),
                BinOp::Mul => $lv.checked_mul(*$rv),
                BinOp::Div => {
                    if *$rv == 0 {
                        return Err(ConstEvalError::DivByZero { span: span.clone() });
                    }
                    $lv.checked_div(*$rv)
                }
                BinOp::Mod => {
                    if *$rv == 0 {
                        return Err(ConstEvalError::DivByZero { span: span.clone() });
                    }
                    $lv.checked_rem(*$rv)
                }
                BinOp::Shl => {
                    let shift = u32::try_from(*$rv).ok();
                    shift.and_then(|s| $lv.checked_shl(s))
                }
                BinOp::Shr => {
                    let shift = u32::try_from(*$rv).ok();
                    shift.and_then(|s| $lv.checked_shr(s))
                }
                BinOp::BitAnd => Some($lv & $rv),
                BinOp::BitOr => Some($lv | $rv),
                BinOp::BitXor => Some($lv ^ $rv),
                _ => return Err(ConstEvalError::NonConstShape(span.clone())),
            };
            match result {
                Some(v) => Ok($variant(v)),
                None => Err(ConstEvalError::Overflow {
                    op,
                    lhs: $variant(*$lv),
                    rhs: $variant(*$rv),
                    span: span.clone(),
                }),
            }
        }};
    }
    match (&lhs, &rhs) {
        (I8(a), I8(b)) => apply_int!(a, b, I8, i8),
        (I16(a), I16(b)) => apply_int!(a, b, I16, i16),
        (I32(a), I32(b)) => apply_int!(a, b, I32, i32),
        (I64(a), I64(b)) => apply_int!(a, b, I64, i64),
        (I128(a), I128(b)) => apply_int!(a, b, I128, i128),
        (U8(a), U8(b)) => apply_int!(a, b, U8, u8),
        (U16(a), U16(b)) => apply_int!(a, b, U16, u16),
        (U32(a), U32(b)) => apply_int!(a, b, U32, u32),
        (U64(a), U64(b)) => apply_int!(a, b, U64, u64),
        (U128(a), U128(b)) => apply_int!(a, b, U128, u128),
        (Usize(a), Usize(b)) => apply_int!(a, b, Usize, u64),
        // Mismatched int widths or non-int operands.
        (l, _) if matches!(l, Bool(_) | Char(_) | EnumVariant { .. }) => {
            Err(ConstEvalError::ArithOnNonInt {
                ty: const_value_type(l),
                op,
                span: span.clone(),
            })
        }
        _ => Err(ConstEvalError::ArithOnNonInt {
            ty: const_value_type(&lhs),
            op,
            span: span.clone(),
        }),
    }
}

// ── Attribute Helpers ───────────────────────────────────────────

/// Extract trait names from `#[derive(Eq, Hash, ...)]` attributes.
/// Also handles call-form args like `Display(snake_case)` — the trait name
/// (`"Display"`) is inserted regardless of arguments.
fn extract_derived_traits(attributes: &[Attribute]) -> HashSet<String> {
    let mut traits = HashSet::new();
    for attr in attributes {
        if attr.name == "derive" {
            for arg in &attr.args {
                match &arg.value {
                    // `#[derive(Eq)]` — bare identifier
                    Some(Expr {
                        kind: ExprKind::Identifier(name),
                        ..
                    }) => {
                        traits.insert(name.clone());
                    }
                    // `#[derive(Display(snake_case))]` — call expression;
                    // extract the callee-name identifier as the trait name.
                    Some(Expr {
                        kind:
                            ExprKind::Call {
                                callee, args: _, ..
                            },
                        ..
                    }) => {
                        if let ExprKind::Identifier(name) = &callee.kind {
                            traits.insert(name.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    traits
}

/// Returns `true` when `attributes` contains `#[derive(Display(snake_case))]`.
fn has_display_snake_case(attributes: &[Attribute]) -> bool {
    for attr in attributes {
        if attr.name == "derive" {
            for arg in &attr.args {
                if let Some(Expr {
                    kind:
                        ExprKind::Call {
                            callee,
                            args: call_args,
                            ..
                        },
                    ..
                }) = &arg.value
                {
                    if let ExprKind::Identifier(name) = &callee.kind {
                        if name == "Display" {
                            // Check for a single `snake_case` positional argument.
                            if let Some(first) = call_args.first() {
                                if let ExprKind::Identifier(flag) = &first.value.kind {
                                    if flag == "snake_case" {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

// ── Type Display ────────────────────────────────────────────────

/// Reduce a `Type` to a single textual head name suitable for runtime
/// dispatch — the concrete struct/enum name for `Type::Named`, the textual
/// name for `Type::TypeParam` (caller will resolve it transitively against
/// the runtime substitution stack), or one of the primitive lowercase names
/// (`"i32"`, `"bool"`, ...). Returns `None` for compound shapes (tuples,
/// arrays, references, function values) — those don't dispatch through
/// `Type.method` impl entries.
pub fn type_to_concrete_or_param_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Named { name, .. } => Some(name.clone()),
        Type::TypeParam(name) => Some(name.clone()),
        Type::Int(_) | Type::UInt(_) | Type::Float(_) | Type::Bool | Type::Char | Type::Str => {
            Some(type_display(ty))
        }
        _ => None,
    }
}

/// Head name + type-argument vector suitable for `env.impls` lookup.
/// Primitives are keyed under their stringified name (`"i32"`, `"f64"`,
/// `"bool"`, …) by `register_stdlib_impls` with empty args. Named types
/// return their nominal head name and the recursive argument list.
/// Normalize a function's inline generic-param bounds (`fn f[T: Hash + Eq]`)
/// into the where-clause representation (`WhereConstraint::TypeBound`),
/// merged with any existing where-clause constraints. Returns `None` when
/// there are no constraints from either source.
///
/// Used at FunctionSig construction (`src/typechecker.rs:6130` /
/// `src/typechecker.rs:6253`) so the call-site bound-discharge engine
/// (`discharge_type_bounds`) sees inline and where-clause bounds through
/// one uniform API.
///
/// Slice 0.a, sub-step 1 of monomorphized collections prereq
/// ([`phase-7-codegen.md`](../docs/implementation_checklist/phase-7-codegen.md)).
fn normalize_bounds_into_where_clause(
    generic_params: &Option<GenericParams>,
    where_clause: &Option<WhereClause>,
) -> Option<WhereClause> {
    let mut constraints: Vec<WhereConstraint> = Vec::new();
    if let Some(ref gp) = generic_params {
        for param in &gp.params {
            if param.is_const || param.bounds.is_empty() {
                continue;
            }
            constraints.push(WhereConstraint::TypeBound {
                type_name: param.name.clone(),
                bounds: param.bounds.clone(),
                span: param.span.clone(),
            });
        }
    }
    if let Some(ref wc) = where_clause {
        constraints.extend(wc.constraints.iter().cloned());
    }
    if constraints.is_empty() {
        return None;
    }
    let span = where_clause
        .as_ref()
        .map(|wc| wc.span.clone())
        .or(generic_params.as_ref().map(|gp| gp.span.clone()))
        .unwrap_or(Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        });
    Some(WhereClause { constraints, span })
}

/// Returns `None` for type variables, function types, slices, tuples,
/// etc. — none of which can satisfy a nominal trait bound today. Strips
/// outer `ref` / `mut ref` so a borrowed receiver discharges against
/// the same impls as its inner type.
fn impl_table_key(ty: &Type) -> Option<(String, Vec<Type>)> {
    match ty {
        Type::Int(s) => Some((
            match s {
                IntSize::I8 => "i8",
                IntSize::I16 => "i16",
                IntSize::I32 => "i32",
                IntSize::I64 => "i64",
                IntSize::I128 => "i128",
            }
            .to_string(),
            Vec::new(),
        )),
        Type::UInt(s) => Some((
            match s {
                UIntSize::U8 => "u8",
                UIntSize::U16 => "u16",
                UIntSize::U32 => "u32",
                UIntSize::U64 => "u64",
                UIntSize::U128 => "u128",
                UIntSize::Usize => "usize",
            }
            .to_string(),
            Vec::new(),
        )),
        Type::Float(s) => Some((
            match s {
                FloatSize::F32 => "f32",
                FloatSize::F64 => "f64",
            }
            .to_string(),
            Vec::new(),
        )),
        Type::Bool => Some(("bool".to_string(), Vec::new())),
        Type::Char => Some(("char".to_string(), Vec::new())),
        Type::Str => Some(("String".to_string(), Vec::new())),
        Type::Named { name, args } => Some((name.clone(), args.clone())),
        Type::Ref(inner) | Type::MutRef(inner) => impl_table_key(inner),
        _ => None,
    }
}

/// Match rule for the Theme-4 impl-table key shape: a stored impl's
/// `target_args` matches a call-site args vector iff the stored args
/// are empty (impl is generic-on-name and applies to any instantiation)
/// OR the two args vectors are equal. Length mismatch when the stored
/// args are non-empty is a non-match.
fn impl_args_match(stored: &[Type], call_site: &[Type]) -> bool {
    stored.is_empty() || stored == call_site
}

/// Strip the outer wrapper from a method-call receiver type to surface
/// the named receiver for impl-table lookup. Per design.md § Method
/// Resolution Step 1, the autoref candidates `T`, `ref T`, `mut ref T`
/// collapse to the same name lookup. Sub-item 3a of the
/// `Type::Shared` / `Type::Rc` / `Type::Arc` representation work
/// extends this with three more wrappers — shared structs lower their
/// outer `Type::Shared(name)` to `Type::Named { name, args: [] }`
/// (matches the user-defined-struct lookup path verbatim);
/// `Rc(inner)` / `Arc(inner)` deref to the inner type so the
/// wrapped-type's methods become reachable.
fn receiver_for_method_lookup(obj_ty: &Type) -> Type {
    match obj_ty {
        Type::Ref(inner) | Type::MutRef(inner) => (**inner).clone(),
        Type::Shared(name) => Type::Named {
            name: name.clone(),
            args: vec![],
        },
        Type::Rc(inner) | Type::Arc(inner) => (**inner).clone(),
        other => other.clone(),
    }
}

/// `true` iff every `TypeParam` / `TypeVar` / `AssocProjection` is
/// absent from the type recursively. Used by `env_add_impl` to decide
/// whether an impl's target args should be stored as specialized
/// (fully concrete → keep) or treated as generic-on-name (any
/// non-concrete piece → drop the args, store empty).
fn type_is_fully_concrete(ty: &Type) -> bool {
    match ty {
        Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } => false,
        Type::Named { args, .. } => args.iter().all(type_is_fully_concrete),
        Type::Tuple(types) => types.iter().all(type_is_fully_concrete),
        Type::Array { element, .. } => type_is_fully_concrete(element),
        Type::Slice { element, .. } => type_is_fully_concrete(element),
        Type::Ref(inner) | Type::MutRef(inner) | Type::Weak(inner) => type_is_fully_concrete(inner),
        Type::Rc(inner) | Type::Arc(inner) => type_is_fully_concrete(inner),
        Type::Pointer { inner, .. } => type_is_fully_concrete(inner),
        Type::Function {
            params,
            return_type,
        }
        | Type::OnceFunction {
            params,
            return_type,
        } => params.iter().all(type_is_fully_concrete) && type_is_fully_concrete(return_type),
        Type::Int(_)
        | Type::UInt(_)
        | Type::Float(_)
        | Type::Bool
        | Type::Char
        | Type::Str
        | Type::Unit
        | Type::Never
        | Type::Shared(_)
        | Type::Error => true,
    }
}

pub fn type_display(ty: &Type) -> String {
    match ty {
        Type::Int(s) => match s {
            IntSize::I8 => "i8",
            IntSize::I16 => "i16",
            IntSize::I32 => "i32",
            IntSize::I64 => "i64",
            IntSize::I128 => "i128",
        }
        .to_string(),
        Type::UInt(s) => match s {
            UIntSize::U8 => "u8",
            UIntSize::U16 => "u16",
            UIntSize::U32 => "u32",
            UIntSize::U64 => "u64",
            UIntSize::U128 => "u128",
            UIntSize::Usize => "usize",
        }
        .to_string(),
        Type::Float(s) => match s {
            FloatSize::F32 => "f32",
            FloatSize::F64 => "f64",
        }
        .to_string(),
        Type::Bool => "bool".to_string(),
        Type::Char => "char".to_string(),
        Type::Str => "String".to_string(),
        Type::Unit => "()".to_string(),
        Type::Never => "!".to_string(),
        Type::Tuple(types) => {
            let inner: Vec<String> = types.iter().map(type_display).collect();
            format!("({})", inner.join(", "))
        }
        Type::Array { element, size } => format!(
            "Array[{}, {}]",
            type_display(element),
            const_arg_display(size)
        ),
        Type::Slice { element, mutable } => {
            if *mutable {
                format!("mut Slice[{}]", type_display(element))
            } else {
                format!("Slice[{}]", type_display(element))
            }
        }
        Type::Named { name, args } if args.is_empty() => name.clone(),
        Type::Named { name, args } => {
            let inner: Vec<String> = args.iter().map(type_display).collect();
            format!("{}<{}>", name, inner.join(", "))
        }
        Type::Shared(name) => name.clone(),
        Type::Rc(inner) => format!("Rc[{}]", type_display(inner)),
        Type::Arc(inner) => format!("Arc[{}]", type_display(inner)),
        Type::Function {
            params,
            return_type,
        } => {
            let p: Vec<String> = params.iter().map(type_display).collect();
            if **return_type == Type::Unit {
                format!("Fn({})", p.join(", "))
            } else {
                format!("Fn({}) -> {}", p.join(", "), type_display(return_type))
            }
        }
        Type::OnceFunction {
            params,
            return_type,
        } => {
            let p: Vec<String> = params.iter().map(type_display).collect();
            if **return_type == Type::Unit {
                format!("OnceFn({})", p.join(", "))
            } else {
                format!("OnceFn({}) -> {}", p.join(", "), type_display(return_type))
            }
        }
        Type::Ref(inner) => format!("ref {}", type_display(inner)),
        Type::MutRef(inner) => format!("mut ref {}", type_display(inner)),
        Type::Weak(inner) => format!("weak {}", type_display(inner)),
        Type::Pointer { is_mut, inner } => {
            if *is_mut {
                format!("*mut {}", type_display(inner))
            } else {
                format!("*const {}", type_display(inner))
            }
        }
        Type::TypeParam(name) => name.clone(),
        Type::TypeVar(id) => format!("?T{}", id.0),
        Type::AssocProjection { param, assoc } => format!("{}.{}", param, assoc),
        Type::Error => "<error>".to_string(),
    }
}

// ── Type Compatibility ──────────────────────────────────────────

/// True iff `name` is a primitive / prelude type or stdlib module name
/// reachable at scope-0 — used by `resolve_identifier_type`'s variant
/// fallback to skip name-shadow cases like `Json.String(String)` where
/// the variant name collides with the primitive type name. See the
/// comment block at the variant-fallback site.
fn is_prelude_type_or_module_name(name: &str) -> bool {
    crate::prelude::PRELUDE_PRIMITIVES.contains(&name)
        || crate::prelude::PRELUDE_TYPES.contains(&name)
}

/// Map a primitive-type associated constant value to its surface `Type`.
/// Used by `infer_field_access` to resolve `i64.MAX` / `f64.INFINITY` /
/// `usize.MAX` etc. to the correct numeric type. The interpreter and
/// codegen consume the same `ConstValue` for the runtime / LLVM value.
fn primitive_const_type(cv: &crate::prelude::ConstValue) -> Type {
    use crate::prelude::ConstValue::*;
    match cv {
        I8(_) => Type::Int(IntSize::I8),
        I16(_) => Type::Int(IntSize::I16),
        I32(_) => Type::Int(IntSize::I32),
        I64(_) => Type::Int(IntSize::I64),
        I128(_) => Type::Int(IntSize::I128),
        U8(_) => Type::UInt(UIntSize::U8),
        U16(_) => Type::UInt(UIntSize::U16),
        U32(_) => Type::UInt(UIntSize::U32),
        U64(_) => Type::UInt(UIntSize::U64),
        U128(_) => Type::UInt(UIntSize::U128),
        Usize(_) => Type::UInt(UIntSize::Usize),
        F32(_) => Type::Float(FloatSize::F32),
        F64(_) => Type::Float(FloatSize::F64),
        Bool(_) => Type::Bool,
        Char(_) => Type::Char,
        EnumVariant { enum_name, .. } => Type::Named {
            name: enum_name.clone(),
            args: Vec::new(),
        },
    }
}

fn is_numeric(ty: &Type) -> bool {
    matches!(ty, Type::Int(_) | Type::UInt(_) | Type::Float(_))
}

fn is_integer(ty: &Type) -> bool {
    matches!(ty, Type::Int(_) | Type::UInt(_))
}

/// Width of an integer type in bits, for the char→int narrowing check.
/// `usize` / `isize` are conservatively treated as 32-bit so a 32-bit
/// target rejects `char as usize`; on 64-bit targets the cast is still
/// allowed via the wider-int path. The actual address-width of `usize`
/// is platform-dependent and folded in at codegen.
fn integer_width_bits(ty: &Type) -> Option<u32> {
    match ty {
        Type::Int(IntSize::I8) => Some(8),
        Type::Int(IntSize::I16) => Some(16),
        Type::Int(IntSize::I32) => Some(32),
        Type::Int(IntSize::I64) => Some(64),
        Type::UInt(UIntSize::U8) => Some(8),
        Type::UInt(UIntSize::U16) => Some(16),
        Type::UInt(UIntSize::U32) => Some(32),
        Type::UInt(UIntSize::U64) => Some(64),
        Type::UInt(UIntSize::Usize) => Some(64),
        _ => None,
    }
}

/// Map a typechecked receiver type to the receiver-name segment used in the
/// `Type.method` keys of `EffectCheckResult.{inferred,declared}_effects`
/// (and therefore in `Program.callee_effectful`). Returns `None` for
/// shapes that don't carry method dispatch in v1 (function types, type
/// variables, `Type::Error`, etc.). Used by `infer_method_call` to
/// populate `method_callee_types`, which feeds the par-branch cancel-check
/// narrowing.
fn method_callee_type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Named { name, .. } => Some(name.clone()),
        Type::Str => Some("String".to_string()),
        Type::Slice { .. } => Some("Slice".to_string()),
        Type::Array { .. } => Some("Array".to_string()),
        Type::Bool => Some("bool".to_string()),
        Type::Char => Some("char".to_string()),
        Type::Int(IntSize::I8) => Some("i8".to_string()),
        Type::Int(IntSize::I16) => Some("i16".to_string()),
        Type::Int(IntSize::I32) => Some("i32".to_string()),
        Type::Int(IntSize::I64) => Some("i64".to_string()),
        Type::UInt(UIntSize::U8) => Some("u8".to_string()),
        Type::UInt(UIntSize::U16) => Some("u16".to_string()),
        Type::UInt(UIntSize::U32) => Some("u32".to_string()),
        Type::UInt(UIntSize::U64) => Some("u64".to_string()),
        Type::UInt(UIntSize::Usize) => Some("usize".to_string()),
        Type::Float(FloatSize::F32) => Some("f32".to_string()),
        Type::Float(FloatSize::F64) => Some("f64".to_string()),
        Type::Ref(inner) | Type::MutRef(inner) | Type::Weak(inner) => {
            method_callee_type_name(inner)
        }
        _ => None,
    }
}

/// Resolve the `Item` type of an iterable receiver — the element yielded by
/// `next()` after `iter()` / `into_iter()`. Returns `None` if `ty` is not an
/// iterable collection. `Map[K, V]` yields `(K, V)` tuples per design.md
/// § Iteration; `Vec`, `Set`, `SortedSet`, `Array`, `Slice` yield `T`.
/// `ref` / `mut ref` borrows are unwrapped transparently.
fn iterator_item_type_for(ty: &Type) -> Option<Type> {
    match ty {
        Type::Array { element, .. } => Some((**element).clone()),
        Type::Slice { element, .. } => Some((**element).clone()),
        Type::Named { name, args } => match name.as_str() {
            "Vec" | "Set" | "SortedSet" | "VecDeque" if args.len() == 1 => Some(args[0].clone()),
            "Map" if args.len() == 2 => Some(Type::Tuple(vec![args[0].clone(), args[1].clone()])),
            // `Range` / `RangeInclusive` are Iterators — `(0..n).iter()` is
            // a redundant pass-through that yields the bound element type.
            "Range" | "RangeInclusive" if args.len() == 1 => Some(args[0].clone()),
            _ => None,
        },
        Type::Ref(inner) | Type::MutRef(inner) => iterator_item_type_for(inner),
        _ => None,
    }
}

/// Return the `Self` type for `clone()` on stdlib collection types, or
/// None if the receiver isn't a Clone-bearing collection. Used by the
/// `clone()` arm in `infer_method_call` so any `ref`/`mut ref` borrow of
/// a collection still resolves to the underlying owned type. See the
/// `Clone trait surface for collections` bullet in
/// `phase-8-stdlib-floor.md`.
///
/// Element-type `T: Clone` bound checking rides the existing trait-bound
/// machinery — primitives, `String`, and stdlib collection types satisfy
/// `Clone` trivially; user structs without `#[derive(Clone)]` would be
/// rejected at the bound-resolution layer when that lands.
fn clone_self_type_for(ty: &Type) -> Option<Type> {
    match ty {
        Type::Str => Some(Type::Str),
        Type::Array { .. } => Some(ty.clone()),
        Type::Named { name, args: _ } => match name.as_str() {
            "Vec" | "Set" | "SortedSet" | "VecDeque" | "Map" | "TreeMap" => Some(ty.clone()),
            _ => None,
        },
        Type::Ref(inner) | Type::MutRef(inner) => clone_self_type_for(inner),
        _ => None,
    }
}

/// Walk `ty` looking for any `Type::TypeParam` or `Type::AssocProjection` node.
/// Used by `infer_call` to decide whether a callee signature needs ad-hoc
/// generic instantiation.
fn contains_type_param(ty: &Type) -> bool {
    match ty {
        Type::TypeParam(_) | Type::AssocProjection { .. } => true,
        Type::Tuple(elems) => elems.iter().any(contains_type_param),
        // Const generics slice 3b: a `ConstArg::ConstParam` in
        // `Type::Array.size` is also a "generic dependency" — the
        // call-site inference solver needs to fire so the const-param
        // gets resolved. Pre-3b this returned false for
        // `Array[i64, ConstParam(N)]` and the generic-call path was
        // skipped entirely; post-3b we check the size too.
        Type::Array { element, size } => {
            contains_type_param(element) || matches!(size, ConstArg::ConstParam(_))
        }
        Type::Slice { element, .. } => contains_type_param(element),
        Type::Ref(inner) | Type::MutRef(inner) | Type::Weak(inner) => contains_type_param(inner),
        Type::Pointer { inner, .. } => contains_type_param(inner),
        Type::Named { args, .. } => args.iter().any(contains_type_param),
        Type::Function {
            params,
            return_type,
        }
        | Type::OnceFunction {
            params,
            return_type,
        } => params.iter().any(contains_type_param) || contains_type_param(return_type),
        _ => false,
    }
}

/// Structural substitution of `Type::TypeParam(name)` → concrete type
/// from `subs`. Callers build `subs` externally from concrete types and
/// use this purely as a tree-walk utility — they do *not* perform type
/// inference. Inference at call sites uses the metavar substrate
/// (`instantiate_signature_with_fresh_vars` + `unify_types` +
/// `resolve_type_vars`, item 131 sub-step 2b) instead.
///
/// Post-F1 (const generics slice 1) the substitution map carries
/// `SubstValue::Type | Const` so a single context flows both type-args
/// and const-args. `Type::TypeParam(name)` lookups extract the `Type`
/// payload via `SubstValue::as_type`; `Const` entries are inert here
/// (consumed by slice 2's evaluator + slice 4's codegen).
///
/// Unsolved params pass through unchanged.
fn substitute_type_params(ty: &Type, subs: &HashMap<String, SubstValue>) -> Type {
    match ty {
        Type::TypeParam(name) => subs
            .get(name)
            .and_then(SubstValue::as_type)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Type::Tuple(elems) => Type::Tuple(
            elems
                .iter()
                .map(|e| substitute_type_params(e, subs))
                .collect(),
        ),
        Type::Array { element, size } => Type::Array {
            element: Box::new(substitute_type_params(element, subs)),
            // Const generics slice 3 sub-step (g): substitute
            // `ConstArg::ConstParam(name)` against the same
            // `SubstValue` map used for type params. When the map binds
            // `name` → `Const(cv)`, rewrite the array's size to a
            // `Literal` carrying the resolved value. `Literal` and
            // `ConstVar` arms pass through unchanged.
            size: substitute_const_arg(size, subs),
        },
        Type::Slice { element, mutable } => Type::Slice {
            element: Box::new(substitute_type_params(element, subs)),
            mutable: *mutable,
        },
        Type::Ref(inner) => Type::Ref(Box::new(substitute_type_params(inner, subs))),
        Type::MutRef(inner) => Type::MutRef(Box::new(substitute_type_params(inner, subs))),
        Type::Weak(inner) => Type::Weak(Box::new(substitute_type_params(inner, subs))),
        Type::Pointer { is_mut, inner } => Type::Pointer {
            is_mut: *is_mut,
            inner: Box::new(substitute_type_params(inner, subs)),
        },
        Type::Named { name, args } => Type::Named {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| substitute_type_params(a, subs))
                .collect(),
        },
        Type::Function {
            params,
            return_type,
        } => Type::Function {
            params: params
                .iter()
                .map(|p| substitute_type_params(p, subs))
                .collect(),
            return_type: Box::new(substitute_type_params(return_type, subs)),
        },
        Type::OnceFunction {
            params,
            return_type,
        } => Type::OnceFunction {
            params: params
                .iter()
                .map(|p| substitute_type_params(p, subs))
                .collect(),
            return_type: Box::new(substitute_type_params(return_type, subs)),
        },
        // If the param is solved but we can't resolve the assoc type yet
        // (requires impl table lookup), keep as AssocProjection so the
        // caller can post-resolve via `resolve_assoc_projections`.
        Type::AssocProjection { param, assoc } => {
            if let Some(concrete) = subs.get(param).and_then(SubstValue::as_type) {
                Type::AssocProjection {
                    param: type_display(concrete),
                    assoc: assoc.clone(),
                }
            } else {
                ty.clone()
            }
        }
        _ => ty.clone(),
    }
}

/// Replace every `Type::TypeParam(name)` in `params` and `return_type`
/// with a fresh `Type::TypeVar(id)`, allocating ids out of the supplied
/// `next_type_var` counter. Returns the substituted (params, return)
/// alongside both directions of the name↔id mapping. Used by item 131
/// sub-step 2b at generic call sites: each call gets its own fresh
/// metavariables so cross-call collisions are impossible (`id(id(7))`
/// gets `?M0` for outer T and `?M1` for inner T even though both have
/// the spelling `T`). Names appear once in the order they're first
/// encountered; this stability isn't required by callers but keeps
/// diagnostic output deterministic.
fn instantiate_signature_with_fresh_vars(
    params: &[Type],
    return_type: &Type,
    next_type_var: &mut u32,
    next_const_var: &mut u32,
) -> InstantiatedSignature {
    fn collect(
        ty: &Type,
        names: &mut Vec<String>,
        seen: &mut HashSet<String>,
        const_names: &mut Vec<String>,
        const_seen: &mut HashSet<String>,
    ) {
        match ty {
            Type::TypeParam(n) if seen.insert(n.clone()) => {
                names.push(n.clone());
            }
            Type::TypeParam(_) => {}
            Type::Tuple(es) => {
                for e in es {
                    collect(e, names, seen, const_names, const_seen);
                }
            }
            Type::Array { element, size } => {
                collect(element, names, seen, const_names, const_seen);
                // Const generics slice 3b: descend into the Array size
                // to gather `ConstArg::ConstParam(n)` names so the
                // signature instantiation mints a fresh `ConstVarId`
                // per unique const-param name.
                if let ConstArg::ConstParam(n) = size {
                    if const_seen.insert(n.clone()) {
                        const_names.push(n.clone());
                    }
                }
            }
            Type::Slice { element, .. } => collect(element, names, seen, const_names, const_seen),
            Type::Ref(i) | Type::MutRef(i) | Type::Weak(i) => {
                collect(i, names, seen, const_names, const_seen)
            }
            Type::Pointer { inner, .. } => collect(inner, names, seen, const_names, const_seen),
            Type::Named { args, .. } => {
                for a in args {
                    collect(a, names, seen, const_names, const_seen);
                }
            }
            Type::Function {
                params,
                return_type,
            }
            | Type::OnceFunction {
                params,
                return_type,
            } => {
                for p in params {
                    collect(p, names, seen, const_names, const_seen);
                }
                collect(return_type, names, seen, const_names, const_seen);
            }
            // AssocProjection.param is a String holding the resolved
            // concrete type name; not a TypeParam introduction site.
            _ => {}
        }
    }
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut const_names: Vec<String> = Vec::new();
    let mut const_seen: HashSet<String> = HashSet::new();
    for p in params {
        collect(p, &mut names, &mut seen, &mut const_names, &mut const_seen);
    }
    collect(
        return_type,
        &mut names,
        &mut seen,
        &mut const_names,
        &mut const_seen,
    );

    let mut name_to_id: HashMap<String, TypeVarId> = HashMap::new();
    let mut id_to_name: HashMap<TypeVarId, String> = HashMap::new();
    for name in &names {
        let id = TypeVarId(*next_type_var);
        *next_type_var += 1;
        name_to_id.insert(name.clone(), id);
        id_to_name.insert(id, name.clone());
    }

    let mut name_to_const_id: HashMap<String, ConstVarId> = HashMap::new();
    let mut const_id_to_name: HashMap<ConstVarId, String> = HashMap::new();
    for name in &const_names {
        let id = ConstVarId(*next_const_var);
        *next_const_var += 1;
        name_to_const_id.insert(name.clone(), id);
        const_id_to_name.insert(id, name.clone());
    }

    fn substitute(
        ty: &Type,
        name_to_id: &HashMap<String, TypeVarId>,
        name_to_const_id: &HashMap<String, ConstVarId>,
    ) -> Type {
        match ty {
            Type::TypeParam(n) => name_to_id
                .get(n)
                .map(|&id| Type::TypeVar(id))
                .unwrap_or_else(|| ty.clone()),
            Type::Tuple(es) => Type::Tuple(
                es.iter()
                    .map(|e| substitute(e, name_to_id, name_to_const_id))
                    .collect(),
            ),
            Type::Array { element, size } => Type::Array {
                element: Box::new(substitute(element, name_to_id, name_to_const_id)),
                size: substitute_const_param_to_var(size, name_to_const_id),
            },
            Type::Slice { element, mutable } => Type::Slice {
                element: Box::new(substitute(element, name_to_id, name_to_const_id)),
                mutable: *mutable,
            },
            Type::Ref(inner) => {
                Type::Ref(Box::new(substitute(inner, name_to_id, name_to_const_id)))
            }
            Type::MutRef(inner) => {
                Type::MutRef(Box::new(substitute(inner, name_to_id, name_to_const_id)))
            }
            Type::Weak(inner) => {
                Type::Weak(Box::new(substitute(inner, name_to_id, name_to_const_id)))
            }
            Type::Pointer { is_mut, inner } => Type::Pointer {
                is_mut: *is_mut,
                inner: Box::new(substitute(inner, name_to_id, name_to_const_id)),
            },
            Type::Named { name, args } => Type::Named {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|a| substitute(a, name_to_id, name_to_const_id))
                    .collect(),
            },
            Type::Function {
                params,
                return_type,
            } => Type::Function {
                params: params
                    .iter()
                    .map(|p| substitute(p, name_to_id, name_to_const_id))
                    .collect(),
                return_type: Box::new(substitute(return_type, name_to_id, name_to_const_id)),
            },
            Type::OnceFunction {
                params,
                return_type,
            } => Type::OnceFunction {
                params: params
                    .iter()
                    .map(|p| substitute(p, name_to_id, name_to_const_id))
                    .collect(),
                return_type: Box::new(substitute(return_type, name_to_id, name_to_const_id)),
            },
            _ => ty.clone(),
        }
    }
    let new_params: Vec<Type> = params
        .iter()
        .map(|p| substitute(p, &name_to_id, &name_to_const_id))
        .collect();
    let new_ret = substitute(return_type, &name_to_id, &name_to_const_id);
    InstantiatedSignature {
        params: new_params,
        return_type: new_ret,
        name_to_id,
        id_to_name,
        name_to_const_id,
        const_id_to_name,
    }
}

/// Result of `instantiate_signature_with_fresh_vars`. Kept as a named
/// struct (slice 3b — fork G1) so the 6-tuple return doesn't accrete
/// positional noise at the caller. Mirrors the slice-2 typechecker-side
/// SubstValue layering: type-side maps for `Type::TypeParam` ↔
/// `Type::TypeVar`, const-side maps for `ConstArg::ConstParam` ↔
/// `ConstArg::ConstVar`.
struct InstantiatedSignature {
    params: Vec<Type>,
    return_type: Type,
    name_to_id: HashMap<String, TypeVarId>,
    id_to_name: HashMap<TypeVarId, String>,
    #[allow(dead_code)]
    name_to_const_id: HashMap<String, ConstVarId>,
    const_id_to_name: HashMap<ConstVarId, String>,
}

/// Const generics slice 3c: walk `expr` and substitute any
/// `ExprKind::Identifier(name)` whose `name` is in `subst` with an
/// `Integer(value)` literal. Used by the where-clause discharge
/// engine to inline resolved const-args into the predicate Expr
/// before evaluation. Composite shapes (Tuple / ArrayLiteral) recurse
/// element-wise; non-substitutable shapes (calls, closures, etc.)
/// pass through unchanged — slice 2's `eval_const_expr` rejects them
/// downstream as `NonConstShape`.
fn substitute_const_idents_in_expr(expr: &Expr, subst: &HashMap<String, i64>) -> Expr {
    let new_kind = match &expr.kind {
        ExprKind::Identifier(name) => match subst.get(name) {
            Some(&value) => ExprKind::Integer(value, None),
            None => expr.kind.clone(),
        },
        ExprKind::Unary { op, operand } => ExprKind::Unary {
            op: op.clone(),
            operand: Box::new(substitute_const_idents_in_expr(operand, subst)),
        },
        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op: op.clone(),
            left: Box::new(substitute_const_idents_in_expr(left, subst)),
            right: Box::new(substitute_const_idents_in_expr(right, subst)),
        },
        ExprKind::Tuple(elems) => ExprKind::Tuple(
            elems
                .iter()
                .map(|e| substitute_const_idents_in_expr(e, subst))
                .collect(),
        ),
        ExprKind::ArrayLiteral(elems) => ExprKind::ArrayLiteral(
            elems
                .iter()
                .map(|e| substitute_const_idents_in_expr(e, subst))
                .collect(),
        ),
        _ => expr.kind.clone(),
    };
    Expr {
        kind: new_kind,
        span: expr.span.clone(),
    }
}

/// Convert an `Expr` parsed in value position back to a `TypeExpr` when
/// the expression actually denotes a type. Used by the layout-query
/// intrinsic intercept (`size_of[T]()` / `align_of[T]()`) where the
/// parser produces `Call { callee: Index { Ident, T_expr } }` for the
/// single-arg shape because `lookahead_generic_args_call` requires a
/// top-level comma to disambiguate from `arr[i]()`. Returns `None` for
/// expression shapes that don't denote a type (literals, calls, binary
/// ops, etc.) — the caller emits a focused diagnostic in that branch.
fn expr_as_type_expr(expr: &Expr) -> Option<TypeExpr> {
    match &expr.kind {
        ExprKind::Identifier(name) => Some(TypeExpr {
            kind: TypeKind::Path(PathExpr {
                segments: vec![name.clone()],
                generic_args: None,
                span: expr.span.clone(),
            }),
            span: expr.span.clone(),
        }),
        ExprKind::Path {
            segments,
            generic_args,
        } => Some(TypeExpr {
            kind: TypeKind::Path(PathExpr {
                segments: segments.clone(),
                generic_args: generic_args.clone(),
                span: expr.span.clone(),
            }),
            span: expr.span.clone(),
        }),
        _ => None,
    }
}

/// Slice 1c / 3c: recognize a literal const-arg expression shape at a
/// call-site bracket position. Integer / bool / char literals plus
/// `Unary { Neg, Integer }` (negative-integer literals) qualify.
fn is_literal_const_arg_expr(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Integer(_, _) | ExprKind::Bool(_) | ExprKind::CharLit(_) => true,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => matches!(operand.kind, ExprKind::Integer(_, _)),
        _ => false,
    }
}

/// Extract a literal integer / bool / char value from an `Expr` and
/// coerce to `i64` for the `ConstArg::Literal` shape. Used by the
/// slice-1c explicit-generic-args pre-binding at call sites.
/// Negative-integer literals (`-1`) parse as `Unary { Neg,
/// Integer(1) }`; recover the literal value here for the
/// `f[-1]()` call shape.
fn const_value_from_literal(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Integer(n, _) => Some(*n),
        ExprKind::Bool(b) => Some(*b as i64),
        ExprKind::CharLit(c) => Some(*c as i64),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => {
            if let ExprKind::Integer(n, _) = &operand.kind {
                Some(-*n)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Replace `ConstArg::ConstParam(name)` with `ConstArg::ConstVar(id)`
/// for every name in the substitution map. Used by signature
/// instantiation at call sites (slice 3b).
fn substitute_const_param_to_var(
    arg: &ConstArg,
    name_to_const_id: &HashMap<String, ConstVarId>,
) -> ConstArg {
    match arg {
        ConstArg::ConstParam(name) => match name_to_const_id.get(name) {
            Some(&id) => ConstArg::ConstVar(id),
            None => arg.clone(),
        },
        _ => arg.clone(),
    }
}

/// Walk `ty` and replace every `Type::TypeVar(id)` with the
/// substitution recorded for `id` in `substitutions`, recursively
/// resolving chains. Unresolved TypeVars are converted back to
/// `Type::TypeParam(original_name)` via `id_to_name` so the existing
/// `find_unbound_type_param` (slice 2a) detects them at the consuming
/// context. Each substitution result is itself recursively resolved so
/// `?M0 → ?M1 → i32` collapses to `i32`.
fn resolve_type_vars(
    ty: &Type,
    substitutions: &HashMap<TypeVarId, Type>,
    id_to_name: &HashMap<TypeVarId, String>,
    const_substitutions: &HashMap<ConstVarId, ConstArg>,
    const_id_to_name: &HashMap<ConstVarId, String>,
) -> Type {
    let recur = |t: &Type| {
        resolve_type_vars(
            t,
            substitutions,
            id_to_name,
            const_substitutions,
            const_id_to_name,
        )
    };
    match ty {
        Type::TypeVar(id) => {
            if let Some(resolved) = substitutions.get(id) {
                recur(resolved)
            } else if let Some(name) = id_to_name.get(id) {
                Type::TypeParam(name.clone())
            } else {
                ty.clone()
            }
        }
        Type::Tuple(es) => Type::Tuple(es.iter().map(&recur).collect()),
        Type::Array { element, size } => Type::Array {
            element: Box::new(recur(element)),
            size: resolve_const_arg(size, const_substitutions, const_id_to_name),
        },
        Type::Slice { element, mutable } => Type::Slice {
            element: Box::new(recur(element)),
            mutable: *mutable,
        },
        Type::Ref(inner) => Type::Ref(Box::new(recur(inner))),
        Type::MutRef(inner) => Type::MutRef(Box::new(recur(inner))),
        Type::Weak(inner) => Type::Weak(Box::new(recur(inner))),
        Type::Pointer { is_mut, inner } => Type::Pointer {
            is_mut: *is_mut,
            inner: Box::new(recur(inner)),
        },
        Type::Named { name, args } => Type::Named {
            name: name.clone(),
            args: args.iter().map(&recur).collect(),
        },
        Type::Function {
            params,
            return_type,
        } => Type::Function {
            params: params.iter().map(&recur).collect(),
            return_type: Box::new(recur(return_type)),
        },
        Type::OnceFunction {
            params,
            return_type,
        } => Type::OnceFunction {
            params: params.iter().map(&recur).collect(),
            return_type: Box::new(recur(return_type)),
        },
        _ => ty.clone(),
    }
}

/// Const-arg analog of `resolve_type_vars` for the `Type::Array.size`
/// position. `ConstVar(id)` resolves via `const_substitutions`;
/// unresolved vars convert back to `ConstParam(name)` via
/// `const_id_to_name` so `check_unsolved_const_param` (slice 3b
/// sub-step h) detects them at the consuming context. `Literal` and
/// `ConstParam` pass through unchanged.
fn resolve_const_arg(
    arg: &ConstArg,
    const_substitutions: &HashMap<ConstVarId, ConstArg>,
    const_id_to_name: &HashMap<ConstVarId, String>,
) -> ConstArg {
    match arg {
        ConstArg::ConstVar(id) => {
            if let Some(resolved) = const_substitutions.get(id) {
                resolve_const_arg(resolved, const_substitutions, const_id_to_name)
            } else if let Some(name) = const_id_to_name.get(id) {
                ConstArg::ConstParam(name.clone())
            } else {
                arg.clone()
            }
        }
        _ => arg.clone(),
    }
}

/// Resolve only the top-level `Type::TypeVar(id)` chain — leaves
/// nested TypeVars in compound types untouched. Used by `unify_types`
/// to peel one level of indirection before structurally comparing.
fn resolve_type_var_top(ty: &Type, substitutions: &HashMap<TypeVarId, Type>) -> Type {
    match ty {
        Type::TypeVar(id) => {
            if let Some(resolved) = substitutions.get(id) {
                resolve_type_var_top(resolved, substitutions)
            } else {
                ty.clone()
            }
        }
        _ => ty.clone(),
    }
}

/// Structural unification with substitution side-effects. When either
/// side is an unresolved `TypeVar`, record the binding and return
/// success; otherwise recurse structurally on compound types
/// (tuple/named/function/array/slice/ref/etc) and fall through to
/// `types_compatible` for terminal cases. Symmetric: order of `a`/`b`
/// doesn't change the result, except that the chosen substitution
/// always points the unresolved TypeVar at its sibling. Returns false
/// if the structural shapes don't match (caller's `check_assignable`
/// pass surfaces the diagnostic; this function is silent so a single
/// shape mismatch at depth doesn't poison higher-level recovery).
fn unify_types(
    a: &Type,
    b: &Type,
    substitutions: &mut HashMap<TypeVarId, Type>,
    const_substitutions: &mut HashMap<ConstVarId, ConstArg>,
) -> bool {
    let a = resolve_type_var_top(a, substitutions);
    let b = resolve_type_var_top(b, substitutions);
    match (&a, &b) {
        (Type::TypeVar(id_a), Type::TypeVar(id_b)) if id_a == id_b => true,
        (Type::TypeVar(id), _) => {
            substitutions.insert(*id, b.clone());
            true
        }
        (_, Type::TypeVar(id)) => {
            substitutions.insert(*id, a.clone());
            true
        }
        (Type::Error, _) | (_, Type::Error) => true,
        (Type::Tuple(as_), Type::Tuple(bs)) if as_.len() == bs.len() => as_
            .iter()
            .zip(bs.iter())
            .all(|(x, y)| unify_types(x, y, substitutions, const_substitutions)),
        (Type::Named { name: an, args: aa }, Type::Named { name: bn, args: bb })
            if an == bn && aa.len() == bb.len() =>
        {
            aa.iter()
                .zip(bb.iter())
                .all(|(x, y)| unify_types(x, y, substitutions, const_substitutions))
        }
        (Type::Ref(x), Type::Ref(y))
        | (Type::MutRef(x), Type::MutRef(y))
        | (Type::Weak(x), Type::Weak(y)) => unify_types(x, y, substitutions, const_substitutions),
        (
            Type::Array {
                element: xe,
                size: xs,
            },
            Type::Array {
                element: ye,
                size: ys,
            },
        ) => {
            // Const generics slice 3b: route the size comparison
            // through `unify_const_args` so `ConstArg::ConstVar` can
            // bind during call-site inference.
            unify_const_args(xs, ys, const_substitutions)
                && unify_types(xe, ye, substitutions, const_substitutions)
        }
        (
            Type::Slice {
                element: xe,
                mutable: xm,
            },
            Type::Slice {
                element: ye,
                mutable: ym,
            },
        ) if xm == ym => unify_types(xe, ye, substitutions, const_substitutions),
        (
            Type::Function {
                params: xp,
                return_type: xr,
            },
            Type::Function {
                params: yp,
                return_type: yr,
            },
        ) if xp.len() == yp.len() => {
            xp.iter()
                .zip(yp.iter())
                .all(|(x, y)| unify_types(x, y, substitutions, const_substitutions))
                && unify_types(xr, yr, substitutions, const_substitutions)
        }
        (
            Type::OnceFunction {
                params: xp,
                return_type: xr,
            },
            Type::OnceFunction {
                params: yp,
                return_type: yr,
            },
        ) if xp.len() == yp.len() => {
            xp.iter()
                .zip(yp.iter())
                .all(|(x, y)| unify_types(x, y, substitutions, const_substitutions))
                && unify_types(xr, yr, substitutions, const_substitutions)
        }
        // Terminal / cross-shape cases handled by the existing
        // structural compatibility check (covers integer-coercion,
        // never, slice/vec coercions, etc).
        _ => types_compatible(&a, &b),
    }
}

/// Const-arg unification (const generics slice 3b — fork G1). Mirrors
/// `unify_types` with `(ConstVar, other)` bind-and-succeed semantics.
/// `Literal`/`Literal` requires equality; `ConstParam`/`ConstParam`
/// requires name equality (post-instantiation these should be rare —
/// the inference solver substitutes `ConstParam` → `ConstVar` at
/// signature minting). Returns false on incompatible shapes; the
/// caller surfaces the diagnostic.
fn unify_const_args(
    a: &ConstArg,
    b: &ConstArg,
    const_substitutions: &mut HashMap<ConstVarId, ConstArg>,
) -> bool {
    let a = resolve_const_var_top(a, const_substitutions);
    let b = resolve_const_var_top(b, const_substitutions);
    match (&a, &b) {
        (ConstArg::ConstVar(id_a), ConstArg::ConstVar(id_b)) if id_a == id_b => true,
        (ConstArg::ConstVar(id), _) => {
            const_substitutions.insert(*id, b.clone());
            true
        }
        (_, ConstArg::ConstVar(id)) => {
            const_substitutions.insert(*id, a.clone());
            true
        }
        (ConstArg::Literal(x), ConstArg::Literal(y)) => x == y,
        (ConstArg::ConstParam(name_a), ConstArg::ConstParam(name_b)) => name_a == name_b,
        _ => false,
    }
}

/// One-step resolution of `ConstArg::ConstVar(id)` against the
/// substitutions map. Mirrors `resolve_type_var_top` for the const-arg
/// metavariable substrate.
fn resolve_const_var_top(
    arg: &ConstArg,
    const_substitutions: &HashMap<ConstVarId, ConstArg>,
) -> ConstArg {
    match arg {
        ConstArg::ConstVar(id) => match const_substitutions.get(id) {
            Some(inner) => resolve_const_var_top(inner, const_substitutions),
            None => arg.clone(),
        },
        _ => arg.clone(),
    }
}

/// Walk `ty` for a `TypeParam(name)` whose name is **not** in
/// `in_scope`. Returns the first such name. Used by the unsolved-T
/// diagnostic (item 131 sub-step 2a) at synthesis-mode let bindings:
/// any TypeParam that didn't get pinned by arguments and doesn't
/// belong to an enclosing function/impl generic is unsolved at this
/// site.
/// Walk `ty` for a `ConstArg::ConstParam(name)` whose name isn't in
/// `in_scope`. Returns the first such name. Mirrors
/// `find_unbound_type_param` for the const-arg metavariable substrate
/// (const generics slice 3b — fork G2). Used by
/// `check_unsolved_const_param` at synthesis-mode let bindings: any
/// const param that wasn't pinned by arguments and doesn't belong to
/// an enclosing function/impl generic surfaces as unsolved.
fn find_unbound_const_param<'a>(ty: &'a Type, in_scope: &HashSet<&str>) -> Option<&'a str> {
    fn check_arg<'a>(arg: &'a ConstArg, in_scope: &HashSet<&str>) -> Option<&'a str> {
        match arg {
            ConstArg::ConstParam(name) => {
                if in_scope.contains(name.as_str()) {
                    None
                } else {
                    Some(name.as_str())
                }
            }
            _ => None,
        }
    }
    match ty {
        Type::Array { element, size } => {
            check_arg(size, in_scope).or_else(|| find_unbound_const_param(element, in_scope))
        }
        Type::Tuple(elems) => elems
            .iter()
            .find_map(|e| find_unbound_const_param(e, in_scope)),
        Type::Slice { element, .. } => find_unbound_const_param(element, in_scope),
        Type::Ref(inner) | Type::MutRef(inner) | Type::Weak(inner) => {
            find_unbound_const_param(inner, in_scope)
        }
        Type::Pointer { inner, .. } => find_unbound_const_param(inner, in_scope),
        Type::Named { args, .. } => args
            .iter()
            .find_map(|a| find_unbound_const_param(a, in_scope)),
        Type::Function {
            params,
            return_type,
        }
        | Type::OnceFunction {
            params,
            return_type,
        } => params
            .iter()
            .find_map(|p| find_unbound_const_param(p, in_scope))
            .or_else(|| find_unbound_const_param(return_type, in_scope)),
        _ => None,
    }
}

fn find_unbound_type_param<'a>(ty: &'a Type, in_scope: &HashSet<&str>) -> Option<&'a str> {
    match ty {
        Type::TypeParam(name) => {
            if in_scope.contains(name.as_str()) {
                None
            } else {
                Some(name.as_str())
            }
        }
        Type::Tuple(elems) => elems
            .iter()
            .find_map(|e| find_unbound_type_param(e, in_scope)),
        Type::Array { element, .. } | Type::Slice { element, .. } => {
            find_unbound_type_param(element, in_scope)
        }
        Type::Ref(inner) | Type::MutRef(inner) | Type::Weak(inner) => {
            find_unbound_type_param(inner, in_scope)
        }
        Type::Pointer { inner, .. } => find_unbound_type_param(inner, in_scope),
        Type::Named { args, .. } => args
            .iter()
            .find_map(|a| find_unbound_type_param(a, in_scope)),
        Type::Function {
            params,
            return_type,
        }
        | Type::OnceFunction {
            params,
            return_type,
        } => params
            .iter()
            .find_map(|p| find_unbound_type_param(p, in_scope))
            .or_else(|| find_unbound_type_param(return_type, in_scope)),
        Type::AssocProjection { param, .. } => {
            if in_scope.contains(param.as_str()) {
                None
            } else {
                Some(param.as_str())
            }
        }
        _ => None,
    }
}

/// Directional subsumption: can a value of type `sub_ty` be used where
/// `super_ty` is expected? Used by `check_assignable` (item 131 sub-step 3).
///
/// Differs from `types_compatible` in two ways:
///   1. **Function-type variance** — params are contravariant
///      (`is_subtype(b_p, s_p)` per pair) and return is covariant
///      (`is_subtype(s_r, b_r)`). For Kāra v1 with no user-declared
///      subtyping, this is observationally equivalent to the symmetric
///      check on the body — the variance plumbing is foundational for
///      future subtyping (refinement narrowing, declarable trait variance).
///   2. **`Fn → OnceFn` upward subtyping** — a `Type::Function` value
///      satisfies a `Type::OnceFunction` slot (callable-once is a weaker
///      contract than repeatedly-callable). The reverse direction is
///      rejected here and produces the focused E0235 (`OnceFnIntoFnSlot`)
///      diagnostic via `check_assignable`'s `is_once_into_fn_shape` arm.
///
/// Borrow forms (`Ref`/`MutRef`) recurse through `is_subtype` so the
/// function-arm subsumption applies under references too. Everything
/// else delegates to `types_compatible`; deep variance on nested
/// compound types (`Vec[Fn(...)]` → `Vec[OnceFn(...)]`, tuple element
/// subsumption) is intentionally out of scope until Kāra introduces
/// declarable variance for user-defined generics.
///
/// Effect-set variance (the third leg of design.md § Type Inference's
/// subsumption rule) is deferred until phase-3 lands effect variables
/// on `Type::Function` — the type lacks an effect-set field today.
fn is_subtype(super_ty: &Type, sub_ty: &Type) -> bool {
    if super_ty == sub_ty {
        return true;
    }
    match (super_ty, sub_ty) {
        (
            Type::Function {
                params: sp,
                return_type: sr,
            },
            Type::Function {
                params: bp,
                return_type: br,
            },
        )
        | (
            Type::OnceFunction {
                params: sp,
                return_type: sr,
            },
            Type::OnceFunction {
                params: bp,
                return_type: br,
            },
        )
        | (
            Type::OnceFunction {
                params: sp,
                return_type: sr,
            },
            Type::Function {
                params: bp,
                return_type: br,
            },
        ) => {
            sp.len() == bp.len()
                && sp.iter().zip(bp.iter()).all(|(s, b)| is_subtype(b, s))
                && is_subtype(sr, br)
        }
        (Type::Ref(s), Type::Ref(b)) | (Type::MutRef(s), Type::MutRef(b)) => is_subtype(s, b),
        // Owned-to-ref coercion: a `ref T` slot accepts an owned `T` value.
        // design.md Feature 4 Part 3 § Explicit ref for Borrow Returns — the
        // borrow source is inferred from context (function-return tail, call
        // arg, field init, let RHS) and the ownership checker handles the
        // borrow-not-consume semantics via the slot's declared mode. The
        // reverse (`ref T` → owned `T`) requires explicit deref and is not
        // accepted here; the `MutRef` sub-case is also excluded so owned-to-
        // mut-ref still needs the explicit `mut` call-site marker.
        (Type::Ref(inner), sub) if !matches!(sub, Type::Ref(_) | Type::MutRef(_)) => {
            is_subtype(inner, sub)
        }
        _ => types_compatible(super_ty, sub_ty),
    }
}

/// LB3 — labeled-block LUB inference helper.
///
/// Compute the labeled-block expression's type by joining the tail
/// expression's type with each `break label expr` value-type collected
/// during body inference. Rules:
/// - `Type::Never` is the unit element: a `break label expr` after which
///   control cannot fall through (or vice versa) doesn't constrain the
///   block type.
/// - `Type::Error` propagates (any error participating in the LUB poisons
///   the result so cascading errors don't fire).
/// - All non-`Never` participants must be pairwise `types_compatible`;
///   otherwise the block type collapses to `Type::Error`. Diagnosing the
///   actual mismatch is left to the surrounding context (the
///   labeled-block expression participates as an operand and the parent
///   site emits the focused diagnostic).
///
/// The helper is deliberately conservative — it does not perform unification
/// across type metavariables. The current `if`-arm joining path uses the
/// same one-shot `types_compatible` check, so this is consistent with the
/// rest of the typechecker. A more aggressive `lub_n` over metavariables
/// is a future refactor (out-of-scope for this slice).
fn lub_block_type(tail: Type, breaks: &[Type]) -> Type {
    // Pick the first non-Never as the candidate.
    let mut candidate: Option<Type> = if tail != Type::Never {
        Some(tail.clone())
    } else {
        None
    };
    for b in breaks {
        if *b == Type::Never {
            continue;
        }
        if *b == Type::Error {
            return Type::Error;
        }
        match &candidate {
            None => candidate = Some(b.clone()),
            Some(c) => {
                if *c == Type::Error {
                    return Type::Error;
                }
                if !types_compatible(c, b) {
                    return Type::Error;
                }
            }
        }
    }
    candidate.unwrap_or(tail)
}

fn types_compatible(a: &Type, b: &Type) -> bool {
    if a == b {
        return true;
    }
    match (a, b) {
        (Type::Error, _) | (_, Type::Error) => true,
        (Type::Never, _) | (_, Type::Never) => true,
        // Unresolved type parameters and associated type projections are
        // treated as permissive — they appear when a generic enum constructor
        // leaves an argument unconstrained (e.g. `let x: Option[i64] = None`
        // — the `None` arm has no value from which to solve `T`). Equivalent
        // to the `TypeVar` handling below.
        (Type::TypeParam(_), _) | (_, Type::TypeParam(_)) => true,
        (Type::AssocProjection { .. }, _) | (_, Type::AssocProjection { .. }) => true,
        // Pragmatic: integer literals (i64) compatible with any int/uint
        (Type::Int(_), Type::Int(_)) => true,
        (Type::UInt(_), Type::UInt(_)) => true,
        (Type::Int(_), Type::UInt(_)) | (Type::UInt(_), Type::Int(_)) => true,
        (Type::Float(_), Type::Float(_)) => true,
        // Implicit widening: int/uint ↔ float (pragmatic, bidirectional for compatibility checks)
        (Type::Int(_), Type::Float(_)) | (Type::Float(_), Type::Int(_)) => true,
        (Type::UInt(_), Type::Float(_)) | (Type::Float(_), Type::UInt(_)) => true,
        (Type::Tuple(a_types), Type::Tuple(b_types)) => {
            a_types.len() == b_types.len()
                && a_types
                    .iter()
                    .zip(b_types.iter())
                    .all(|(a, b)| types_compatible(a, b))
        }
        (
            Type::Named {
                name: a_name,
                args: a_args,
            },
            Type::Named {
                name: b_name,
                args: b_args,
            },
        ) => {
            a_name == b_name
                && a_args.len() == b_args.len()
                && a_args
                    .iter()
                    .zip(b_args.iter())
                    .all(|(a, b)| types_compatible(a, b))
        }
        (Type::Ref(a), Type::Ref(b)) => types_compatible(a, b),
        (Type::MutRef(a), Type::MutRef(b)) => types_compatible(a, b),
        (
            Type::Array {
                element: a_el,
                size: a_sz,
            },
            Type::Array {
                element: b_el,
                size: b_sz,
            },
        ) => a_sz == b_sz && types_compatible(a_el, b_el),
        // Slice[T] → Slice[T] with compatible elements (identity case is
        // covered above by `a == b`; this arm handles e.g. integer
        // compatibility on the element type).
        (
            Type::Slice {
                element: a_el,
                mutable: a_mut,
            },
            Type::Slice {
                element: b_el,
                mutable: b_mut,
            },
        ) => {
            // Read-only slot accepts mutable source (reborrow as read-only).
            // Mutable slot rejects read-only source.
            let mut_ok = !*a_mut || *b_mut;
            mut_ok && types_compatible(a_el, b_el)
        }
        // Coercion at call boundaries: `Slice[T]` accepts `Vec[T]` / `Array[T, N]`,
        // and their `ref` borrows. One-directional — the reverse is not compatible.
        // See design.md § Slices.
        (
            Type::Slice {
                element: slice_el,
                mutable: false,
            },
            Type::Array {
                element: arr_el, ..
            },
        ) => types_compatible(slice_el, arr_el),
        (
            Type::Slice {
                element: slice_el,
                mutable: false,
            },
            Type::Named { name, args },
        ) if name == "Vec" && args.len() == 1 => types_compatible(slice_el, &args[0]),
        (
            Type::Slice {
                element: slice_el,
                mutable: false,
            },
            Type::Ref(inner),
        ) => match inner.as_ref() {
            Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                types_compatible(slice_el, &args[0])
            }
            Type::Array {
                element: arr_el, ..
            } => types_compatible(slice_el, arr_el),
            Type::Slice {
                element: inner_el, ..
            } => types_compatible(slice_el, inner_el),
            _ => false,
        },
        // `mut Slice[T]` at the slot — accepts `mut ref Vec[T]` / `mut ref Array[T, N]`
        // / `mut Slice[T]` itself (already covered by the generic Slice→Slice arm),
        // and also owned `Vec[T]` / `Array[T, N]` at call boundaries. The owned-source
        // case requires a `mut` marker at the call site (enforced separately by
        // `check_call_site_marker`); check_assignable is type-level only.
        // Read-only sources (`ref Vec`, `Slice{mutable:false}`) do not upgrade.
        (
            Type::Slice {
                element: slice_el,
                mutable: true,
            },
            Type::MutRef(inner),
        ) => match inner.as_ref() {
            Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                types_compatible(slice_el, &args[0])
            }
            Type::Array {
                element: arr_el, ..
            } => types_compatible(slice_el, arr_el),
            Type::Slice {
                element: inner_el,
                mutable: true,
            } => types_compatible(slice_el, inner_el),
            _ => false,
        },
        (
            Type::Slice {
                element: slice_el,
                mutable: true,
            },
            Type::Array {
                element: arr_el, ..
            },
        ) => types_compatible(slice_el, arr_el),
        (
            Type::Slice {
                element: slice_el,
                mutable: true,
            },
            Type::Named { name, args },
        ) if name == "Vec" && args.len() == 1 => types_compatible(slice_el, &args[0]),
        (
            Type::Function {
                params: a_p,
                return_type: a_r,
            },
            Type::Function {
                params: b_p,
                return_type: b_r,
            },
        )
        | (
            Type::OnceFunction {
                params: a_p,
                return_type: a_r,
            },
            Type::OnceFunction {
                params: b_p,
                return_type: b_r,
            },
        ) => {
            a_p.len() == b_p.len()
                && a_p
                    .iter()
                    .zip(b_p.iter())
                    .all(|(a, b)| types_compatible(a, b))
                && types_compatible(a_r, b_r)
        }
        (Type::Shared(a_name), Type::Shared(b_name)) => a_name == b_name,
        (Type::Rc(a_inner), Type::Rc(b_inner)) => types_compatible(a_inner, b_inner),
        (Type::Arc(a_inner), Type::Arc(b_inner)) => types_compatible(a_inner, b_inner),
        // No (Rc, Arc) / (Arc, Rc) cross arms: `Rc[T]` is not assignable
        // to `Arc[T]` and vice versa per design.md § RC integration. The
        // value-site auto-promotion in `OwnershipChecker::promote_rc_to_arc`
        // is the only path that crosses the boundary, and it rewrites the
        // value's representation, not the type — so type-level compat
        // stays strict.
        _ => false,
    }
}

// ── Type Environment ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StructInfo {
    pub generic_params: Vec<String>,
    pub fields: Vec<(String, Type, bool)>, // (name, type, is_pub)
    pub derived_traits: HashSet<String>,
    pub no_rc: bool,
    pub is_shared: bool,
}

#[derive(Debug, Clone)]
pub struct EnumInfo {
    pub generic_params: Vec<String>,
    pub variants: Vec<(String, VariantTypeInfo)>,
    pub derived_traits: HashSet<String>,
    pub is_shared: bool,
}

#[derive(Debug, Clone)]
pub struct FunctionSig {
    pub generic_params: Vec<String>,
    pub param_names: Vec<Option<String>>,
    pub params: Vec<Type>,
    pub return_type: Type,
    /// Where-clause constraints carried alongside the signature
    /// (const generics slice 3c). `None` when the function declares
    /// no where clause. The call-site discharge engine walks
    /// `WhereConstraint::ConstPredicate(_)` entries here and
    /// evaluates each predicate against the bound const-args.
    pub where_clause: Option<WhereClause>,
}

#[derive(Debug, Clone)]
pub struct ImplInfo {
    pub target_type: String,
    /// Type arguments of the impl target (`impl Foo for Option[Ordering]`
    /// → `[Type::Named { name: "Ordering", args: [] }]`). Empty means the
    /// impl is generic-on-name — it applies to every instantiation of
    /// `target_type` (the status quo for every impl that pre-dates the
    /// Theme-4 slice). Non-empty means the impl is specialized to the
    /// listed concrete instantiation; lookup matches iff the call site's
    /// args vector-equal the stored args. `env_add_impl` populates this
    /// only when every recursive arg is fully concrete (no `TypeParam`
    /// or `TypeVar`) — generic impls (`impl Foo for Option[T]`) keep
    /// `target_args.is_empty()`.
    pub target_args: Vec<Type>,
    pub trait_name: Option<String>,
    pub methods: HashMap<String, FunctionSig>,
    /// Impl-level type-parameter declarations including their inline
    /// bounds (`impl[T: Ord] Foo for Bar[T]`). Populated by
    /// `env_add_impl`; consumed by the conditional-impl-filtering pass
    /// (slice 1 of the method-resolution CR — see
    /// `phase-4-interpreter.md`) to decide whether an impl applies at a
    /// given call site. `None` for unconditional impls (`impl Foo for
    /// Bar { ... }`).
    pub generic_params: Option<GenericParams>,
    /// Impl-level `where` clause predicates. Same role as
    /// `generic_params`'s inline bounds for the discharge engine; the
    /// two compose additively (every predicate must discharge for the
    /// impl to apply).
    pub where_clause: Option<WhereClause>,
}

/// Associated type names declared by a trait.
#[derive(Debug, Clone)]
pub struct TraitInfo {
    pub assoc_types: Vec<String>,
    /// Names of supertraits declared in `trait Foo: Bar + Baz`.
    pub supertraits: Vec<String>,
}

pub struct TypeEnv {
    pub structs: HashMap<String, StructInfo>,
    pub enums: HashMap<String, EnumInfo>,
    /// Derived traits for each `distinct type` declaration.
    pub distinct_types: HashMap<String, HashSet<String>>,
    /// Names of opaque foreign types declared inside `unsafe extern "ABI" { ... }`
    /// blocks (`type Foo;`). Consulted by `lower_type_expr_inner` for
    /// `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`, by `infer_field_access` for
    /// `E_OPAQUE_TYPE_NO_FIELDS`, and by `env_add_impl` for
    /// `E_OPAQUE_TYPE_NO_INHERENT_OR_TRAIT_IMPLS` (slice 1b). Slice 1
    /// (registration only) shipped 2026-05-14; slice 1b (use-site
    /// precision) shipped alongside.
    pub opaque_foreign_types: HashSet<String>,
    pub functions: HashMap<String, FunctionSig>,
    pub constants: HashMap<String, Type>,
    pub type_aliases: HashMap<String, Type>,
    pub traits: HashMap<String, TraitInfo>,
    /// Names of declared trait aliases (`trait NAME = bound1 + ...;`).
    /// Recognized at parse + resolver time; the typechecker emits
    /// `E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET` at every use site as a v1
    /// stub. Bound substitution lands in P1 (see `docs/deferred.md` §
    /// Trait Aliases — Expansion).
    pub trait_aliases: HashSet<String>,
    /// Names of declared marker traits (`marker trait NAME;`). Marker
    /// traits register in `traits` alongside ordinary traits so bound
    /// resolution and impl coherence work uniformly; this side-set
    /// records the marker-ness so impl-body checks (no methods allowed)
    /// can look it up. v60 item 55 / design.md § Marker Traits.
    pub marker_traits: HashSet<String>,
    pub impls: Vec<ImplInfo>,
    /// Indices into `impls` keyed by trait name. Trait-less inherent impls
    /// are not indexed here.
    pub impls_by_trait: HashMap<String, Vec<usize>>,
    /// Associated type bindings from impl blocks. Key is `(concrete_type_name,
    /// assoc_type_name)`; value is the concrete type. E.g. `impl Iterator for
    /// Vec[i32]` with `type Item = i32` inserts `("Vec", "Item") → i32`.
    /// Used by `resolve_assoc_projections` to substitute `T.Item` after `T`
    /// is solved to a concrete named type.
    pub impl_assoc_types: HashMap<(String, String), Type>,
    /// Names of functions declared with `#[compiler_builtin]` in stdlib
    /// source (CR-202 slice 2). The signature still lives in `functions`
    /// — the entry here marks the function as having its body replaced by
    /// Rust dispatch, so `check_items` skips body type-checking and the
    /// interpreter knows not to evaluate the placeholder body. Slice 1's
    /// resolver gate (`E0237`) prevents user code from getting entries
    /// into this set.
    pub compiler_builtins: HashSet<String>,
    #[allow(dead_code)]
    next_type_var: u32,
    #[allow(dead_code)]
    substitutions: HashMap<TypeVarId, Type>,
    /// Const-arg metavar counter, parallel to `next_type_var`. Bumped
    /// when `instantiate_signature_with_fresh_vars` mints a fresh
    /// `ConstVarId` per unique const-param name in a signature (const
    /// generics slice 3b).
    #[allow(dead_code)]
    next_const_var: u32,
    /// Const-arg metavar substitutions, parallel to `substitutions`.
    /// Populated by `unify_const_args` at call sites; consumed by
    /// `resolve_type_vars` (Array arm) to substitute `ConstArg::ConstVar`
    /// with its bound value.
    #[allow(dead_code)]
    const_substitutions: HashMap<ConstVarId, ConstArg>,
}

impl TypeEnv {
    fn new() -> Self {
        TypeEnv {
            structs: HashMap::new(),
            enums: HashMap::new(),
            distinct_types: HashMap::new(),
            opaque_foreign_types: HashSet::new(),
            functions: HashMap::new(),
            constants: HashMap::new(),
            type_aliases: HashMap::new(),
            traits: HashMap::new(),
            trait_aliases: HashSet::new(),
            marker_traits: HashSet::new(),
            impls: Vec::new(),
            impls_by_trait: HashMap::new(),
            impl_assoc_types: HashMap::new(),
            compiler_builtins: HashSet::new(),
            next_type_var: 0,
            substitutions: HashMap::new(),
            next_const_var: 0,
            const_substitutions: HashMap::new(),
        }
    }

    /// Push an impl into the env and update the trait index.
    pub fn add_impl(&mut self, imp: ImplInfo) -> usize {
        let idx = self.impls.len();
        if let Some(t) = imp.trait_name.clone() {
            self.impls_by_trait.entry(t).or_default().push(idx);
        }
        self.impls.push(imp);
        idx
    }

    /// Look up the impl of `trait_name` for `target_type`. The match
    /// rule is the Theme-4 args-aware shape: a stored impl's
    /// `target_args` matches the call site iff the stored args are
    /// empty (impl is generic-on-name, applies to any instantiation)
    /// OR they vector-equal the call-site args. Callers without any
    /// generic-arg context pass `&[]`, which selectively sees only
    /// generic-on-name impls (specialized impls become invisible to
    /// these callers — correct conservative default).
    pub fn find_impl(
        &self,
        trait_name: &str,
        target_type: &str,
        target_args: &[Type],
    ) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get(trait_name)?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target_type && impl_args_match(&imp.target_args, target_args)
            })
    }

    pub fn has_impl(&self, trait_name: &str, target_type: &str, target_args: &[Type]) -> bool {
        self.find_impl(trait_name, target_type, target_args)
            .is_some()
    }

    /// Look up a method by name on `target_type` from `impls`, preferring
    /// inherent methods over trait methods per design.md § Method Resolution
    /// Step 3. Returns the first inherent impl's method if any matches;
    /// otherwise the first trait impl's method. First-match within each tier
    /// — multi-inherent and multi-trait ambiguity detection is deferred to
    /// the step-4 work tracked in phase-4-interpreter.md. The `target_args`
    /// parameter applies the Theme-4 args-match rule (see
    /// [`Self::find_impl`]).
    pub fn find_method(
        &self,
        target_type: &str,
        target_args: &[Type],
        method: &str,
    ) -> Option<&FunctionSig> {
        let mut inherent: Option<&FunctionSig> = None;
        let mut trait_method: Option<&FunctionSig> = None;
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
                continue;
            }
            let Some(sig) = imp.methods.get(method) else {
                continue;
            };
            if imp.trait_name.is_none() {
                if inherent.is_none() {
                    inherent = Some(sig);
                }
            } else if trait_method.is_none() {
                trait_method = Some(sig);
            }
        }
        inherent.or(trait_method)
    }

    /// Collect every method name registered on `target_type` across both
    /// inherent and trait impls. Used for `did you mean` typo suggestions
    /// when method resolution falls through (design.md § Method Resolution
    /// Step 7). The `target_args` parameter applies the Theme-4 args-match
    /// rule (see [`Self::find_impl`]).
    pub fn collect_method_names(&self, target_type: &str, target_args: &[Type]) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
                continue;
            }
            for name in imp.methods.keys() {
                if !names.iter().any(|n| n == name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    /// Conditional-impl-aware variant of [`Self::find_method`] — slice 1 of
    /// the method-resolution CR (see `phase-4-interpreter.md`). Filters
    /// candidates by [`Self::impl_bounds_discharge`] before applying the
    /// inherent-beats-trait priority. Pass `target_args` from the receiver's
    /// `Type::Named { args }` so impl-level bounds (`impl[T: Ord] Foo for
    /// Bar[T]`) discharge against the concrete instantiation.
    pub fn find_method_with_args(
        &self,
        target_type: &str,
        target_args: &[Type],
        method: &str,
    ) -> Option<&FunctionSig> {
        let mut inherent: Option<&FunctionSig> = None;
        let mut trait_method: Option<&FunctionSig> = None;
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
                continue;
            }
            let Some(sig) = imp.methods.get(method) else {
                continue;
            };
            if !self.impl_bounds_discharge(imp, target_args) {
                continue;
            }
            if imp.trait_name.is_none() {
                if inherent.is_none() {
                    inherent = Some(sig);
                }
            } else if trait_method.is_none() {
                trait_method = Some(sig);
            }
        }
        inherent.or(trait_method)
    }

    /// Slice 3 of the method-resolution CR — all-candidates variant of
    /// [`Self::find_method_with_args`]. Returns every impl that matches
    /// `target_type` + `method` after the conditional-impl-filtering pass
    /// (`impl_bounds_discharge`), partitioned by inherent-vs-trait priority:
    /// if any inherent impls match, only those are returned; otherwise the
    /// surviving trait impls are returned. Each entry is `(&ImplInfo,
    /// &FunctionSig)` so callers can render the source impl in ambiguity
    /// diagnostics. The returned vec preserves source order.
    ///
    /// A length-1 result is the dispatch-normally case; length ≥ 2 is the
    /// ambiguity case (item 4 of the parent CR — see
    /// `phase-4-interpreter.md`).
    pub fn find_methods_with_args(
        &self,
        target_type: &str,
        target_args: &[Type],
        method: &str,
    ) -> Vec<(&ImplInfo, &FunctionSig)> {
        let mut inherent: Vec<(&ImplInfo, &FunctionSig)> = Vec::new();
        let mut trait_methods: Vec<(&ImplInfo, &FunctionSig)> = Vec::new();
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
                continue;
            }
            let Some(sig) = imp.methods.get(method) else {
                continue;
            };
            if !self.impl_bounds_discharge(imp, target_args) {
                continue;
            }
            if imp.trait_name.is_none() {
                inherent.push((imp, sig));
            } else {
                trait_methods.push((imp, sig));
            }
        }
        if !inherent.is_empty() {
            inherent
        } else {
            trait_methods
        }
    }

    /// Slice 1 of the method-resolution CR — conditional impl filtering.
    /// Returns `true` when an impl applies at the call site whose receiver
    /// type instantiates with `target_args`. Unconditional impls (no
    /// `generic_params`) discharge trivially. Conditional impls
    /// (`impl[T: Ord] Foo for Bar[T]` and `where`-clause variants)
    /// substitute each impl-level type parameter with its concrete arg
    /// from `target_args` and check that every declared bound holds.
    /// Walks the supertrait graph, so `T: Ord` discharges against any
    /// type that impls `Ord` directly OR impls a trait whose supertrait
    /// closure reaches `Ord`.
    ///
    /// Out of scope for slice 1 (see roadmap in `phase-4-interpreter.md`):
    /// associated-type bounds (`T: Iterator<Item=i32>`), bounds with
    /// generic args on the trait (`T: Foo[U]` — only the path-tail trait
    /// name is consulted today), and the deeper "impl on a specific
    /// type-argument instantiation" key extension that unblocks
    /// `impl Option[Ordering]`.
    pub fn impl_bounds_discharge(&self, imp: &ImplInfo, target_args: &[Type]) -> bool {
        let Some(gp) = &imp.generic_params else {
            // No impl-level generic params → no inline bounds; the where
            // clause (if present) couldn't reference any names anyway.
            return true;
        };

        // Substitution map: impl-level generic-param name → concrete arg.
        let subs: std::collections::HashMap<&str, &Type> = gp
            .params
            .iter()
            .zip(target_args.iter())
            .map(|(p, a)| (p.name.as_str(), a))
            .collect();

        // Inline bounds on each generic param.
        for param in &gp.params {
            if param.bounds.is_empty() {
                continue;
            }
            let Some(&subst_ty) = subs.get(param.name.as_str()) else {
                // Receiver had fewer type args than the impl declares — can't
                // substitute the param to discharge its bounds. Conservative:
                // drop the candidate.
                return false;
            };
            for bound in &param.bounds {
                if !self.bound_satisfied(subst_ty, bound) {
                    return false;
                }
            }
        }

        // Where-clause `TypeBound` predicates: `where T: Bound`. Each
        // predicate's `type_name` is either an impl-level generic param
        // (substituted via `subs`) or a concrete type name (treated as
        // a bare `Type::Named` lookup against `env.impls`). The latter
        // covers cases like `where AnotherType: Ord` that name a type
        // unrelated to the impl's generics.
        if let Some(wc) = &imp.where_clause {
            for constraint in &wc.constraints {
                let WhereConstraint::TypeBound {
                    type_name, bounds, ..
                } = constraint
                else {
                    // `AssocTypeEq` predicates are out of scope for slice 1.
                    continue;
                };
                if bounds.is_empty() {
                    continue;
                }
                let owned;
                let target_ty: &Type = if let Some(&t) = subs.get(type_name.as_str()) {
                    t
                } else {
                    owned = Type::Named {
                        name: type_name.clone(),
                        args: Vec::new(),
                    };
                    &owned
                };
                for bound in bounds {
                    if !self.bound_satisfied(target_ty, bound) {
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Discharge `bound` against `ty`. The bound's last path segment names
    /// the trait. Walks the supertrait graph via [`Self::type_satisfies_trait`].
    fn bound_satisfied(&self, ty: &Type, bound: &TraitBound) -> bool {
        let Some(trait_name) = bound.path.last() else {
            return false;
        };
        let Some((ty_name, ty_args)) = impl_table_key(ty) else {
            // Type variables, function types, etc. don't appear in
            // `env.impls`. Conservative: drop. Generic call-site resolution
            // against bounds (item 8 of the method-resolution CR) is a
            // separate slice that handles `TypeParam` receivers properly.
            return false;
        };
        self.type_satisfies_trait(&ty_name, &ty_args, trait_name)
    }

    /// `true` when `ty_name` impls `trait_name` directly OR impls some
    /// trait whose supertrait closure reaches `trait_name`. The walk is
    /// finite — supertrait graphs are acyclic by construction. The
    /// `ty_args` parameter applies the Theme-4 args-match rule when
    /// scanning the impl table.
    fn type_satisfies_trait(&self, ty_name: &str, ty_args: &[Type], trait_name: &str) -> bool {
        if self.has_impl(trait_name, ty_name, ty_args) {
            return true;
        }
        let directly_impld_traits: Vec<&str> = self
            .impls
            .iter()
            .filter(|imp| imp.target_type == ty_name && impl_args_match(&imp.target_args, ty_args))
            .filter_map(|imp| imp.trait_name.as_deref())
            .collect();
        for start in directly_impld_traits {
            if self.supertrait_closure_contains(start, trait_name) {
                return true;
            }
        }
        false
    }

    /// BFS over the supertrait graph. `true` iff `target_trait` is reachable
    /// from `start_trait` through `TraitInfo::supertraits` edges.
    fn supertrait_closure_contains(&self, start_trait: &str, target_trait: &str) -> bool {
        use std::collections::{HashSet, VecDeque};
        let mut frontier: VecDeque<&str> = VecDeque::from([start_trait]);
        let mut seen: HashSet<&str> = HashSet::from([start_trait]);
        while let Some(name) = frontier.pop_front() {
            let Some(info) = self.traits.get(name) else {
                continue;
            };
            for st in &info.supertraits {
                if st == target_trait {
                    return true;
                }
                if seen.insert(st.as_str()) {
                    frontier.push_back(st);
                }
            }
        }
        false
    }

    /// All trait names reachable from `start_trait` through
    /// `TraitInfo::supertraits` edges, including `start_trait` itself, in
    /// BFS order. Slice 3.5 of the method-resolution CR — the candidate
    /// trait list for `self.method()` dispatch in a trait default body.
    fn supertrait_closure_traits(&self, start_trait: &str) -> Vec<String> {
        use std::collections::{HashSet, VecDeque};
        let mut order: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut frontier: VecDeque<String> = VecDeque::new();
        seen.insert(start_trait.to_string());
        frontier.push_back(start_trait.to_string());
        while let Some(name) = frontier.pop_front() {
            order.push(name.clone());
            if let Some(info) = self.traits.get(&name) {
                for st in &info.supertraits {
                    if seen.insert(st.clone()) {
                        frontier.push_back(st.clone());
                    }
                }
            }
        }
        order
    }

    /// Find a `From` impl mapping `source` → `target`. Disambiguates
    /// multiple `impl From[X] for T` impls for the same target by matching
    /// the `from` method's first parameter type against `source`. The
    /// `target_args` parameter applies the Theme-4 args-match rule
    /// (`&[]` selectively sees only generic-on-name From impls).
    pub fn find_from_impl(
        &self,
        source: &Type,
        target: &str,
        target_args: &[Type],
    ) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get("From")?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target
                    && impl_args_match(&imp.target_args, target_args)
                    && imp.methods.get("from").and_then(|sig| sig.params.first()) == Some(source)
            })
    }

    /// Find a `TryFrom` impl mapping `source` → `target`. Disambiguates
    /// multiple `impl TryFrom[X] for T` impls for the same target by matching
    /// the `try_from` method's first parameter type against `source`.
    /// The `target_args` parameter applies the Theme-4 args-match rule.
    pub fn find_tryfrom_impl(
        &self,
        source: &Type,
        target: &str,
        target_args: &[Type],
    ) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get("TryFrom")?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target
                    && impl_args_match(&imp.target_args, target_args)
                    && imp
                        .methods
                        .get("try_from")
                        .and_then(|sig| sig.params.first())
                        == Some(source)
            })
    }

    fn fresh_type_var(&mut self) -> Type {
        let id = TypeVarId(self.next_type_var);
        self.next_type_var += 1;
        Type::TypeVar(id)
    }

    #[allow(dead_code)]
    fn resolve_type(&self, ty: &Type) -> Type {
        match ty {
            Type::TypeVar(id) => {
                if let Some(resolved) = self.substitutions.get(id) {
                    self.resolve_type(resolved)
                } else {
                    ty.clone()
                }
            }
            _ => ty.clone(),
        }
    }

    #[allow(dead_code)]
    fn unify(&mut self, a: &Type, b: &Type) -> bool {
        let a = self.resolve_type(a);
        let b = self.resolve_type(b);
        match (&a, &b) {
            (Type::TypeVar(id), _) => {
                self.substitutions.insert(*id, b);
                true
            }
            (_, Type::TypeVar(id)) => {
                self.substitutions.insert(*id, a);
                true
            }
            (Type::Error, _) | (_, Type::Error) => true,
            _ => types_compatible(&a, &b),
        }
    }
}

// ── Local Type Scope ────────────────────────────────────────────

/// Mode for `closure_consumes_captured_non_copy`'s body walk: tracks
/// whether the current position is a Reading or Consuming context.
/// Mirrors `use_classifier::Mode` so the typechecker's capture-consume
/// detection lines up with the legacy ownership-side detector. Round
/// 12.44 (Step 2 — once-callability inference at construction).
#[derive(Copy, Clone, Eq, PartialEq)]
enum CaptureWalkMode {
    Reading,
    Consuming,
}

struct LocalTypeScope {
    scopes: Vec<HashMap<String, Type>>,
}

impl LocalTypeScope {
    fn new() -> Self {
        LocalTypeScope {
            scopes: vec![HashMap::new()],
        }
    }

    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn insert(&mut self, name: String, ty: Type) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, ty);
        }
    }

    fn lookup(&self, name: &str) -> Option<&Type> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty);
            }
        }
        None
    }
}

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TypeError {
    pub message: String,
    pub span: Span,
    pub kind: TypeErrorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeErrorKind {
    TypeMismatch,
    UndefinedField,
    WrongNumberOfArgs,
    MissingField,
    ExtraField,
    NonExhaustiveMatch,
    NotCallable,
    NotAStruct,
    InvalidBinaryOp,
    InvalidUnaryOp,
    InvalidCast,
    ConditionNotBool,
    BranchTypeMismatch,
    ReturnTypeMismatch,
    InvalidTupleIndex,
    LabelMismatch,
    NonContiguousLabels,
    InvalidPipePlaceholder,
    /// Call-site `mut` marker required but not written on a fresh binding
    /// passed to a `mut ref T` / `mut Slice[T]` parameter (design.md Part 1½).
    MissingMutMarker,
    /// Call-site `mut` marker written where it is not legal: either the
    /// parameter is not mutating, or the argument is already a mut-ref
    /// (e.g., forwarded binding, nested mut-ref return).
    InvalidMutMarker,
    /// 128-bit integer literal used (`123i128`, `0u128`). 128-bit integer
    /// types are not yet supported by the type system or codegen.
    UnsupportedNumericSuffix,
    /// A non-`pub` type appears in a `pub` signature position
    /// (function parameter/return, public struct field, public enum variant
    /// payload, public type alias, public constant). See design.md
    /// § Struct Field Visibility and § Three-level visibility. (CR-18.)
    PrivateTypeInPublicSignature,
    /// A refutable pattern (one that may not match all values) appears where
    /// only irrefutable patterns are allowed — function parameters, closure
    /// parameters, `let` bindings. Use `if let` or `match` for refutable cases.
    RefutablePattern,
    /// `impl Foo for T` is missing a required `impl Bar for T` where `Bar` is
    /// a supertrait of `Foo`. See design.md § Trait Constraints (Supertraits).
    MissingSupertrait,
    /// A type argument does not satisfy the required trait bound (e.g. T in
    /// `SortedSet[T]` must implement `Ord`; K in `Map[K, V]` must implement
    /// `Hash + Eq`).
    TraitBoundNotSatisfied,
    /// `T.method(...)` where T is a generic type parameter and two or more of
    /// its bound traits declare an associated function with that name. The
    /// programmer must use UFCS `Trait.method(...)` to disambiguate.
    AmbiguousAssocFn,
    /// `e.method(args)` where two or more user-impl candidates of the same
    /// priority tier survive method resolution on the receiver's type
    /// (typically two trait impls when no inherent matches; the
    /// inherent-beats-trait priority filter eliminates inherent-vs-trait
    /// ambiguity). The programmer must use UFCS `Trait.method(receiver, ...)`
    /// to disambiguate. Distinct from `AmbiguousAssocFn`, which targets the
    /// type-prefixed `T.method(...)` form on a generic type parameter.
    /// Slice 3 of the method-resolution CR — see
    /// `phase-4-interpreter.md` § "TypeChecker: implement full method
    /// resolution algorithm" item 4.
    AmbiguousMethod,
    /// Bare `method(args)` call appears in a synthesis position (no expected
    /// type) where the only candidate resolutions are trait associated
    /// functions. The typechecker cannot infer the target type — programmer
    /// must add a type annotation or use type-prefixed `T.method(...)`.
    CannotInferAssocFn,
    /// A once-callable closure (`OnceFn(...)` value, or a closure literal
    /// whose body consumes a captured owned non-Copy binding) is being
    /// assigned to a slot whose type is `Fn(...)` or `ref Fn(...)`. The slot
    /// promises repeatable invocation; the closure can only be called once.
    /// Round 12.45 (Step 3) — caller-side rejection of `OnceFn` at `Fn` /
    /// `ref Fn` parameter slots and any other Fn-shaped assignment boundary.
    OnceFnIntoFnSlot,
    /// `e.m(args)` where no candidate at any receiver level resolves to a
    /// method named `m`. Carries an optional `did you mean 'm2'?` tail when
    /// an edit-distance-≤2 candidate exists on the receiver type's impls.
    /// Method-resolution Step 7 — see phase-4-interpreter.md § TypeChecker:
    /// implement full method resolution algorithm.
    NoMethodFound,
    /// A match arm pattern is fully covered by an earlier (unguarded) arm,
    /// so its body can never execute. Emitted as a warning, not an error —
    /// codegen retains the arm. Reachability slice of the Maranget
    /// exhaustiveness upgrade (step 6).
    UnreachableArm,
    /// A generic call's return type contains a `TypeParam(T)` that no
    /// argument or expected-type context pinned. Today the permissive
    /// `TypeParam` arm of `types_compatible` lets these silently flow
    /// through; this diagnostic surfaces them at the consuming context
    /// (currently: synthesis-mode `let` bindings without an annotation).
    /// Item 131 sub-step 2a.
    CannotInferTypeParam,
    /// Two impls would coexist on the same `(trait_name, target_type)`
    /// where one is generic-on-name (`impl Foo for Bar[T]`) and the other
    /// is specialized to a concrete instantiation (`impl Foo for
    /// Bar[i32]`), or both are specialized to the same concrete
    /// instantiation. v1 rejects the overlap at impl registration time
    /// rather than picking a winner at the call site (Rust-style
    /// specialization is post-v1). Theme-4 slice — see
    /// `phase-4-interpreter.md` § `impl Option[Ordering]` deferred entry.
    ConflictingImpl,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

// ── Result ──────────────────────────────────────────────────────

pub struct TypeCheckResult {
    pub errors: Vec<TypeError>,
    /// Non-fatal diagnostics: typecheck-time signals that don't block
    /// later phases. Currently carries `UnreachableArm` from the Maranget
    /// reachability pass; future signals belong here too.
    pub warnings: Vec<TypeError>,
    pub expr_types: HashMap<SpanKey, Type>,
    pub struct_info: HashMap<String, StructInfo>,
    pub enum_info: HashMap<String, EnumInfo>,
    /// Derived traits for each `distinct type` declaration.
    pub distinct_type_traits: HashMap<String, HashSet<String>>,
    /// For each `?` expression that requires cross-error-type conversion via
    /// `From`, the target error type's name. Interpreter / codegen consult
    /// this side-table to know whether to call `<target>.from(err)` on the
    /// propagated Err value.
    pub question_conversions: HashMap<SpanKey, String>,
    /// `(trait_name, target_type_name)` pairs for every impl registered at
    /// typecheck time. The lowering pass consults this to decide whether a
    /// non-primitive operand has an applicable trait impl (e.g. user
    /// `impl Eq for MyStruct` drives `==` dispatch).
    pub trait_impls: std::collections::HashSet<(String, String)>,
    /// For each `x.into()` call resolved against an expected type, the target
    /// type's name. Lowering rewrites these to `Target.from(x)` — the `Into`
    /// blanket impl is not materialized in `env.impls`, it's purely a lowering
    /// rewrite backed by the `From` dispatch already in place.
    pub into_conversions: HashMap<SpanKey, String>,
    /// For each `x.try_into()` call resolved against an expected `Result[T, E]`,
    /// the target type's name (the `T` inside the Result). Lowering rewrites
    /// these to `Target.try_from(x)` — same desugar architecture as `into`.
    pub try_into_conversions: HashMap<SpanKey, String>,
    /// Enum names that derive `Display(snake_case)`. The interpreter uses
    /// this to convert variant names to `lower_snake_case` in `to_string()`.
    pub display_snake_case_enums: HashSet<String>,
    /// For each `MethodCall` expression, the canonical `Type.method` callee
    /// key — the same shape used in `EffectCheckResult.{inferred,declared}_effects`
    /// and in `Program.callee_effectful`. Lowering forwards this to
    /// `Program.method_callee_types` so codegen can narrow the par-branch
    /// cooperative-cancel check at instance method sites.
    ///
    /// Populated alongside the receiver-type dispatch in `infer_method_call`.
    /// Keyed by call-expression `SpanKey` (note: the parser sets
    /// `MethodCall.span == receiver.span`, so callers must not reuse
    /// `expr_types` for this purpose — a separate map avoids the
    /// return-type-overwrites-receiver-type race).
    pub method_callee_types: HashMap<SpanKey, String>,
    /// Bare-call dispatch resolutions: span of a `Call(Identifier(name))` →
    /// resolved target type name (e.g. `"Wrapper"`). Populated when expected-
    /// type inference resolves a bare associated-function call to a concrete
    /// type. Lowering rewrites the call to `Target.name(args)` so the
    /// interpreter / codegen dispatches via the existing impl table without
    /// further special-casing.
    pub bare_assoc_fn_targets: HashMap<SpanKey, String>,
    /// Per-call-site generic-param substitutions: call-expression span → name
    /// → resolved type name. Concrete entries (`"Wrapper"`) come from the
    /// typechecker's solver; abstract entries (`"T"`) propagate the caller's
    /// generic binding and are resolved against the runtime substitution
    /// stack at execution time. Consumed by the interpreter to dispatch
    /// `T.method()` calls inside generic function bodies.
    pub call_type_subs: HashMap<SpanKey, HashMap<String, String>>,
    /// For each pattern-binding name introduced by `bind_pattern_types`, the
    /// canonical type name (e.g. `"MyError"`). Keyed by the pattern's span.
    /// Used by codegen to reconstitute struct payloads from the i64 word
    /// when binding match-arm variables: `Err(e)` where the variant payload
    /// is a struct, `e` is bound as i64 by the enum-payload codegen, and
    /// codegen uses this table to know the surface type of `e` so
    /// `e.field` field access can dispatch through the right struct shape.
    /// Only `Type::Named` types are recorded (primitives, refs, etc. don't
    /// need the reconstruction step).
    pub pattern_binding_types: HashMap<SpanKey, String>,
    /// Sibling table to `pattern_binding_types` carrying the inner element
    /// `TypeExpr` for `Vec[T]` / `Slice[T]` pattern bindings only. Keyed by
    /// the same `SpanKey` (the pattern's span). Populated alongside the
    /// String-name entry in `bind_pattern_types` / `check_pattern_against`
    /// when the surface type is `Vec[T]` or `Slice[T]`. Consumed by codegen
    /// at `bind_pattern_values` to populate `vec_elem_types` /
    /// `slice_elem_types` keyed by the binding's variable name, so direct
    /// method dispatch on a pattern-bound `Vec` / `Slice` payload (`xs.len()`,
    /// `xs[0]`, `xs.push(...)`) routes through the right element-typed path
    /// without going through function-arg routing as a work-around. Empty
    /// for non-collection bindings (the existing String-name table is
    /// sufficient for those). PB sibling slice (2026-05-09).
    pub pattern_binding_inner_types: HashMap<SpanKey, TypeExpr>,
    /// Per-leaf-binding borrow mode under a `ref` / `mut ref` scrutinee.
    /// Keyed by the leaf binding pattern's span (or, for struct shorthand
    /// fields without a sub-pattern, the field's span). Owned bindings are
    /// absent. Forwarded to `Program.pattern_binding_borrow_modes` by the
    /// lowering pass and consumed by codegen at `bind_pattern_values` to
    /// emit the ref-binding shim — see `ast::PatternBindingBorrow`.
    pub pattern_binding_borrow_modes: HashMap<SpanKey, crate::ast::PatternBindingBorrow>,
    /// Names of functions declared with `#[compiler_builtin]` (CR-202
    /// slice 2). The signature lives in `env.functions`; the entry here
    /// flags the function as having its body replaced by Rust dispatch.
    /// Empty in user-only programs (slice 1's resolver gate `E0237`
    /// prevents the attribute outside stdlib source).
    pub compiler_builtins: HashSet<String>,
}

// ── Cross-module visibility helpers (CR-24 slice 6) ─────────────

/// Return the declared `Visibility` of a top-level item named `name` inside
/// `module`. Returns `None` when the item does not exist or is not a kind
/// that carries top-level visibility (impl blocks, layouts, etc).
fn find_item_visibility(module: &crate::module::Module, name: &str) -> Option<Visibility> {
    for item in &module.items {
        match item {
            Item::Function(f) if f.name == name => return Some(f.visibility()),
            Item::StructDef(s) if s.name == name => return Some(s.visibility()),
            Item::EnumDef(e) if e.name == name => return Some(e.visibility()),
            Item::TraitDef(t) if t.name == name => return Some(t.visibility()),
            Item::ConstDecl(c) if c.name == name => return Some(c.visibility()),
            Item::TypeAlias(t) if t.name == name => return Some(t.visibility()),
            Item::DistinctType(d) if d.name == name => return Some(d.visibility()),
            Item::ExternFunction(e) if e.name == name => return Some(e.visibility()),
            Item::ExternBlock(b) => {
                for it in &b.items {
                    match it {
                        ExternItem::Function(f) if f.name == name => {
                            return Some(f.visibility());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the `StructDef` for a top-level struct named `name` in `module`, if
/// any. Used by `infer_field_access` to enforce cross-module field visibility.
fn find_struct_def<'m>(module: &'m crate::module::Module, name: &str) -> Option<&'m StructDef> {
    for item in &module.items {
        if let Item::StructDef(s) = item {
            if s.name == name {
                return Some(s);
            }
        }
    }
    None
}

// ── Type Checker ────────────────────────────────────────────────

pub struct TypeChecker<'a> {
    program: &'a Program,
    resolve_result: &'a ResolveResult,
    /// Optional project-wide tree for cross-module checks (CR-24 slice 6b):
    /// extends `E0221 PrivateTypeInPublicSignature` to imported types and
    /// turns on field-access rejection for cross-module struct fields.
    tree: Option<&'a crate::module::ProgramTree>,
    /// The id of the module being typechecked, when `tree` is set. Used to
    /// scope cross-module visibility checks — an access is "cross-module"
    /// when the accessed item's origin differs from `current_module`.
    current_module: Option<crate::module::ModuleId>,
    /// Local name → (canonical origin module path, canonical item name,
    /// declared visibility) for items imported into the current module from
    /// elsewhere in the tree. Slice 7: re-exports collapse to the canonical
    /// entry — `import M.X` where M re-exports `a.b.X` records
    /// `("X" → (["a","b"], "X", ...))`, and an alias `import M.Y as Z` maps
    /// `"Z" → (["a","b"], "Y", ...)`. Populated during `build_type_env` when
    /// `tree` is set.
    type_origins: HashMap<String, (Vec<String>, String, Visibility)>,
    env: TypeEnv,
    local_scope: LocalTypeScope,
    errors: Vec<TypeError>,
    warnings: Vec<TypeError>,
    expr_types: HashMap<SpanKey, Type>,
    current_return_type: Option<Type>,
    /// LB3 — per-label collector stack for labeled-block break-with-value
    /// LUB inference. Pushed at labeled-block entry; each `Break { label:
    /// Some(name), value: Some(e) }` site appends `infer_expr(e)` to the
    /// matching frame; bare `break label` (no value) appends `Type::Unit`.
    /// Popped at labeled-block exit; the labeled block's type is the LUB
    /// of `tail_type` and the collected break types. Saved/restored at
    /// closure boundaries (LB4) so labels are lexical to the function-
    /// body control flow. Loops keep their existing `Type::Never`-by-
    /// default behavior — loop-LUB inference is a separate slice that
    /// will reuse the same machinery once the design entry promotes
    /// (out-of-scope here).
    break_value_types: Vec<(String, Vec<Type>)>,
    current_self_type: Option<Type>,
    /// True when type-checking inside a defer/errdefer block.
    in_defer: bool,
    /// `?` cross-error From conversions (span → target error type name).
    question_conversions: HashMap<SpanKey, String>,
    /// `x.into()` conversions (span of the MethodCall → target type name).
    into_conversions: HashMap<SpanKey, String>,
    /// `x.try_into()` conversions (span of the MethodCall → target type name,
    /// where target is the `T` extracted from `Result[T, E]`).
    try_into_conversions: HashMap<SpanKey, String>,
    /// Enum names that derive `Display(snake_case)`. Populated during
    /// `env_add_enum`; transferred to `TypeCheckResult`.
    display_snake_case_enums: HashSet<String>,
    /// MethodCall span → `Type.method` canonical callee key. See the
    /// matching field on `TypeCheckResult` for the full rationale.
    method_callee_types: HashMap<SpanKey, String>,
    /// Bare-call expected-type dispatch resolutions: call-expression span →
    /// resolved target type name (e.g. `"Wrapper"`). Populated when
    /// `try_apply_expected_assoc_fn_inference` resolves a bare `name(args)`
    /// call against a concrete expected type. The lowering pass rewrites
    /// these to `Target.name(args)` so the interpreter / codegen can dispatch
    /// through the existing `Type.method` impl table.
    bare_assoc_fn_targets: HashMap<SpanKey, String>,
    /// Per-call-site type substitutions: call-expression span → name → resolved
    /// type name (concrete struct/enum, or another generic param if the caller
    /// is itself generic and propagates the binding). Populated by `infer_call`
    /// after solving and by `check_expr`'s expected-type-driven pass for
    /// zero-arg generic calls. Consumed by the interpreter at each call: it
    /// pushes the resolved frame so `T.method()` and bare-method calls inside
    /// the callee's body can look up `T`'s concrete binding.
    call_type_subs: HashMap<SpanKey, HashMap<String, String>>,
    /// Pattern-binding name → canonical type name. See the public copy on
    /// `TypeCheckResult` for the consumer doc.
    pattern_binding_types: HashMap<SpanKey, String>,
    /// Pattern-binding span → inner element `TypeExpr` for `Vec[T]` / `Slice[T]`
    /// bindings. Sibling to `pattern_binding_types`. See the public copy on
    /// `TypeCheckResult` for the full rationale (PB sibling slice 2026-05-09).
    pattern_binding_inner_types: HashMap<SpanKey, TypeExpr>,
    /// Internal mirror of the public table; written by `check_pattern_against`
    /// at every leaf-binding site (and at struct shorthand fields) when
    /// the scrutinee mode is non-Owned. Surfaced in `check()`.
    pattern_binding_borrow_modes: HashMap<SpanKey, crate::ast::PatternBindingBorrow>,
    /// Parallel to `pattern_binding_inner_types`, storing the raw `Type`
    /// (which may contain unresolved `Type::TypeVar`) captured at the
    /// recording site. After body inference completes, `finalize_pattern_
    /// binding_inner_types` walks this map, resolves typevars against
    /// `env.substitutions`, and overwrites `pattern_binding_inner_types`
    /// with the substituted `TypeExpr`. Without this, `let mut q =
    /// VecDeque.new(); q.push_back(x);` writes the inner-type entry at
    /// the let site (where `?T0` is still unsolved), and the resulting
    /// `TypeKind::Error` strands codegen with the wrong element type.
    pattern_binding_inner_unresolved: HashMap<SpanKey, Type>,
    /// Trait bounds for the generic parameters in the current enclosing scope
    /// (impl-level + function/method-level). Indexed by the param's textual
    /// name so it pairs naturally with `Type::TypeParam(name)`. Populated on
    /// entering a generic-bearing scope and saved/restored on exit, mirroring
    /// the enclosing-generic-name list threaded through the lower / check
    /// path. Used to resolve bare `method(args)` calls at expected-type
    /// positions when the expected type is a generic param.
    enclosing_bounds: HashMap<String, Vec<crate::ast::TraitBound>>,
    /// Name of the enclosing trait declaration when type-checking a default
    /// method body. Populated on entering `check_trait_def`, cleared on exit.
    /// Consumed by `dispatch_self_receiver_method` (slice 3.5 of the
    /// method-resolution CR — see `phase-4-interpreter.md` item 8): when a
    /// receiver-form `self.method()` call appears in a default body, the
    /// candidate methods are the enclosing trait's own methods plus every
    /// method on traits in its supertrait closure. Outside trait bodies this
    /// is `None` and `Self` falls through to the silent pre-existing path
    /// (impl-method bodies bind `Self` to the impl's target type via
    /// `current_self_type`, a different mechanism).
    enclosing_trait: Option<String>,
    /// Closure expression span → reason that closure became once-callable.
    /// Populated by `closure_type_with_capture_inference` when the body walk
    /// finds a captured-non-Copy consume; consumed by `check_assignable` so
    /// `E_ONCE_FN_INTO_FN_SLOT` can name the consumed binding when a closure
    /// literal is rejected at a `Fn` slot. Round 12.45 (Step 3).
    closure_once_reasons: HashMap<SpanKey, OnceReason>,
}

/// Why a closure is `OnceFunction`-typed: which captured outer binding the
/// body consumed, and where in the body the consume happened. Populated by
/// the once-callability walker when it flips its first identifier-leaf in
/// `Consuming` mode that resolves to an outer non-Copy binding.
#[derive(Debug, Clone)]
struct OnceReason {
    /// The outer binding name (or `"self"`) that the closure body consumed.
    consumed_binding: String,
    /// The body span where the consume occurred (the identifier-leaf, not
    /// the enclosing call). Used for diagnostics; not currently surfaced in
    /// the rejection message but kept for future polish in Step 5.
    #[allow(dead_code)]
    consumed_span: Span,
}

impl<'a> TypeChecker<'a> {
    pub fn new(program: &'a Program, resolve_result: &'a ResolveResult) -> Self {
        TypeChecker {
            program,
            resolve_result,
            tree: None,
            current_module: None,
            type_origins: HashMap::new(),
            env: TypeEnv::new(),
            local_scope: LocalTypeScope::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            expr_types: HashMap::new(),
            current_return_type: None,
            break_value_types: Vec::new(),
            current_self_type: None,
            in_defer: false,
            question_conversions: HashMap::new(),
            into_conversions: HashMap::new(),
            try_into_conversions: HashMap::new(),
            display_snake_case_enums: HashSet::new(),
            method_callee_types: HashMap::new(),
            bare_assoc_fn_targets: HashMap::new(),
            call_type_subs: HashMap::new(),
            pattern_binding_types: HashMap::new(),
            pattern_binding_inner_types: HashMap::new(),
            pattern_binding_borrow_modes: HashMap::new(),
            pattern_binding_inner_unresolved: HashMap::new(),
            enclosing_bounds: HashMap::new(),
            enclosing_trait: None,
            closure_once_reasons: HashMap::new(),
        }
    }

    /// Attach a project-wide `ProgramTree` so cross-module visibility checks
    /// (CR-24 slice 6) can consult origin modules. Without the tree, the
    /// typechecker runs in single-file mode exactly as before.
    pub fn with_tree(
        mut self,
        tree: &'a crate::module::ProgramTree,
        module_id: crate::module::ModuleId,
    ) -> Self {
        self.tree = Some(tree);
        self.current_module = Some(module_id);
        self
    }

    pub fn check(mut self) -> TypeCheckResult {
        self.build_type_env();
        self.validate_derive_copy();
        self.validate_copy_implies_clone();
        self.validate_derived_traits_recursive();
        self.validate_enum_payload_no_nested_enum();
        self.validate_derive_arithmetic();
        self.check_signature_visibility();
        self.check_items();
        self.finalize_pattern_binding_inner_types();
        let trait_impls: std::collections::HashSet<(String, String)> = self
            .env
            .impls
            .iter()
            .filter_map(|imp| imp.trait_name.clone().map(|t| (t, imp.target_type.clone())))
            .collect();
        let distinct_type_traits = self.env.distinct_types.clone();
        let compiler_builtins = self.env.compiler_builtins.clone();
        TypeCheckResult {
            errors: self.errors,
            warnings: self.warnings,
            expr_types: self.expr_types,
            struct_info: self.env.structs,
            enum_info: self.env.enums,
            distinct_type_traits,
            question_conversions: self.question_conversions,
            trait_impls,
            into_conversions: self.into_conversions,
            try_into_conversions: self.try_into_conversions,
            display_snake_case_enums: self.display_snake_case_enums,
            method_callee_types: self.method_callee_types,
            bare_assoc_fn_targets: self.bare_assoc_fn_targets,
            call_type_subs: self.call_type_subs,
            pattern_binding_types: self.pattern_binding_types,
            pattern_binding_inner_types: self.pattern_binding_inner_types,
            pattern_binding_borrow_modes: self.pattern_binding_borrow_modes,
            compiler_builtins,
        }
    }

    fn type_error(&mut self, message: String, span: Span, kind: TypeErrorKind) {
        self.errors.push(TypeError {
            message,
            span,
            kind,
        });
    }

    /// Render a `ConstEvalError` from the const-expression evaluator
    /// (slice 2) as a focused `type_error` diagnostic. Reuses
    /// `TypeErrorKind::TypeMismatch` as the kind for surface-level
    /// const-eval errors — a dedicated kind isn't introduced at slice 2;
    /// future work may split if downstream consumers (`karac explain`)
    /// need to distinguish const-eval failures from other type errors.
    pub(crate) fn emit_const_eval_error(&mut self, err: ConstEvalError) {
        use ConstEvalError::*;
        let (msg, span) = match err {
            NonConstShape(s) => (
                "expression is not a valid const expression \
                 (no function calls, closures, method calls, or runtime-only shapes)"
                    .to_string(),
                s,
            ),
            Overflow { op, lhs, rhs, span } => (
                format!(
                    "const expression overflow: {} {} {} overflows {}",
                    format_const_value(&lhs),
                    binop_glyph(&op),
                    format_const_value(&rhs),
                    type_display(&const_value_type(&lhs))
                ),
                span,
            ),
            UnaryOverflow { op, operand, span } => (
                format!(
                    "const expression overflow: {}{} overflows {}",
                    unaryop_glyph(&op),
                    format_const_value(&operand),
                    type_display(&const_value_type(&operand))
                ),
                span,
            ),
            DivByZero { span } => ("const expression: division by zero".to_string(), span),
            OutOfRange {
                value,
                target_ty,
                span,
            } => (
                format!(
                    "const expression: literal {} does not fit in {}",
                    value,
                    type_display(&target_ty)
                ),
                span,
            ),
            TypeMismatch {
                expected,
                found,
                span,
            } => (
                format!(
                    "const expression type mismatch: expected {}, found {}",
                    type_display(&expected),
                    type_display(&found)
                ),
                span,
            ),
            UndefinedConst { name, span } => (
                format!("const expression: '{}' is not a known const", name),
                span,
            ),
            ArithOnNonInt { ty, op, span } => (
                format!(
                    "arithmetic operator '{}' is not supported on {} \
                     (only integer types)",
                    binop_glyph(&op),
                    type_display(&ty)
                ),
                span,
            ),
            LogicalOnNonBool { ty, op, span } => (
                format!(
                    "logical operator '{}' is not supported on {} (only bool)",
                    binop_glyph(&op),
                    type_display(&ty)
                ),
                span,
            ),
            CompareIncomparable {
                lhs_ty,
                rhs_ty,
                span,
            } => (
                format!(
                    "cannot compare {} with {} in const expression \
                     — both sides must have the same type",
                    type_display(&lhs_ty),
                    type_display(&rhs_ty)
                ),
                span,
            ),
            CyclicConstDef { chain, span } => (
                format!(
                    "const expression: cyclic const definition ({})",
                    chain.join(" -> ")
                ),
                span,
            ),
        };
        self.type_error(msg, span, TypeErrorKind::TypeMismatch);
    }

    fn type_warning(&mut self, message: String, span: Span, kind: TypeErrorKind) {
        self.warnings.push(TypeError {
            message,
            span,
            kind,
        });
    }

    /// Validate an `as` cast (`from as to`) and emit a focused diagnostic
    /// when the pair is rejected. Per design.md § Numeric Semantics > as-
    /// cast semantics (v60 item 49):
    ///
    /// Accepted: numeric → numeric (saturating float→int, sign-/zero-
    /// extending int→int, IEEE 754 int→float, fptrunc / fpext for
    /// float→float); `bool → iN/uN` (zero-extends from i1); `char → uN`
    /// for `N >= 32` and `char → iN` for `N >= 32` (Unicode scalar value
    /// fits in 21 bits).
    ///
    /// Rejected with focused diagnostics:
    /// - `char → iN/uN` with `N < 32` → `E_CHAR_AS_NARROW_INT`.
    /// - `iN/uN → char` → `E_INT_AS_CHAR`.
    /// - `iN/uN → bool` → `E_INT_AS_BOOL`.
    /// - `f32/f64 → bool` → `E_FLOAT_AS_BOOL`.
    ///
    /// All other unsupported pairs fall through to the generic
    /// `cannot cast` diagnostic.
    fn check_cast_pair(&mut self, from_ty: &Type, to_ty: &Type, span: &Span) {
        // Type::Error is a wildcard — silently accept; the original error
        // already surfaced elsewhere.
        if matches!(from_ty, Type::Error) || matches!(to_ty, Type::Error) {
            return;
        }

        // Numeric → numeric: always accepted (existing rule).
        if is_numeric(from_ty) && is_numeric(to_ty) {
            return;
        }

        // Bool → integer: produces 0/1.
        if matches!(from_ty, Type::Bool) && is_integer(to_ty) {
            return;
        }

        // Char → wide integer (>= 32 bits): Unicode scalar value fits.
        if matches!(from_ty, Type::Char) {
            if let Some(width) = integer_width_bits(to_ty) {
                if width >= 32 {
                    return;
                }
                // Char → narrow integer: rejected with focused diagnostic.
                self.type_error(
                    format!(
                        "error[E_CHAR_AS_NARROW_INT]: cannot cast `char` to \
                         `{}` directly because the Unicode scalar range \
                         (`0..=0x10FFFF`) does not fit in {width} bits; \
                         help: `c as u32 as {}` for explicit truncation, or \
                         `c.encode_utf8(buf)` for proper UTF-8 encoding",
                        type_display(to_ty),
                        type_display(to_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::InvalidCast,
                );
                return;
            }
        }

        // Integer → char: rejected (use char.try_from for fallible
        // construction).
        if is_integer(from_ty) && matches!(to_ty, Type::Char) {
            self.type_error(
                format!(
                    "error[E_INT_AS_CHAR]: cannot cast `{}` to `char` \
                     directly because not every integer is a valid \
                     Unicode scalar (surrogate range, values above \
                     `0x10FFFF`); help: `char.try_from(n)` returns \
                     `Result[char, _]`",
                    type_display(from_ty)
                ),
                span.clone(),
                TypeErrorKind::InvalidCast,
            );
            return;
        }

        // Integer → bool: rejected (use `n != 0`).
        if is_integer(from_ty) && matches!(to_ty, Type::Bool) {
            self.type_error(
                format!(
                    "error[E_INT_AS_BOOL]: cannot cast `{}` to `bool`; \
                     help: write `n != 0` for the explicit non-zero \
                     check",
                    type_display(from_ty)
                ),
                span.clone(),
                TypeErrorKind::InvalidCast,
            );
            return;
        }

        // Float → bool: rejected (the operation is meaningless).
        if matches!(from_ty, Type::Float(_)) && matches!(to_ty, Type::Bool) {
            self.type_error(
                format!(
                    "error[E_FLOAT_AS_BOOL]: cannot cast `{}` to `bool`; \
                     a float-to-bool conversion is not well-defined \
                     (NaN? denormal? -0?); decide on a predicate \
                     explicitly (e.g., `f != 0.0`) before casting",
                    type_display(from_ty)
                ),
                span.clone(),
                TypeErrorKind::InvalidCast,
            );
            return;
        }

        // Anything else falls through to the generic diagnostic.
        self.type_error(
            format!(
                "cannot cast '{}' to '{}'",
                type_display(from_ty),
                type_display(to_ty)
            ),
            span.clone(),
            TypeErrorKind::InvalidCast,
        );
    }

    /// Emit `error[E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION]` for an empty
    /// `Vec[]` / `Array[]` / `Set[]` / `Map[]` literal that reached
    /// synthesis mode without an enclosing annotation. The diagnostic body
    /// names the literal kind, supplies a per-kind annotation skeleton, and
    /// suggests the corresponding constructor (`Vec.new()` / `Set.new()` /
    /// `Map.new()`) per design.md § Collection Literals.
    fn report_empty_prefix_literal(&mut self, type_name: &str, span: &Span) {
        let (annotation_skeleton, constructor) = match type_name {
            "Vec" => ("Vec[T]", Some("Vec.new()")),
            "Array" => ("Array[T, 0]", None),
            "Set" => ("Set[T]", Some("Set.new()")),
            "Map" => ("Map[K, V]", Some("Map.new()")),
            _ => (type_name, None),
        };
        let mut msg = format!(
            "error[E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION]: cannot infer \
             element type from empty `{type_name}[]` literal — \
             add a binding annotation: `let v: {annotation_skeleton} = {type_name}[]`"
        );
        if let Some(ctor) = constructor {
            msg.push_str(&format!(", or use `{ctor}`"));
        }
        self.type_error(msg, span.clone(), TypeErrorKind::TypeMismatch);
    }

    /// Emit `NoMethodFound` for an unknown stdlib method only when a close
    /// candidate exists in `known_methods` (edit distance ≤ 2 via
    /// `edit_distance::suggest_similar`). Used by per-type `infer_*_method`
    /// arms to surface typos without breaking the silent fallback for
    /// runtime-only methods that the typechecker has not yet enumerated.
    /// Each arm's `KNOWN_METHODS` constant is the typechecker's current
    /// enumeration of that type's surface — it grows as stdlib enumeration
    /// catches up to the interpreter, at which point the arm's `_` case
    /// can flip from "typo-only" to "always-error". See
    /// phase-4-interpreter.md § Method Resolution Step 7.
    fn maybe_emit_method_typo(
        &mut self,
        type_name: &str,
        method: &str,
        known_methods: &[&str],
        span: &Span,
    ) {
        if let Some(suggestion) = crate::edit_distance::suggest_similar(method, known_methods) {
            self.type_error(
                format!(
                    "no method '{}' on type '{}', did you mean '{}'?",
                    method, type_name, suggestion
                ),
                span.clone(),
                TypeErrorKind::NoMethodFound,
            );
        }
    }

    /// Default `_` arm body for per-type `infer_*_method` dispatch: emit a
    /// typo-suggestion diagnostic when the typed name is close to a known
    /// method, type-check the arguments, and return `Type::Error`. The
    /// silent fallback for far-from-anything names preserves the historical
    /// permissive behavior for runtime-only methods that the typechecker
    /// has not yet enumerated.
    ///
    /// Reserved for arms whose typechecker enumeration has *not yet* reached
    /// parity with the interpreter (currently the four phase-11 arms — Regex
    /// and the three HTTP types). Phase-8-floor arms have flipped to
    /// `require_known_method` so unknown methods on those types fail loudly.
    fn handle_unknown_method(
        &mut self,
        type_name: &str,
        method: &str,
        known_methods: &[&str],
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        self.maybe_emit_method_typo(type_name, method, known_methods, span);
        for arg in args {
            self.infer_expr(&arg.value);
        }
        Type::Error
    }

    /// Default `_` arm body for per-type `infer_*_method` dispatch on arms
    /// whose typechecker enumeration has reached parity with the interpreter:
    /// **always** emit `NoMethodFound`, type-check the arguments, and return
    /// `Type::Error`. If the typed name is edit-distance ≤ 2 from a known
    /// method, the diagnostic includes a `did you mean ...?` suggestion;
    /// otherwise it reports the unknown name plainly. Either way the
    /// diagnostic fires — there is no silent fall-through.
    ///
    /// Used by phase-8-floor arms (String, Slice, Map, Entry, SortedSet,
    /// Set, Iterator, Sender, Receiver). Phase-11 arms keep using
    /// `handle_unknown_method` until their floor lands.
    /// See phase-4-interpreter.md § Method Resolution Step 7(d).
    fn require_known_method(
        &mut self,
        type_name: &str,
        method: &str,
        known_methods: &[&str],
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let msg = match crate::edit_distance::suggest_similar(method, known_methods) {
            Some(suggestion) => format!(
                "no method '{}' on type '{}', did you mean '{}'?",
                method, type_name, suggestion
            ),
            None => format!("no method '{}' on type '{}'", method, type_name),
        };
        self.type_error(msg, span.clone(), TypeErrorKind::NoMethodFound);
        for arg in args {
            self.infer_expr(&arg.value);
        }
        Type::Error
    }

    /// Map a lexer-provided integer suffix to the concrete `Type` it denotes.
    /// `None` defaults to `i64`. `I128` / `U128` route to
    /// `IntSize::I128` / `UIntSize::U128` (added 2026-05-11 alongside
    /// const generics slice 2b — `IntSize`/`UIntSize` carry the 128-bit
    /// variants now; downstream consumers should handle them through
    /// the standard arms).
    fn type_from_int_suffix(&mut self, sfx: Option<IntSuffix>, _span: Span) -> Type {
        match sfx {
            None => Type::Int(IntSize::I64),
            Some(IntSuffix::I8) => Type::Int(IntSize::I8),
            Some(IntSuffix::I16) => Type::Int(IntSize::I16),
            Some(IntSuffix::I32) => Type::Int(IntSize::I32),
            Some(IntSuffix::I64) => Type::Int(IntSize::I64),
            Some(IntSuffix::I128) => Type::Int(IntSize::I128),
            Some(IntSuffix::U8) => Type::UInt(UIntSize::U8),
            Some(IntSuffix::U16) => Type::UInt(UIntSize::U16),
            Some(IntSuffix::U32) => Type::UInt(UIntSize::U32),
            Some(IntSuffix::U64) => Type::UInt(UIntSize::U64),
            Some(IntSuffix::U128) => Type::UInt(UIntSize::U128),
        }
    }

    /// Map a lexer-provided float suffix to the concrete `Type` it denotes.
    /// `None` defaults to `f64`.
    fn type_from_float_suffix(sfx: Option<FloatSuffix>) -> Type {
        match sfx {
            None | Some(FloatSuffix::F64) => Type::Float(FloatSize::F64),
            Some(FloatSuffix::F32) => Type::Float(FloatSize::F32),
        }
    }

    /// Check whether a type supports `==` / `!=` (PartialEq).
    /// All primitives including floats support PartialEq.
    /// Named types (structs/enums) require `#[derive(Eq)]` or `#[derive(PartialEq)]`.
    fn type_supports_partial_eq(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Str
            | Type::Unit => true,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_partial_eq(e)),
            Type::Array { element, .. } => self.type_supports_partial_eq(element),
            Type::Slice { element, .. } => self.type_supports_partial_eq(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_partial_eq(inner),
            Type::Named { name, args } => {
                // A user-provided `impl Eq for Name` is sufficient — the
                // lowering pass dispatches `==`/`!=` through it. Falls back
                // to `#[derive(Eq)]`/`#[derive(PartialEq)]` when no impl is
                // registered (e.g. for compiler-provided structural eq on
                // built-in enums like `Option`/`Result`).
                if self.env.has_impl("Eq", name, args) {
                    return true;
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_partial_eq(inner),
            Type::Shared(name) => {
                if self.env.has_impl("Eq", name, &[]) {
                    return true;
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports full `Eq` (required for Map/Set keys, etc.).
    /// Floats (f32/f64) do NOT support Eq due to IEEE 754 NaN != NaN.
    /// Named types require `#[derive(Eq)]`.
    fn type_supports_eq(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_) | Type::UInt(_) | Type::Bool | Type::Char | Type::Str | Type::Unit => true,
            // f32/f64 follow IEEE 754: NaN != NaN, so they don't implement Eq
            Type::Float(_) => false,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_eq(e)),
            Type::Array { element, .. } => self.type_supports_eq(element),
            Type::Slice { element, .. } => self.type_supports_eq(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_eq(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq")
                } else {
                    // Unknown type — permissive to avoid cascading errors
                    // when the resolver has already flagged it.
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_eq(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports `Hash`. Floats do not — NaN-as-key would
    /// break the hash/eq contract. Named types require `#[derive(Hash)]`.
    fn type_supports_hash(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_) | Type::UInt(_) | Type::Bool | Type::Char | Type::Str | Type::Unit => true,
            Type::Float(_) => false,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_hash(e)),
            Type::Array { element, .. } => self.type_supports_hash(element),
            Type::Slice { element, .. } => self.type_supports_hash(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_hash(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Hash")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Hash")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_hash(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Hash")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Hash")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports total `Ord`. Floats do not (see Eq).
    fn type_supports_ord(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_) | Type::UInt(_) | Type::Bool | Type::Char | Type::Str | Type::Unit => true,
            Type::Float(_) => false,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_ord(e)),
            Type::Array { element, .. } => self.type_supports_ord(element),
            Type::Slice { element, .. } => self.type_supports_ord(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_ord(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_ord(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type implements `Display`.
    /// All primitives support Display. Built-in containers (Vec, Map, SortedSet,
    /// Option, Result) support Display when their type arguments do.
    /// Named user types require `#[derive(Display)]`.
    fn type_supports_display(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Str
            | Type::Unit => true,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_display(e)),
            Type::Array { element, .. } => self.type_supports_display(element),
            Type::Slice { element, .. } => self.type_supports_display(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_display(inner),
            Type::Named { name, args } => match name.as_str() {
                "Vec" | "Option" | "SortedSet" | "Set" if args.len() == 1 => {
                    self.type_supports_display(&args[0])
                }
                "Map" | "Result" if args.len() == 2 => {
                    self.type_supports_display(&args[0]) && self.type_supports_display(&args[1])
                }
                _ => {
                    if self.env.has_impl("Display", name, args) {
                        return true;
                    }
                    if let Some(info) = self.env.structs.get(name) {
                        info.derived_traits.contains("Display")
                    } else if let Some(info) = self.env.enums.get(name) {
                        info.derived_traits.contains("Display")
                    } else {
                        true
                    }
                }
            },
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_display(inner),
            Type::Shared(name) => {
                if self.env.has_impl("Display", name, &[]) {
                    return true;
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Display")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Display")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports `PartialOrd` (admits NaN for floats).
    fn type_supports_partial_ord(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Str
            | Type::Unit => true,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_partial_ord(e)),
            Type::Array { element, .. } => self.type_supports_partial_ord(element),
            Type::Slice { element, .. } => self.type_supports_partial_ord(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_partial_ord(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_partial_ord(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    fn check_assignable(&mut self, expected: &Type, found: &Type, span: Span) -> bool {
        if is_subtype(expected, found) {
            return true;
        }
        if Self::is_once_into_fn_shape(expected, found) {
            let mut msg = format!(
                "cannot pass once-callable closure where '{}' is expected; \
                 the slot promises repeatable invocation but the closure has type '{}'",
                type_display(expected),
                type_display(found),
            );
            let consumed = self
                .closure_once_reasons
                .get(&SpanKey::from_span(&span))
                .map(|r| r.consumed_binding.clone());
            if let Some(name) = &consumed {
                msg.push_str(&format!(
                    " (closure becomes once-callable because it consumes captured binding '{}')",
                    name
                ));
            }
            msg.push_str(
                "; help: clone the captured value before the closure body consumes it \
                 so the closure becomes repeatable, restructure the code to invoke the \
                 closure locally instead of routing it through this slot, or change the \
                 slot type to `OnceFn(...)` if you control its declaration",
            );
            self.type_error(msg, span, TypeErrorKind::OnceFnIntoFnSlot);
            return false;
        }
        self.type_error(
            format!(
                "expected '{}', found '{}'",
                type_display(expected),
                type_display(found)
            ),
            span,
            TypeErrorKind::TypeMismatch,
        );
        false
    }

    /// Returns `true` iff the assignment is a once-callable closure flowing
    /// into a `Fn`-shaped slot. Both `Fn(...)` and `ref Fn(...)` slots
    /// reject `OnceFn` arguments — the callee in either case may invoke
    /// the parameter many times, which violates the once-callable contract.
    /// Refs on either side are stripped before comparison so cross-wrapping
    /// (e.g., bare `OnceFn` arg into `ref Fn` slot) is also recognized as
    /// the once-callability violation rather than a generic ref-mismatch.
    /// Step 3 / round 12.45.
    fn is_once_into_fn_shape(expected: &Type, found: &Type) -> bool {
        fn unwrap(t: &Type) -> &Type {
            match t {
                Type::Ref(inner) | Type::MutRef(inner) => unwrap(inner),
                _ => t,
            }
        }
        matches!(
            (unwrap(expected), unwrap(found)),
            (Type::Function { .. }, Type::OnceFunction { .. })
        )
    }

    fn record_expr_type(&mut self, span: &Span, ty: &Type) {
        self.expr_types.insert(SpanKey::from_span(span), ty.clone());
    }

    // ── lower_type_expr ─────────────────────────────────────────

    fn lower_type_expr(&mut self, ty: &TypeExpr, generic_scope: &[String]) -> Type {
        // Top-level entry: by default the type is in a sized-by-value
        // position. Slice 1b's `E_OPAQUE_TYPE_REQUIRES_INDIRECTION` check
        // flips `parent_is_ref` to `true` only when descending through
        // `TypeKind::Ref` / `TypeKind::MutRef`, so opaque-foreign-type names
        // are accepted at `ref Foo` / `mut ref Foo` and rejected everywhere
        // else (fn params/return, struct fields, enum payloads, let
        // bindings, generic args, tuples, arrays, etc.).
        self.lower_type_expr_inner(ty, generic_scope, false)
    }

    fn lower_type_expr_inner(
        &mut self,
        ty: &TypeExpr,
        generic_scope: &[String],
        parent_is_ref: bool,
    ) -> Type {
        match &ty.kind {
            TypeKind::Path(path) => {
                let lowered = self.lower_path_type(path, generic_scope);
                // Slice 1b: opaque foreign types declared via `unsafe extern
                // "ABI" { type Foo; }` have no known size and cannot appear
                // by value. `parent_is_ref` is `true` only when the
                // immediate parent is `Ref` / `MutRef`, so `Vec[Foo]` (Foo
                // by-value inside Vec) and `ref Vec[Foo]` (Foo still
                // by-value inside Vec) both correctly emit; `ref Foo` and
                // `mut ref Foo` do not. The lowered type is returned
                // unchanged so downstream phases see the user's intent for
                // recovery purposes.
                if !parent_is_ref {
                    if let Type::Named { name, .. } = &lowered {
                        if self.env.opaque_foreign_types.contains(name) {
                            self.type_error(
                                format!(
                                    "error[E_OPAQUE_TYPE_REQUIRES_INDIRECTION]: opaque \
                                     foreign type '{name}' has no known size and cannot \
                                     appear by value here; wrap it in `ref {name}` / \
                                     `mut ref {name}` to use it through indirection"
                                ),
                                ty.span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                        }
                    }
                }
                lowered
            }
            TypeKind::Tuple(types) => Type::Tuple(
                types
                    .iter()
                    .map(|t| self.lower_type_expr_inner(t, generic_scope, false))
                    .collect(),
            ),
            TypeKind::Array { element, .. } => {
                Type::Array {
                    element: Box::new(self.lower_type_expr_inner(element, generic_scope, false)),
                    size: ConstArg::Literal(0), // const eval deferred
                }
            }
            TypeKind::Pointer { is_mut, inner } => Type::Pointer {
                is_mut: *is_mut,
                inner: Box::new(self.lower_type_expr_inner(inner, generic_scope, false)),
            },
            TypeKind::FnType {
                params,
                return_type,
                is_once,
                ..
            } => {
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|t| self.lower_type_expr_inner(t, generic_scope, false))
                    .collect();
                let ret = return_type
                    .as_ref()
                    .map(|t| self.lower_type_expr_inner(t, generic_scope, false))
                    .unwrap_or(Type::Unit);
                if *is_once {
                    Type::OnceFunction {
                        params: param_types,
                        return_type: Box::new(ret),
                    }
                } else {
                    Type::Function {
                        params: param_types,
                        return_type: Box::new(ret),
                    }
                }
            }
            TypeKind::Ref(inner) => Type::Ref(Box::new(self.lower_type_expr_inner(
                inner,
                generic_scope,
                true,
            ))),
            TypeKind::MutRef(inner) => Type::MutRef(Box::new(self.lower_type_expr_inner(
                inner,
                generic_scope,
                true,
            ))),
            TypeKind::MutSlice(element) => Type::Slice {
                element: Box::new(self.lower_type_expr_inner(element, generic_scope, false)),
                mutable: true,
            },
            TypeKind::Weak(inner) => Type::Weak(Box::new(self.lower_type_expr_inner(
                inner,
                generic_scope,
                false,
            ))),
            TypeKind::Unit => Type::Unit,
            TypeKind::Error => Type::Error,
        }
    }

    /// Const generics slice 3d: deferred-F regression-pin diagnostic.
    /// `Type::Named.args` is `Vec<Type>` — there's no representation
    /// for a const-arg on a user-defined struct / enum. When a
    /// `GenericArg::Const` payload arrives at a `Type::Named` lowering
    /// site, emit a focused "const-args on user-defined types are not
    /// yet supported" diagnostic and drop the const-arg. The
    /// surrounding type name (when known) flows into the message so
    /// users see which call site triggered the rejection. `Array[T, N]`
    /// const-args don't reach here — they're special-cased in
    /// `lower_array_type` before `lower_generic_args` is invoked.
    fn lower_generic_args_named(
        &mut self,
        generic_args: &Option<Vec<GenericArg>>,
        generic_scope: &[String],
        type_name: Option<&str>,
    ) -> Vec<Type> {
        generic_args
            .as_ref()
            .map(|ga| {
                ga.iter()
                    .filter_map(|arg| match arg {
                        GenericArg::Type(t) => Some(self.lower_type_expr(t, generic_scope)),
                        GenericArg::Const(expr) => {
                            let target = type_name.unwrap_or("this type");
                            self.type_error(
                                format!(
                                    "const generic argument on user-defined type '{}' is \
                                     not yet supported in this slice; only the built-in \
                                     `Array[T, N]` accepts const-args at v1",
                                    target
                                ),
                                expr.span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn lower_path_type(&mut self, path: &PathExpr, generic_scope: &[String]) -> Type {
        if path.segments.len() == 1 {
            let name = &path.segments[0];
            // Built-in `Array[T, N]` — fixed-size array with const-generic size
            if name == "Array" {
                if let Some(ty) = self.lower_array_type(&path.generic_args, generic_scope) {
                    return ty;
                }
            }
            // Built-in `Slice[T]` — borrowed view into contiguous memory
            if name == "Slice" {
                if let Some(ty) = self.lower_slice_type(&path.generic_args, generic_scope) {
                    return ty;
                }
            }
            // Check primitives
            if let Some(prim) = self.primitive_type(name) {
                return prim;
            }
            // Check generic scope
            if generic_scope.contains(name) {
                return Type::TypeParam(name.clone());
            }
            // Check type aliases — resolve transitively so that
            // `type AdminId = UserId; type UserId = i64;` sees `AdminId`
            // as `i64` regardless of source order.
            if self.env.type_aliases.contains_key(name) {
                let mut visited: HashSet<String> = HashSet::new();
                return self.resolve_alias_deep(name.clone(), &mut visited);
            }
            // Named type (struct/enum/import)
            let args = self.lower_generic_args_named(&path.generic_args, generic_scope, Some(name));
            // Intercept stdlib Rc[T] / Arc[T] wrappers — sub-item 2 of the
            // Type::Shared/Rc/Arc representation work. Single-arg form
            // only; zero/multi-arg keeps flowing through Type::Named so
            // the existing arity diagnostics still fire from there.
            if name == "Rc" && args.len() == 1 {
                return Type::Rc(Box::new(args.into_iter().next().unwrap()));
            }
            if name == "Arc" && args.len() == 1 {
                return Type::Arc(Box::new(args.into_iter().next().unwrap()));
            }
            // Intercept shared structs — bare struct name `S` lowers to
            // Type::Shared(S) when `S` was declared as `shared struct S`.
            // Non-shared structs continue through Type::Named.
            if let Some(info) = self.env.structs.get(name) {
                if info.is_shared {
                    return Type::Shared(name.clone());
                }
            }
            Type::Named {
                name: name.clone(),
                args,
            }
        } else {
            // Two-segment path where the first segment is a type parameter:
            // `I.Item` — an associated type projection. Exactly two segments
            // required; deeper paths (`A.B.C`) are module paths, not projections.
            if path.segments.len() == 2 && generic_scope.contains(&path.segments[0]) {
                return Type::AssocProjection {
                    param: path.segments[0].clone(),
                    assoc: path.segments[1].clone(),
                };
            }
            // Multi-segment module path — use last segment as type name
            let name = path.segments.last().unwrap().clone();
            let args =
                self.lower_generic_args_named(&path.generic_args, generic_scope, Some(&name));
            Type::Named { name, args }
        }
    }

    /// Lower `Array[T, N]` to `Type::Array { element, size }`.
    /// N must be a positive integer literal (const-eval of arithmetic expressions deferred).
    fn lower_array_type(
        &mut self,
        generic_args: &Option<Vec<GenericArg>>,
        generic_scope: &[String],
    ) -> Option<Type> {
        let args = generic_args.as_ref()?;
        if args.len() != 2 {
            return None;
        }
        let element_ty = match &args[0] {
            GenericArg::Type(t) => self.lower_type_expr(t, generic_scope),
            GenericArg::Const(_) => return None,
        };
        let size: ConstArg = match &args[1] {
            GenericArg::Const(expr) => match &expr.kind {
                ExprKind::Integer(n, _) if *n >= 0 => ConstArg::Literal(*n),
                // Const generics slice 3 (fork G4): an `Identifier` whose
                // name is a const-generic param in scope flows through
                // as `ConstArg::ConstParam(name)` — the inference solver
                // will substitute when the function is monomorphized.
                ExprKind::Identifier(name) if generic_scope.contains(name) => {
                    ConstArg::ConstParam(name.clone())
                }
                // Other shapes (non-literal, non-const-param) route
                // through slice 2's const-expression evaluator. A
                // successful evaluation to a non-negative integer
                // becomes a `Literal`; eval errors emit a focused
                // diagnostic and fall back to `Literal(0)` so downstream
                // consumers don't crash.
                _ => match self.eval_const_expr(expr, &Type::UInt(UIntSize::Usize)) {
                    Ok(cv) => match const_value_to_array_size(&cv) {
                        Some(n) => ConstArg::Literal(n as i64),
                        None => {
                            self.type_error(
                                format!(
                                    "Array size const-arg must evaluate to a \
                                     non-negative integer; got {}",
                                    type_display(&const_value_type(&cv))
                                ),
                                expr.span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            return None;
                        }
                    },
                    Err(e) => {
                        self.emit_const_eval_error(e);
                        return None;
                    }
                },
            },
            // Parser-side carveout (const generics slice 3b): the
            // generic-arg parser routes plain `Identifier` to
            // `GenericArg::Type` (it can't disambiguate a type-param
            // ref from a const-param ref without scope info). At type
            // lowering we recover: an Identifier in scope that's
            // *not* a type-param can be treated as a `ConstParam`
            // reference for the Array size position. `lower_array_type`
            // is the only place that needs this disambiguation today
            // (other `Type::Named.args` consumers stay type-only per
            // the deferred-F carveout).
            GenericArg::Type(te) => {
                if let TypeKind::Path(p) = &te.kind {
                    if p.segments.len() == 1 && p.generic_args.is_none() {
                        let name = &p.segments[0];
                        if generic_scope.contains(name) {
                            ConstArg::ConstParam(name.clone())
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
        Some(Type::Array {
            element: Box::new(element_ty),
            size,
        })
    }

    /// Walk a type alias chain until reaching a non-alias type. Guards
    /// against cycles (`type A = B; type B = A;`) by tracking visited
    /// names and returning `Type::Error` on re-entry — a later diagnostic
    /// pass can surface the cycle; the important invariant here is
    /// termination.
    fn resolve_alias_deep(&self, name: String, visited: &mut HashSet<String>) -> Type {
        if !visited.insert(name.clone()) {
            return Type::Error;
        }
        let Some(ty) = self.env.type_aliases.get(&name) else {
            return Type::Named {
                name,
                args: Vec::new(),
            };
        };
        if let Type::Named {
            name: inner,
            args: _,
        } = ty
        {
            if self.env.type_aliases.contains_key(inner) {
                return self.resolve_alias_deep(inner.clone(), visited);
            }
        }
        ty.clone()
    }

    /// Lower `Slice[T]` to `Type::Slice { element, mutable: false }`.
    /// The `mut Slice[T]` form is produced by the parser when it sees the
    /// `mut` modifier; path-type lowering always yields the read-only form.
    fn lower_slice_type(
        &mut self,
        generic_args: &Option<Vec<GenericArg>>,
        generic_scope: &[String],
    ) -> Option<Type> {
        let args = generic_args.as_ref()?;
        if args.len() != 1 {
            return None;
        }
        let element_ty = match &args[0] {
            GenericArg::Type(t) => self.lower_type_expr(t, generic_scope),
            GenericArg::Const(_) => return None,
        };
        Some(Type::Slice {
            element: Box::new(element_ty),
            mutable: false,
        })
    }

    fn primitive_type(&self, name: &str) -> Option<Type> {
        match name {
            "i8" => Some(Type::Int(IntSize::I8)),
            "i16" => Some(Type::Int(IntSize::I16)),
            "i32" => Some(Type::Int(IntSize::I32)),
            "i64" => Some(Type::Int(IntSize::I64)),
            "i128" => Some(Type::Int(IntSize::I128)),
            "u8" => Some(Type::UInt(UIntSize::U8)),
            "u16" => Some(Type::UInt(UIntSize::U16)),
            "u32" => Some(Type::UInt(UIntSize::U32)),
            "u64" => Some(Type::UInt(UIntSize::U64)),
            "u128" => Some(Type::UInt(UIntSize::U128)),
            "usize" => Some(Type::UInt(UIntSize::Usize)),
            "f32" => Some(Type::Float(FloatSize::F32)),
            "f64" => Some(Type::Float(FloatSize::F64)),
            "bool" => Some(Type::Bool),
            "char" => Some(Type::Char),
            "String" => Some(Type::Str),
            // F32/F64 are stdlib total-order wrappers (NaN sorts last, implements Eq/Ord/Hash)
            "F32" => Some(Type::Named {
                name: "F32".to_string(),
                args: vec![],
            }),
            "F64" => Some(Type::Named {
                name: "F64".to_string(),
                args: vec![],
            }),
            _ => None,
        }
    }

    fn generic_param_names(generics: &Option<GenericParams>) -> Vec<String> {
        generics
            .as_ref()
            .map(|g| g.params.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Collect inline + where-clause trait bounds keyed by the generic param's
    /// textual name. Mirrors the resolver's per-symbol `generic_param_bounds`
    /// map but is name-keyed so callers can look up bounds for a
    /// `Type::TypeParam(name)` directly. Pure AST walk — no symbol-table
    /// lookup needed.
    fn collect_param_bounds(
        generics: &Option<GenericParams>,
        where_clause: &Option<WhereClause>,
    ) -> HashMap<String, Vec<crate::ast::TraitBound>> {
        let mut map: HashMap<String, Vec<crate::ast::TraitBound>> = HashMap::new();
        // Pre-populate with every generic param name (empty bound vec
        // when none were declared). Callers rely on `enclosing_bounds`
        // doubling as the "names in scope" set — sub-step 2a's
        // unsolved-T diagnostic uses `keys()` to skip type params that
        // belong to an enclosing function/impl. Pre-2a callers used
        // `.get(name)?` and short-circuited on absence; with always-
        // present entries they get `Some(vec![])` and proceed to find
        // no matching trait-bound candidates — same final outcome.
        if let Some(ref gp) = generics {
            for param in &gp.params {
                let entry = map.entry(param.name.clone()).or_default();
                entry.extend(param.bounds.iter().cloned());
            }
        }
        if let Some(ref wc) = where_clause {
            for c in &wc.constraints {
                if let WhereConstraint::TypeBound {
                    type_name, bounds, ..
                } = c
                {
                    map.entry(type_name.clone())
                        .or_default()
                        .extend(bounds.iter().cloned());
                }
            }
        }
        map
    }

    /// Walk `ty` and resolve any `AssocProjection { param, assoc }` nodes
    /// whose `param` (after substitution it holds the concrete type name as a
    /// string) has an entry in `impl_assoc_types`. This is called after
    /// `substitute_type_params` so that `T.Item` first gets its `T` replaced
    /// by the concrete type name (stored in `param`), then gets resolved to
    /// the actual associated type.
    fn resolve_assoc_projections(&self, ty: &Type) -> Type {
        match ty {
            Type::AssocProjection { param, assoc } => {
                if let Some(resolved) = self
                    .env
                    .impl_assoc_types
                    .get(&(param.clone(), assoc.clone()))
                {
                    resolved.clone()
                } else {
                    ty.clone()
                }
            }
            Type::Tuple(elems) => Type::Tuple(
                elems
                    .iter()
                    .map(|e| self.resolve_assoc_projections(e))
                    .collect(),
            ),
            Type::Array { element, size } => Type::Array {
                element: Box::new(self.resolve_assoc_projections(element)),
                size: size.clone(),
            },
            Type::Slice { element, mutable } => Type::Slice {
                element: Box::new(self.resolve_assoc_projections(element)),
                mutable: *mutable,
            },
            Type::Ref(inner) => Type::Ref(Box::new(self.resolve_assoc_projections(inner))),
            Type::MutRef(inner) => Type::MutRef(Box::new(self.resolve_assoc_projections(inner))),
            Type::Weak(inner) => Type::Weak(Box::new(self.resolve_assoc_projections(inner))),
            Type::Pointer { is_mut, inner } => Type::Pointer {
                is_mut: *is_mut,
                inner: Box::new(self.resolve_assoc_projections(inner)),
            },
            Type::Named { name, args } => Type::Named {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|a| self.resolve_assoc_projections(a))
                    .collect(),
            },
            Type::Function {
                params,
                return_type,
            } => Type::Function {
                params: params
                    .iter()
                    .map(|p| self.resolve_assoc_projections(p))
                    .collect(),
                return_type: Box::new(self.resolve_assoc_projections(return_type)),
            },
            Type::OnceFunction {
                params,
                return_type,
            } => Type::OnceFunction {
                params: params
                    .iter()
                    .map(|p| self.resolve_assoc_projections(p))
                    .collect(),
                return_type: Box::new(self.resolve_assoc_projections(return_type)),
            },
            _ => ty.clone(),
        }
    }

    /// Return the element type produced when iterating over `ty`.
    ///
    /// For built-in collection types this consults `impl_assoc_types` keyed by
    /// `(type_name, "Item")`, then substitutes any `TypeParam` placeholders
    /// using the struct's declared `generic_params` paired with the concrete
    /// type arguments from `ty`. Falls back to `ty` itself for unknown types
    /// so the rest of the type checker can proceed without a hard error.
    fn element_type_of(&self, ty: &Type) -> Type {
        match ty {
            // Primitive borrowed views — element type is the inner type.
            Type::Array { element, .. } | Type::Slice { element, .. } => *element.clone(),
            Type::Named { name, args } => {
                // Look up the "Item" associated type for this collection.
                let Some(item_ty) = self
                    .env
                    .impl_assoc_types
                    .get(&(name.clone(), "Item".to_string()))
                else {
                    return ty.clone();
                };
                // Build substitution from generic_params → concrete args.
                // Range types store the bound type at args[0] under param "T".
                let generic_params: &[String] = self
                    .env
                    .structs
                    .get(name)
                    .map(|s| s.generic_params.as_slice())
                    .unwrap_or(&[]);
                let subs: HashMap<String, SubstValue> = generic_params
                    .iter()
                    .zip(args.iter())
                    .map(|(p, a)| (p.clone(), SubstValue::Type(a.clone())))
                    .collect();
                substitute_type_params(item_ty, &subs)
            }
            _ => ty.clone(),
        }
    }

    // ── Build Type Environment (Pass 1) ─────────────────────────

    fn build_type_env(&mut self) {
        // Two-step stdlib seeding (CR-202):
        //   1. Walk every item in `runtime/stdlib/*.kara` (baked into
        //      the binary via `prelude::STDLIB_PROGRAMS`) and register
        //      it through the same `env_add_*` paths user items use.
        //   2. Register the residual compiler-internal entries that
        //      have no syntactic representation in baked source —
        //      `impl_assoc_types` mappings, the `Iterator` parametric
        //      pseudo-struct, primitive operator impls, etc.
        self.register_baked_stdlib();
        self.register_compiler_intrinsic_env();

        let items: Vec<Item> = self.program.items.clone();

        // Stub pre-pass for self-referential shared types. Field-type
        // lowering inside `env_add_struct` / `env_add_enum` calls
        // `lower_type_expr`, which checks `env.structs` / `env.enums`
        // for `is_shared` to decide whether to return `Type::Shared(name)`
        // vs `Type::Named { name, args: [] }`. Without the stub, a
        // shared struct's own self-reference resolves before the parent
        // entry is inserted and falls through to `Type::Named` — later
        // uses of the same name resolve to `Type::Shared`, and the two
        // representations fail `types_compatible` with the visually
        // identical "expected 'Option<S>', found 'Option<S>'"
        // diagnostic. Insert empty-fields stubs first so the self-ref
        // path hits the populated entry; the real pass below overwrites
        // with the fully-lowered fields.
        for item in &items {
            match item {
                Item::StructDef(s) => {
                    let gp = Self::generic_param_names(&s.generic_params);
                    let derived_traits = extract_derived_traits(&s.attributes);
                    self.env.structs.insert(
                        s.name.clone(),
                        StructInfo {
                            generic_params: gp,
                            fields: Vec::new(),
                            derived_traits,
                            no_rc: s.no_rc,
                            is_shared: s.is_shared,
                        },
                    );
                }
                Item::EnumDef(e) => {
                    let gp = Self::generic_param_names(&e.generic_params);
                    let derived_traits = extract_derived_traits(&e.attributes);
                    self.env.enums.insert(
                        e.name.clone(),
                        EnumInfo {
                            generic_params: gp,
                            variants: Vec::new(),
                            derived_traits,
                            is_shared: e.is_shared,
                        },
                    );
                }
                _ => {}
            }
        }

        for item in &items {
            match item {
                Item::StructDef(s) => self.env_add_struct(s),
                Item::EnumDef(e) => self.env_add_enum(e),
                Item::Function(f) => self.env_add_function(f),
                Item::TraitDef(t) => self.env_add_trait(t),
                Item::TraitAlias(t) => self.env_add_trait_alias(t),
                Item::MarkerTrait(t) => self.env_add_marker_trait(t),
                Item::ImplBlock(i) => self.env_add_impl(i),
                Item::ConstDecl(c) => self.env_add_const(c),
                Item::TypeAlias(t) => self.env_add_type_alias(t),
                Item::ExternFunction(e) => self.env_add_extern_function(e),
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(f) => self.env_add_extern_function(f),
                            ExternItem::OpaqueType(o) => self.env_add_opaque_foreign_type(o),
                        }
                    }
                }
                Item::DistinctType(d) => self.env_add_distinct_type(d),
                _ => {}
            }
        }

        // Cross-module origins: record every imported item's declared
        // visibility in its origin module so `check_signature_visibility`
        // and `infer_field_access` can enforce three-level rules across
        // modules. Silent when `tree` is unset (single-file mode).
        self.collect_import_origins();
    }

    /// Walk `self.program.items` imports, look each target up in the
    /// `ProgramTree`, and stash the (origin path, origin visibility) pair
    /// under the locally-bound name. CR-24 slice 6 (slice 7 extension:
    /// chases `pub import` re-export chains so cross-module field access and
    /// signature-leak checks see the canonical defining module, not the
    /// re-exporter).
    fn collect_import_origins(&mut self) {
        let Some(tree) = self.tree else {
            return;
        };
        // Items collected for env_add_* registration. Done in two
        // passes so the iteration borrow on `self.program.items` ends
        // before the env_add_* methods take `&mut self`.
        let mut imported_items: Vec<(String, crate::ast::Item)> = Vec::new();
        for item in &self.program.items {
            let Item::Import(imp) = item else { continue };
            for ii in &imp.items {
                // Canonical origin walks `pub import` re-exports to the
                // defining module. Falls back to the direct target when no
                // matching item exists (E0225 handles that case in the
                // resolver — typechecker skips the entry silently).
                let Some((origin_path, origin_name)) =
                    crate::module::canonical_origin(tree, &imp.path, &ii.name)
                else {
                    continue;
                };
                let Some(&origin_id) = tree.graph.by_path.get::<[String]>(&origin_path) else {
                    continue;
                };
                let origin_module = tree.module(origin_id);
                if let Some(vis) = find_item_visibility(origin_module, &origin_name) {
                    let bound = ii.alias.clone().unwrap_or_else(|| ii.name.clone());
                    self.type_origins
                        .insert(bound, (origin_path, origin_name.clone(), vis));
                }
                // Theme 4 follow-up (2026-05-10) — pull the imported
                // item's full definition into the local env so per-
                // module typecheck sees imported structs / enums /
                // traits as first-class types. Without this, struct
                // literals on imported types fired `E0207 NotAStruct`
                // even though resolution succeeded. The original CR-24
                // slice-6 surface only carried `(origin_path, name,
                // vis)` in `type_origins`; the full definition is
                // needed for struct-literal validation, variant
                // construction, and trait-method dispatch.
                for oitem in &origin_module.items {
                    let matches = match oitem {
                        Item::StructDef(s) => s.name == origin_name,
                        Item::EnumDef(e) => e.name == origin_name,
                        Item::TraitDef(t) => t.name == origin_name,
                        Item::TraitAlias(t) => t.name == origin_name,
                        Item::MarkerTrait(t) => t.name == origin_name,
                        _ => false,
                    };
                    if matches {
                        imported_items.push((
                            ii.alias.clone().unwrap_or_else(|| ii.name.clone()),
                            oitem.clone(),
                        ));
                        break;
                    }
                }
            }
        }
        for (bound_name, item) in imported_items {
            // Skip when an item with the bound name is already registered
            // — local definitions and stdlib bakeds win over imports.
            match &item {
                Item::StructDef(s) => {
                    if self.env.structs.contains_key(&bound_name) {
                        continue;
                    }
                    // Re-bind the struct under its locally-bound name so
                    // `infer_struct_literal`'s lookup succeeds. The
                    // canonical name is preserved in `type_origins` for
                    // visibility / canonicalization checks.
                    let mut local_def = s.clone();
                    local_def.name = bound_name;
                    self.env_add_struct(&local_def);
                }
                Item::EnumDef(e) => {
                    if self.env.enums.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = e.clone();
                    local_def.name = bound_name;
                    self.env_add_enum(&local_def);
                }
                Item::TraitDef(t) => {
                    if self.env.traits.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = t.clone();
                    local_def.name = bound_name;
                    self.env_add_trait(&local_def);
                }
                Item::TraitAlias(t) => {
                    if self.env.traits.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = t.clone();
                    local_def.name = bound_name;
                    self.env_add_trait_alias(&local_def);
                }
                Item::MarkerTrait(t) => {
                    if self.env.traits.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = t.clone();
                    local_def.name = bound_name;
                    self.env_add_marker_trait(&local_def);
                }
                _ => {}
            }
        }
    }

    /// Walk every item in `runtime/stdlib/*.kara` (baked into the
    /// binary via [`crate::prelude::STDLIB_PROGRAMS`]) and register it
    /// through the same `env_add_*` paths user items use.
    ///
    /// CR-202 incrementally migrated the prelude surface to baked
    /// source; this function is the single entry point that pulls
    /// every type, trait, and impl declaration out of stdlib source
    /// files into the typechecker's environment. See
    /// `runtime/stdlib/` for the authoritative declarations.
    fn register_baked_stdlib(&mut self) {
        let baked: Vec<Item> = crate::prelude::STDLIB_PROGRAMS
            .iter()
            .flat_map(|(_, p)| p.items.iter().cloned())
            .collect();
        for item in &baked {
            match item {
                Item::Function(f) => self.env_add_function(f),
                Item::StructDef(s) => self.env_add_struct(s),
                Item::EnumDef(e) => self.env_add_enum(e),
                Item::TraitDef(t) => self.env_add_trait(t),
                Item::ImplBlock(i) => self.env_add_impl(i),
                Item::TraitAlias(_)
                | Item::MarkerTrait(_)
                | Item::ConstDecl(_)
                | Item::TypeAlias(_)
                | Item::ExternFunction(_)
                | Item::ExternBlock(_)
                | Item::DistinctType(_)
                | Item::EffectResource(_)
                | Item::EffectGroup(_)
                | Item::EffectVerbDecl(_)
                | Item::UseDecl(_)
                | Item::Import(_)
                | Item::LayoutDef(_)
                | Item::AliasDecl(_)
                | Item::IndependentDecl(_) => {
                    // Not yet exercised by baked stdlib source — broaden
                    // the match if a future stdlib file uses one of these
                    // item kinds.
                }
            }
        }
    }

    /// Register the residual compiler-internal entries that have no
    /// syntactic representation in baked source. CR-202 slice 6.5
    /// scrubbed this function down from the original
    /// `register_builtin_types` once the migratable surface had moved
    /// to `runtime/stdlib/*.kara`. What remains is:
    ///
    /// - `impl_assoc_types` mappings keyed `(type, assoc_name) -> Type`
    ///   that thread collection types to their iterator element type.
    ///   `Map[K, V]` yields `(K, V)`, the rest yield `T`.
    /// - The `Iterator` and `Array` parametric pseudo-structs in
    ///   `env.structs`. `Iterator` is a trait per design.md but is
    ///   also treated as a parametric pseudo-type at this layer so
    ///   `for x in v.iter()` resolves through the same
    ///   `impl_assoc_types` path as concrete collections. `Array[T]`
    ///   is a built-in primitive (lowered specially in
    ///   `lower_path_type`); a separate primitive-vs-struct design CR
    ///   would migrate it to baked source.
    /// - The `Range*` family of typechecker-internal iteration types.
    ///   Constructed from `a..b` syntax; never user-referenced as
    ///   `Range[T]`, so a baked struct adds no value.
    /// - Module-path free-function aliases (`env.args`, `env.var`,
    ///   `process.exit`). These cannot be expressed as `impl Env { fn args() }`
    ///   blocks because the lowercase identifier (`env`) doesn't name a
    ///   type; they're a syntactically distinct surface that aliases the
    ///   capitalized `Env.args` / `Env.var` (which now live in baked source).
    ///   The full ambient effect-resource surface — Stdin, Stdout, Stderr,
    ///   FileSystem, Env, Clock, RandomSource — has migrated to
    ///   `runtime/stdlib/io.kara` via the companion-struct pattern; the
    ///   `EffectResource` symbol kind and the baked struct coexist because
    ///   baked source bypasses the resolver, so each resource stays a
    ///   `SymbolKind::EffectResource` for `with_provider[R]` purposes while
    ///   `env.structs` / `env.impls` carries the type+method shape for
    ///   `infer_path_type` lookups.
    /// - The primitive operator impl table via [`Self::register_stdlib_impls`]
    ///   (`impl Add for i32`, `impl Eq for u8`, the numeric widening
    ///   `From` impls, …). Documented as permanently programmatic — a
    ///   compiler-internal dispatch table, not user-readable type
    ///   declarations.
    fn register_compiler_intrinsic_env(&mut self) {
        let t = || Type::TypeParam("T".to_string());
        let k = || Type::TypeParam("K".to_string());
        let v = || Type::TypeParam("V".to_string());

        // Iterator / Array parametric pseudo-structs (see fn doc).
        for name in &["Array", "Iterator"] {
            self.env
                .structs
                .entry(name.to_string())
                .or_insert_with(|| StructInfo {
                    generic_params: vec!["T".to_string()],
                    fields: vec![],
                    derived_traits: HashSet::new(),
                    no_rc: false,
                    is_shared: false,
                });
            self.env
                .impl_assoc_types
                .insert((name.to_string(), "Item".to_string()), t());
        }

        // Iterator-element-type (`Item`) mappings for baked collection
        // types. The structs themselves are baked; the assoc-type
        // mapping has no syntactic representation in baked source.
        for name in &["Vec", "VecDeque", "SortedSet", "Set", "Peekable", "Slice"] {
            self.env
                .impl_assoc_types
                .insert((name.to_string(), "Item".to_string()), t());
        }
        self.env.impl_assoc_types.insert(
            ("Map".to_string(), "Item".to_string()),
            Type::Tuple(vec![k(), v()]),
        );

        // Range family — typechecker-internal types constructed from
        // `a..b` syntax. Both struct shape and assoc-type mapping
        // registered here.
        for name in &[
            "Range",
            "RangeInclusive",
            "RangeFrom",
            "RangeTo",
            "RangeToInclusive",
        ] {
            self.env
                .structs
                .entry(name.to_string())
                .or_insert_with(|| StructInfo {
                    generic_params: vec!["T".to_string()],
                    fields: vec![],
                    derived_traits: HashSet::new(),
                    no_rc: false,
                    is_shared: false,
                });
            self.env
                .impl_assoc_types
                .insert((name.to_string(), "Item".to_string()), t());
        }

        // ── Standard I/O function signatures ───────────────────────────────────
        //
        // The capitalized I/O resource methods (`Stdin.read_line`,
        // `Stdout.println`, `FileSystem.write`, `Env.args`, …) live in baked
        // stdlib source (`runtime/stdlib/io.kara`) as
        // `impl <Resource> { #[compiler_builtin] fn ... }` blocks. The
        // signatures flow through `register_baked_stdlib` → `env_add_impl` →
        // `env.impls`, found by `resolve_path_type`'s impl lookup
        // (`infer_path_type` line ~7227) before the
        // `env.functions.get("Resource.method")` fallback.
        //
        // The lowercase module-path forms `env.args()` / `env.var(name)` stay
        // here — they're aliases that share dispatch with the capitalized
        // form (interpreter routes `env` → `Env` via the alias map at
        // `eval_method_call`) but the lowercase surface has no syntactic
        // representation as an `impl Env { fn args() }` block.

        let vec_string = Type::Named {
            name: "Vec".to_string(),
            args: vec![Type::Str],
        };
        let args_sig = FunctionSig {
            generic_params: vec![],
            param_names: vec![],
            params: vec![],
            return_type: vec_string,
            where_clause: None,
        };
        self.env.functions.insert("env.args".to_string(), args_sig);

        let result_str_var = Type::Named {
            name: "Result".to_string(),
            args: vec![
                Type::Str,
                Type::Named {
                    name: "VarError".to_string(),
                    args: vec![],
                },
            ],
        };
        let var_sig = FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("name".to_string())],
            params: vec![Type::Str],
            return_type: result_str_var,
            where_clause: None,
        };
        self.env.functions.insert("env.var".to_string(), var_sig);

        // `env.set(name, value)` — lowercase alias for `Env.set`. The
        // capitalized form lives in baked stdlib (`runtime/stdlib/io.kara`)
        // alongside `Env.var` / `Env.args`; this lowercase entry mirrors the
        // `env.var` registration above. Carries `writes(Env)` (seeded in
        // `effectchecker::check`) so callers must declare it.
        let set_sig = FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("name".to_string()), Some("value".to_string())],
            params: vec![Type::Str, Type::Str],
            return_type: Type::Unit,
            where_clause: None,
        };
        self.env.functions.insert("env.set".to_string(), set_sig);

        // Register process.exit in the function table
        self.env.functions.insert(
            "process.exit".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("code".to_string())],
                params: vec![Type::Int(IntSize::I32)],
                return_type: Type::Never,
                where_clause: None,
            },
        );

        // ── Stats namespace ──────────────────────────────────────────────────
        // CR-202 slice 6.3: every Stats method now lives in baked source as
        // `impl Stats { #[compiler_builtin] fn ... }`. See
        // `runtime/stdlib/stats.kara`.

        // ── Regex namespace ──────────────────────────────────────────────────
        // CR-202 slice 6.3: every Regex method now lives in baked source as
        // `impl Regex { #[compiler_builtin] fn ... }`. See
        // `runtime/stdlib/regex.kara`. Instance-method calls
        // (`r.is_match(s)`, …) still route through `infer_regex_method` /
        // `eval_regex_method`; only the path-call form `Regex.compile(...)`
        // and the env.functions surface migrated.

        // ── std.http namespace ───────────────────────────────────────────────
        // CR-202 slice 6.3: every Client / Response / HttpError method now
        // lives in baked source as `impl <Type> { #[compiler_builtin] fn ... }`.
        // See `runtime/stdlib/http.kara`. Instance-method calls still route
        // through `infer_http_*_method` / `eval_http_*_method`; only
        // `Client.new()` (associated) and the env.functions surface migrated.

        // ── std.encoding namespace (Base64 / Hex / Url) ──────────────────────
        // CR-202 slice 6.3: every Base64 / Hex / Url method now lives in
        // baked source as `impl <Type> { #[compiler_builtin] fn ... }`.
        // See `runtime/stdlib/encoding.kara`. Interpreter dispatches each
        // call by matching on the path string in `eval_encoding_fn`.

        // `register_stdlib_traits` retired — every trait it registered
        // moved to baked source under `runtime/stdlib/*.kara` across
        // CR-202 slices 5a–5l, 6.2a–6.2e. The only remaining hardcoded
        // trait registration is the `Iterator` / `IntoIterator` pseudo-
        // struct + assoc-type pair below (slice 6.2d migrated the trait
        // shape; the pseudo-struct stays in code).
        self.register_stdlib_impls();
    }

    /// Register stdlib trait impls for primitives, String, and F32/F64 wrappers.
    /// Operator dispatch in Step 6 keys off these. Generic-target impls
    /// (Vec/Option/Result Eq/Ord) are deferred — they need bound checking
    /// against type arguments, which the impl table doesn't model yet.
    fn register_stdlib_impls(&mut self) {
        // Method-signature builders. All operator methods are homogeneous in v1
        // (`fn op(self, rhs: Self) -> Self`). Eq/Ord return bool / Ordering.
        let binop = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("rhs".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: ty.clone(),
            where_clause: None,
        };
        let unop = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into())],
            params: vec![ty.clone()],
            return_type: ty.clone(),
            where_clause: None,
        };
        let eq_sig = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("other".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: Type::Bool,
            where_clause: None,
        };
        let ord_sig = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("other".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: Type::Named {
                name: "Ordering".into(),
                args: vec![],
            },
            where_clause: None,
        };

        let signed_ints: &[(&str, Type)] = &[
            ("i8", Type::Int(IntSize::I8)),
            ("i16", Type::Int(IntSize::I16)),
            ("i32", Type::Int(IntSize::I32)),
            ("i64", Type::Int(IntSize::I64)),
        ];
        let unsigned_ints: &[(&str, Type)] = &[
            ("u8", Type::UInt(UIntSize::U8)),
            ("u16", Type::UInt(UIntSize::U16)),
            ("u32", Type::UInt(UIntSize::U32)),
            ("u64", Type::UInt(UIntSize::U64)),
            ("usize", Type::UInt(UIntSize::Usize)),
        ];
        let floats: &[(&str, Type)] = &[
            ("f32", Type::Float(FloatSize::F32)),
            ("f64", Type::Float(FloatSize::F64)),
        ];
        let f_wrappers: &[(&str, Type)] = &[
            (
                "F32",
                Type::Named {
                    name: "F32".into(),
                    args: vec![],
                },
            ),
            (
                "F64",
                Type::Named {
                    name: "F64".into(),
                    args: vec![],
                },
            ),
        ];

        let all_ints: Vec<(&str, Type)> = signed_ints
            .iter()
            .chain(unsigned_ints.iter())
            .cloned()
            .collect();
        let all_numeric: Vec<(&str, Type)> =
            all_ints.iter().chain(floats.iter()).cloned().collect();
        let signed_numeric: Vec<(&str, Type)> =
            signed_ints.iter().chain(floats.iter()).cloned().collect();

        // Arithmetic on all numeric primitives (binary).
        for (target, ty) in &all_numeric {
            for (trait_name, method) in [
                ("Add", "add"),
                ("Sub", "sub"),
                ("Mul", "mul"),
                ("Div", "div"),
                ("Rem", "rem"),
            ] {
                self.register_builtin_impl(trait_name, target, vec![(method, binop(ty))]);
            }
        }
        // Neg on signed integers and floats only.
        for (target, ty) in &signed_numeric {
            self.register_builtin_impl("Neg", target, vec![("neg", unop(ty))]);
        }
        // Bitwise BitAnd/BitOr/BitXor on integers + bool.
        for (target, ty) in all_ints
            .iter()
            .chain(std::iter::once(&("bool", Type::Bool)))
        {
            for (trait_name, method) in [
                ("BitAnd", "bitand"),
                ("BitOr", "bitor"),
                ("BitXor", "bitxor"),
            ] {
                self.register_builtin_impl(trait_name, target, vec![(method, binop(ty))]);
            }
        }
        // Shifts on integers only (rhs = Self per v1 homogeneity rule).
        for (target, ty) in &all_ints {
            for (trait_name, method) in [("Shl", "shl"), ("Shr", "shr")] {
                self.register_builtin_impl(trait_name, target, vec![(method, binop(ty))]);
            }
        }
        // Not on integers + bool.
        for (target, ty) in all_ints
            .iter()
            .chain(std::iter::once(&("bool", Type::Bool)))
        {
            self.register_builtin_impl("Not", target, vec![("not", unop(ty))]);
        }
        // Eq + Ord on integers, bool, char, String, F32/F64 wrappers.
        // Floats (f32/f64) deliberately excluded — IEEE NaN breaks Eq/Ord.
        let eq_ord_targets: Vec<(&str, Type)> = all_ints
            .iter()
            .cloned()
            .chain(std::iter::once(("bool", Type::Bool)))
            .chain(std::iter::once(("char", Type::Char)))
            .chain(std::iter::once(("String", Type::Str)))
            .chain(f_wrappers.iter().cloned())
            .collect();
        // `ne`/`lt`/`le`/`gt`/`ge` share the bool-returning shape that
        // `eq_sig` produces, so reuse it for them. `cmp` is the only Ord
        // method with the Ordering-returning shape. Registering these makes
        // the names directly callable (e.g. `i32.lt(a, b)`) alongside the
        // operator-lowered form.
        for (target, ty) in &eq_ord_targets {
            let cmp_bool = eq_sig(ty);
            self.register_builtin_impl(
                "Eq",
                target,
                vec![("eq", cmp_bool.clone()), ("ne", cmp_bool.clone())],
            );
            self.register_builtin_impl(
                "Ord",
                target,
                vec![
                    ("cmp", ord_sig(ty)),
                    ("lt", cmp_bool.clone()),
                    ("le", cmp_bool.clone()),
                    ("gt", cmp_bool.clone()),
                    ("ge", cmp_bool),
                ],
            );
        }
        // Add for String — heap concatenation. Effect tracking (allocates(Heap))
        // wired in Step 6 when operator lowering routes through this impl.
        self.register_builtin_impl("Add", "String", vec![("add", binop(&Type::Str))]);

        // Numeric widening: register `impl From[Source] for Target` for every
        // lossless source→target pair. `target.from(value)` then dispatches
        // through this table; the source type disambiguates between impls
        // sharing a target.
        let from_sig = |source: &Type, target: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("value".into())],
            params: vec![source.clone()],
            return_type: target.clone(),
            where_clause: None,
        };
        let widening_pairs: &[(&str, Type, &str, Type)] = &[
            // signed → signed
            ("i8", Type::Int(IntSize::I8), "i16", Type::Int(IntSize::I16)),
            ("i8", Type::Int(IntSize::I8), "i32", Type::Int(IntSize::I32)),
            ("i8", Type::Int(IntSize::I8), "i64", Type::Int(IntSize::I64)),
            (
                "i16",
                Type::Int(IntSize::I16),
                "i32",
                Type::Int(IntSize::I32),
            ),
            (
                "i16",
                Type::Int(IntSize::I16),
                "i64",
                Type::Int(IntSize::I64),
            ),
            (
                "i32",
                Type::Int(IntSize::I32),
                "i64",
                Type::Int(IntSize::I64),
            ),
            // unsigned → unsigned
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "u16",
                Type::UInt(UIntSize::U16),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "u32",
                Type::UInt(UIntSize::U32),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "u64",
                Type::UInt(UIntSize::U64),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "usize",
                Type::UInt(UIntSize::Usize),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "u32",
                Type::UInt(UIntSize::U32),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "u64",
                Type::UInt(UIntSize::U64),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "usize",
                Type::UInt(UIntSize::Usize),
            ),
            (
                "u32",
                Type::UInt(UIntSize::U32),
                "u64",
                Type::UInt(UIntSize::U64),
            ),
            // unsigned → wider signed (always lossless)
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "i16",
                Type::Int(IntSize::I16),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "i32",
                Type::Int(IntSize::I32),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "i64",
                Type::Int(IntSize::I64),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "i32",
                Type::Int(IntSize::I32),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "i64",
                Type::Int(IntSize::I64),
            ),
            (
                "u32",
                Type::UInt(UIntSize::U32),
                "i64",
                Type::Int(IntSize::I64),
            ),
            // float widening
            (
                "f32",
                Type::Float(FloatSize::F32),
                "f64",
                Type::Float(FloatSize::F64),
            ),
        ];
        for (_src_name, src_ty, tgt_name, tgt_ty) in widening_pairs {
            self.register_builtin_impl("From", tgt_name, vec![("from", from_sig(src_ty, tgt_ty))]);
        }
    }

    /// Returns `true` when `ty` is a distinct type that derives `Arithmetic`.
    fn distinct_type_has_arithmetic(&self, ty: &Type) -> bool {
        if let Type::Named { name, args } = ty {
            if args.is_empty() {
                return self
                    .env
                    .distinct_types
                    .get(name)
                    .is_some_and(|t| t.contains("Arithmetic"));
            }
        }
        false
    }

    /// Check whether a type is Copy (primitive or derives Copy).
    fn is_type_copy(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Unit
            | Type::Never
            | Type::Error => true,
            Type::Tuple(types) => types.iter().all(|t| self.is_type_copy(t)),
            // Array[T, N] is Copy iff T is Copy.
            Type::Array { element, .. } => self.is_type_copy(element),
            // Slice[T] is unconditionally Copy; mut Slice[T] is not.
            Type::Slice { mutable, .. } => !mutable,
            Type::Named { name, args } => {
                // Option[T] / Result[T, E] are Copy when all type args are Copy.
                if matches!(name.as_str(), "Option" | "Result") {
                    return args.iter().all(|a| self.is_type_copy(a));
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Copy")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Copy")
                } else if let Some(traits) = self.env.distinct_types.get(name) {
                    traits.contains("Copy")
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Validate that #[derive(Copy)] structs/enums have all-Copy fields, and
    /// that distinct types with #[derive(Copy)] have a Copy base type.
    fn validate_derive_copy(&mut self) {
        self.validate_derived_trait("Copy", |this, ty| this.is_type_copy(ty));
        // Check distinct types: base type must be Copy.
        let distinct_items: Vec<_> = self
            .program
            .items
            .iter()
            .filter_map(|item| {
                if let Item::DistinctType(d) = item {
                    let traits = extract_derived_traits(&d.attributes);
                    if traits.contains("Copy") {
                        return Some((d.name.clone(), d.span.clone(), d.base_type.clone()));
                    }
                }
                None
            })
            .collect();
        for (name, span, base_ty_expr) in distinct_items {
            let base_ty = self.lower_type_expr(&base_ty_expr, &[]);
            if !self.is_type_copy(&base_ty) {
                self.type_error(
                    format!(
                        "distinct type '{}' derives Copy but its base type '{}' is not Copy",
                        name,
                        type_display(&base_ty)
                    ),
                    span,
                    TypeErrorKind::TypeMismatch,
                );
            }
        }
    }

    /// Validate that every type deriving Copy also derives Clone.
    fn validate_copy_implies_clone(&mut self) {
        let items: Vec<_> = self.program.items.clone();
        for item in &items {
            match item {
                Item::StructDef(s) => {
                    let traits = extract_derived_traits(&s.attributes);
                    if traits.contains("Copy") && !traits.contains("Clone") {
                        self.type_error(
                            format!(
                                "struct '{}' derives Copy but not Clone; Copy requires Clone",
                                s.name
                            ),
                            s.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                Item::EnumDef(e) => {
                    let traits = extract_derived_traits(&e.attributes);
                    if traits.contains("Copy") && !traits.contains("Clone") {
                        self.type_error(
                            format!(
                                "enum '{}' derives Copy but not Clone; Copy requires Clone",
                                e.name
                            ),
                            e.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                Item::DistinctType(d) => {
                    let traits = extract_derived_traits(&d.attributes);
                    if traits.contains("Copy") && !traits.contains("Clone") {
                        self.type_error(
                            format!(
                                "distinct type '{}' derives Copy but not Clone; Copy requires Clone",
                                d.name
                            ),
                            d.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Validate that every `#[derive(Trait)]` on a struct/enum implies all
    /// fields recursively support `Trait`. Reports one diagnostic per
    /// offending field. `Copy` is handled separately via `validate_derive_copy`
    /// so the message can reference `is_type_copy`'s broader rules.
    fn validate_derived_traits_recursive(&mut self) {
        self.validate_derived_trait("Eq", |this, ty| this.type_supports_eq(ty));
        self.validate_derived_trait("PartialEq", |this, ty| this.type_supports_partial_eq(ty));
        self.validate_derived_trait("Hash", |this, ty| this.type_supports_hash(ty));
        self.validate_derived_trait("Ord", |this, ty| this.type_supports_ord(ty));
        self.validate_derived_trait("PartialOrd", |this, ty| this.type_supports_partial_ord(ty));
        self.validate_derive_display_on_enums();
    }

    /// Compound-payload enum codegen (Slice CP, CP5 carve-out) —
    /// reject enum variants whose payload field type is itself a
    /// (non-shared) user enum. v1 ships single-level enum-payload
    /// nesting only; the layout pass cannot size a recursively-nested
    /// payload area without an infinite-recursion guard, and the
    /// canonical workaround is to wrap the inner enum in `Vec`,
    /// `shared` (RC pointer), or a `Box`-style indirection. Recursion
    /// through `Vec[T]`, `Slice[T]`, tuples, or `shared` enums is
    /// fine — those layers stop the size recursion at one indirection.
    fn validate_enum_payload_no_nested_enum(&mut self) {
        // Collect enum names for the carve-out check. `shared` enums
        // are heap-allocated via RC, so a payload field of type
        // `SharedFoo` is a single pointer word and is allowed.
        let value_enum_names: std::collections::HashSet<String> = self
            .program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::EnumDef(e) if !e.is_shared => Some(e.name.clone()),
                _ => None,
            })
            .collect();

        // Walk every enum variant and inspect its payload field types.
        // The payload field's `TypeExpr` -> head segment is the
        // user-visible type name; if that name is a value enum, emit
        // the diagnostic. We only flag the *direct* head; recursion
        // through `Vec[Inner]` etc. is intentionally allowed
        // (CP5 carve-out is about size-recursion, not name presence).
        let items: Vec<_> = self.program.items.clone();
        for item in &items {
            if let Item::EnumDef(e) = item {
                if e.is_shared {
                    continue;
                }
                for variant in &e.variants {
                    let field_tys: Vec<&TypeExpr> = match &variant.kind {
                        VariantKind::Unit => Vec::new(),
                        VariantKind::Tuple(tys) => tys.iter().collect(),
                        VariantKind::Struct(fields) => fields.iter().map(|f| &f.ty).collect(),
                    };
                    for ty in field_tys {
                        if let TypeKind::Path(path) = &ty.kind {
                            if let Some(head) = path.segments.first() {
                                if value_enum_names.contains(head) {
                                    self.type_error(
                                        format!(
                                            "error[E_ENUM_NESTED_ENUM_PAYLOAD]: enum variant \
                                             '{}.{}' has a payload of nested enum type '{}' — \
                                             v1 only supports up to one level of enum nesting; \
                                             either flatten the variant, mark the inner enum as \
                                             `shared` (RC pointer), or wrap it in a `Vec` / \
                                             collection layer",
                                            e.name, variant.name, head
                                        ),
                                        variant.span.clone(),
                                        TypeErrorKind::TypeMismatch,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// `#[derive(Display)]` on enums only works for all-unit-variant enums.
    /// Reject any enum that has a tuple or struct variant.
    fn validate_derive_display_on_enums(&mut self) {
        let display_enums: Vec<_> = self
            .env
            .enums
            .iter()
            .filter(|(_, info)| info.derived_traits.contains("Display"))
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect();

        for (name, info) in display_enums {
            let enum_span = self.program.items.iter().find_map(|item| {
                if let Item::EnumDef(e) = item {
                    if e.name == name {
                        return Some(e.span.clone());
                    }
                }
                None
            });
            let Some(span) = enum_span else {
                continue;
            };
            for (variant_name, variant_info) in &info.variants {
                if !matches!(variant_info, VariantTypeInfo::Unit) {
                    self.type_error(
                        format!(
                            "enum '{}' derives Display but variant '{}' is not a unit variant; \
                             #[derive(Display)] only works on all-unit-variant enums — \
                             implement Display manually for enums with data variants",
                            name, variant_name
                        ),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
        }
    }

    /// Validate `#[derive(Arithmetic)]` usage:
    /// - Reject on struct/enum (must use manual impls).
    /// - Reject on distinct types whose base type is non-numeric.
    fn validate_derive_arithmetic(&mut self) {
        let items: Vec<_> = self.program.items.clone();
        for item in &items {
            match item {
                Item::StructDef(s)
                    if extract_derived_traits(&s.attributes).contains("Arithmetic") =>
                {
                    self.type_error(
                        format!(
                            "#[derive(Arithmetic)] is only valid on `distinct type`, not on \
                             struct '{}'; use manual trait impls for structs",
                            s.name
                        ),
                        s.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Item::EnumDef(e)
                    if extract_derived_traits(&e.attributes).contains("Arithmetic") =>
                {
                    self.type_error(
                        format!(
                            "#[derive(Arithmetic)] is only valid on `distinct type`, not on \
                             enum '{}'; use manual trait impls for enums",
                            e.name
                        ),
                        e.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Item::DistinctType(d)
                    if extract_derived_traits(&d.attributes).contains("Arithmetic") =>
                {
                    let base_ty = self.lower_type_expr(&d.base_type, &[]);
                    if !is_numeric(&base_ty) {
                        self.type_error(
                            format!(
                                "distinct type '{}' derives Arithmetic but its base type \
                                 '{}' is not numeric",
                                d.name,
                                type_display(&base_ty)
                            ),
                            d.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Walk every struct/enum that derives `trait_name`; emit a diagnostic for
    /// each field whose type fails `supports`. Skips types that aren't in the
    /// program AST — those are compiler-provided built-ins (`F32`, `F64`,
    /// `Ordering`, `MemoryOrdering`) whose derived-trait bundles are
    /// hand-verified.
    fn validate_derived_trait(
        &mut self,
        trait_name: &str,
        supports: impl Fn(&Self, &Type) -> bool,
    ) {
        let structs: Vec<_> = self
            .env
            .structs
            .iter()
            .filter(|(_, info)| info.derived_traits.contains(trait_name))
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect();

        for (name, info) in structs {
            let struct_span = self.program.items.iter().find_map(|item| {
                if let Item::StructDef(s) = item {
                    if s.name == name {
                        return Some(s.span.clone());
                    }
                }
                None
            });
            let Some(struct_span) = struct_span else {
                continue;
            };
            for (field_name, field_ty, _) in &info.fields {
                if !supports(self, field_ty) {
                    let span = struct_span.clone();
                    self.type_error(
                        format!(
                            "struct '{}' derives {} but field '{}' has non-{} type '{}'",
                            name,
                            trait_name,
                            field_name,
                            trait_name,
                            type_display(field_ty)
                        ),
                        span,
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
        }

        let enums: Vec<_> = self
            .env
            .enums
            .iter()
            .filter(|(_, info)| info.derived_traits.contains(trait_name))
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect();

        for (name, info) in enums {
            let enum_span = self.program.items.iter().find_map(|item| {
                if let Item::EnumDef(e) = item {
                    if e.name == name {
                        return Some(e.span.clone());
                    }
                }
                None
            });
            let Some(enum_span) = enum_span else {
                continue;
            };
            for (variant_name, variant_info) in &info.variants {
                let bad_fields: Vec<(String, Type)> = match variant_info {
                    VariantTypeInfo::Unit => Vec::new(),
                    VariantTypeInfo::Tuple(types) => types
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| !supports(self, t))
                        .map(|(i, t)| (i.to_string(), t.clone()))
                        .collect(),
                    VariantTypeInfo::Struct(fields) => fields
                        .iter()
                        .filter(|(_, t)| !supports(self, t))
                        .map(|(n, t)| (n.clone(), t.clone()))
                        .collect(),
                };
                for (field_ref, field_ty) in bad_fields {
                    self.type_error(
                        format!(
                            "enum '{}' derives {} but variant '{}' field '{}' has non-{} type '{}'",
                            name,
                            trait_name,
                            variant_name,
                            field_ref,
                            trait_name,
                            type_display(&field_ty)
                        ),
                        enum_span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
        }
    }

    fn env_add_struct(&mut self, s: &StructDef) {
        let gp = Self::generic_param_names(&s.generic_params);
        let fields: Vec<(String, Type, bool)> = s
            .fields
            .iter()
            .map(|f| (f.name.clone(), self.lower_type_expr(&f.ty, &gp), f.is_pub))
            .collect();
        let derived_traits = extract_derived_traits(&s.attributes);
        self.env.structs.insert(
            s.name.clone(),
            StructInfo {
                generic_params: gp,
                fields,
                derived_traits,
                no_rc: s.no_rc,
                is_shared: s.is_shared,
            },
        );
    }

    fn env_add_enum(&mut self, e: &EnumDef) {
        let gp = Self::generic_param_names(&e.generic_params);
        let variants: Vec<(String, VariantTypeInfo)> = e
            .variants
            .iter()
            .map(|v| {
                let vtype = match &v.kind {
                    VariantKind::Unit => VariantTypeInfo::Unit,
                    VariantKind::Tuple(types) => VariantTypeInfo::Tuple(
                        types.iter().map(|t| self.lower_type_expr(t, &gp)).collect(),
                    ),
                    VariantKind::Struct(fields) => VariantTypeInfo::Struct(
                        fields
                            .iter()
                            .map(|f| (f.name.clone(), self.lower_type_expr(&f.ty, &gp)))
                            .collect(),
                    ),
                };
                (v.name.clone(), vtype)
            })
            .collect();
        let derived_traits = extract_derived_traits(&e.attributes);
        if has_display_snake_case(&e.attributes) {
            self.display_snake_case_enums.insert(e.name.clone());
        }
        self.env.enums.insert(
            e.name.clone(),
            EnumInfo {
                generic_params: gp,
                variants,
                derived_traits,
                is_shared: e.is_shared,
            },
        );
    }

    fn env_add_function(&mut self, f: &Function) {
        let gp = Self::generic_param_names(&f.generic_params);
        let param_names: Vec<Option<String>> = f
            .params
            .iter()
            .map(|p| p.name().map(|s| s.to_string()))
            .collect();
        let params: Vec<Type> = f
            .params
            .iter()
            .map(|p| self.lower_type_expr(&p.ty, &gp))
            .collect();
        let return_type = f
            .return_type
            .as_ref()
            .map(|t| self.lower_type_expr(t, &gp))
            .unwrap_or(Type::Unit);
        self.env.functions.insert(
            f.name.clone(),
            FunctionSig {
                generic_params: gp,
                param_names,
                params,
                return_type,
                // Normalize inline param bounds into the where-clause so the
                // call-site discharge engine sees both forms uniformly.
                // Slice 0.a, sub-step 1 of monomorphized collections prereq.
                where_clause: normalize_bounds_into_where_clause(
                    &f.generic_params,
                    &f.where_clause,
                ),
            },
        );
        if f.attributes.iter().any(|a| a.name == "compiler_builtin") {
            self.env.compiler_builtins.insert(f.name.clone());
        }
    }

    fn env_add_impl(&mut self, imp: &ImplBlock) {
        // Impl-level generic params are in scope when lowering method
        // signatures so `T` in `impl[T] Box[T] { fn echo(v: T) -> T }` is
        // recognized as a `Type::TypeParam("T")` rather than a `Type::Named
        // { "T", [] }` fallback. The method's own generic params extend the
        // scope; the per-method `generic_params` field on `FunctionSig`
        // continues to record only the method's own names so generic-call
        // inference at the use site doesn't accidentally rebind the
        // impl-level params.
        let impl_gp_names: Vec<String> = imp
            .generic_params
            .as_ref()
            .map(|gp| gp.params.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default();

        // Slice 1b: `impl Foo { ... }` and `impl Trait for Foo` are both
        // rejected when `Foo` is an opaque foreign type — opaque types have
        // no Kāra-visible methods or trait implementations. We check this
        // before `lower_type_expr` is called on the target so the user
        // doesn't see a duplicate `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`
        // diagnostic on the same span. Bail before `add_impl` so the impl
        // doesn't register and produce downstream confusion.
        if let TypeKind::Path(path) = &imp.target_type.kind {
            if path.segments.len() == 1 {
                let target_name = &path.segments[0];
                if self.env.opaque_foreign_types.contains(target_name) {
                    let prefix = match &imp.trait_name {
                        Some(tp) => format!(
                            "`impl {} for {}`",
                            tp.segments.last().cloned().unwrap_or_default(),
                            target_name
                        ),
                        None => format!("`impl {}`", target_name),
                    };
                    self.type_error(
                        format!(
                            "error[E_OPAQUE_TYPE_NO_INHERENT_OR_TRAIT_IMPLS]: cannot \
                             write {prefix} — opaque foreign type '{target_name}' has \
                             no Kāra-visible methods or trait implementations. Wrap \
                             '{target_name}' in a Kāra-side newtype (e.g. `shared \
                             struct {target_name}Handle {{ p: ref {target_name} }}`) \
                             and impl on the wrapper instead"
                        ),
                        imp.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    return;
                }
            }
        }

        // Lower the target type-expression through the standard pipeline so
        // type aliases canonicalize at registration time (`type MyOpt =
        // Option[Ordering]; impl Foo for MyOpt` resolves to target_type
        // "Option" + target_args [Ordering] before insertion). Theme-4
        // slice — see `phase-4-interpreter.md` § `impl Option[Ordering]`.
        let lowered_target = self.lower_type_expr(&imp.target_type, &impl_gp_names);
        let (type_name, target_args) = match &lowered_target {
            Type::Named { name, args } => {
                // Specialized impls store the concrete arg vector;
                // generic-on-name impls (anything containing a TypeParam
                // recursively) collapse to empty target_args so the
                // args-match rule treats them as wildcard-match.
                let concrete = !args.is_empty() && args.iter().all(type_is_fully_concrete);
                if concrete {
                    (name.clone(), args.clone())
                } else {
                    (name.clone(), Vec::new())
                }
            }
            // Shared structs: `impl S { ... }` for a `shared struct S`
            // registers under the bare name (no target_args — shared
            // structs are non-generic at v1 per design.md § Part 5).
            // Sub-item 2 audit miss caught during sub-item 3a.
            Type::Shared(name) => (name.clone(), Vec::new()),
            // Non-path target types (`impl Foo for (i32, i32)` etc.) are
            // unsupported in v1; bail without registering. Matches the
            // pre-Theme-4 behavior of the path-only short-circuit.
            _ => return,
        };

        let trait_name = imp
            .trait_name
            .as_ref()
            .and_then(|p| p.segments.last().cloned());

        let mut methods = HashMap::new();
        for item in &imp.items {
            let method = match item {
                ImplItem::Method(m) => m,
                ImplItem::AssocType(_) => continue,
            };
            let method_gp = Self::generic_param_names(&method.generic_params);
            let mut lowering_scope = impl_gp_names.clone();
            lowering_scope.extend(method_gp.iter().cloned());
            let param_names: Vec<Option<String>> = method
                .params
                .iter()
                .map(|p: &Param| p.name().map(|s| s.to_string()))
                .collect();
            let params: Vec<Type> = method
                .params
                .iter()
                .map(|p| self.lower_type_expr(&p.ty, &lowering_scope))
                .collect();
            let return_type = method
                .return_type
                .as_ref()
                .map(|t| self.lower_type_expr(t, &lowering_scope))
                .unwrap_or(Type::Unit);
            methods.insert(
                method.name.clone(),
                FunctionSig {
                    generic_params: method_gp,
                    param_names,
                    params,
                    return_type,
                    // Method-level inline bounds normalize alongside the
                    // method's own where-clause; impl-level inline bounds
                    // are tracked separately on `ImplInfo.generic_params`
                    // and discharged by `impl_bounds_discharge` at the
                    // dispatch site — NOT folded in here.
                    where_clause: normalize_bounds_into_where_clause(
                        &method.generic_params,
                        &method.where_clause,
                    ),
                },
            );
        }

        // Theme-4 overlap check: reject coexistence of generic-on-name and
        // specialized impls for the same `(trait_name, target_type)` pair,
        // and reject duplicate specialized impls on the same concrete args.
        // Generic-vs-generic and same-args-duplicate cases are pre-existing
        // trait-coherence concerns left unchanged. See
        // `phase-4-interpreter.md` § `impl Option[Ordering]` for the
        // locked design rationale (rejection over Rust-style
        // specialization).
        if self.impl_overlap_exists(&trait_name, &type_name, &target_args) {
            self.type_error(
                format!(
                    "conflicting impl: another `impl{} {}{}` already exists; v1 \
                     does not support generic-vs-specialized impl overlap on the \
                     same trait + target",
                    trait_name
                        .as_deref()
                        .map(|t| format!(" {} for", t))
                        .unwrap_or_default(),
                    type_name,
                    if target_args.is_empty() {
                        String::new()
                    } else {
                        let rendered = target_args
                            .iter()
                            .map(type_display)
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("[{}]", rendered)
                    },
                ),
                imp.span.clone(),
                TypeErrorKind::ConflictingImpl,
            );
            return;
        }

        self.env.add_impl(ImplInfo {
            target_type: type_name,
            target_args,
            trait_name,
            methods,
            generic_params: imp.generic_params.clone(),
            where_clause: imp.where_clause.clone(),
        });
    }

    /// Theme-4 overlap detection. Returns `true` iff registering an impl
    /// with `(trait_name, target_type, target_args)` would conflict with
    /// an already-registered impl on the same `(trait_name, target_type)`
    /// pair under the v1 rule: generic-on-name (`target_args.is_empty()`)
    /// cannot coexist with any specialized variant, and two specialized
    /// variants cannot vector-equal on `target_args`. Anything else
    /// (different concrete instantiations, different traits) is fine.
    fn impl_overlap_exists(
        &self,
        trait_name: &Option<String>,
        target_type: &str,
        target_args: &[Type],
    ) -> bool {
        for existing in &self.env.impls {
            if existing.trait_name != *trait_name || existing.target_type != target_type {
                continue;
            }
            let existing_empty = existing.target_args.is_empty();
            let new_empty = target_args.is_empty();
            if existing_empty != new_empty {
                // generic-on-name + specialized — overlap
                return true;
            }
            if !existing_empty && existing.target_args == target_args {
                // two specialized impls on the same concrete instantiation
                return true;
            }
        }
        false
    }

    /// Register a built-in stdlib impl programmatically (no AST source).
    /// Used by `register_stdlib_impls` to seed primitive operator impls.
    /// Compiler-internal stdlib impls are unconditional and registered
    /// with empty `target_args` (generic-on-name) so primitive operator
    /// dispatch (`1 + 2` etc.) continues to apply uniformly.
    #[allow(dead_code)]
    fn register_builtin_impl(
        &mut self,
        trait_name: &str,
        target_type: &str,
        methods: Vec<(&str, FunctionSig)>,
    ) {
        let methods = methods
            .into_iter()
            .map(|(n, sig)| (n.to_string(), sig))
            .collect();
        self.env.add_impl(ImplInfo {
            target_type: target_type.to_string(),
            target_args: Vec::new(),
            trait_name: Some(trait_name.to_string()),
            methods,
            // Compiler-internal stdlib impls are unconditional —
            // primitive operator dispatch isn't generic over a bound.
            generic_params: None,
            where_clause: None,
        });
    }

    fn env_add_const(&mut self, c: &ConstDecl) {
        let ty = self.lower_type_expr(&c.ty, &[]);
        self.env.constants.insert(c.name.clone(), ty);
    }

    fn env_add_trait(&mut self, t: &TraitDef) {
        let assoc_types: Vec<String> = t
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::AssocType(decl) => Some(decl.name.clone()),
                _ => None,
            })
            .collect();
        let supertraits: Vec<String> = t
            .supertraits
            .iter()
            .map(|b| b.path.last().cloned().unwrap_or_default())
            .collect();
        self.env.traits.insert(
            t.name.clone(),
            TraitInfo {
                assoc_types,
                supertraits,
            },
        );
    }

    fn env_add_trait_alias(&mut self, t: &TraitAliasDef) {
        // v1 stub registration — record the name so use sites can emit
        // `E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET`. Bound substitution + the
        // matching `TraitInfo` shape land in P1.
        self.env.trait_aliases.insert(t.name.clone());
    }

    fn env_add_marker_trait(&mut self, t: &MarkerTraitDef) {
        // Register in `traits` so bound resolution treats the marker
        // identically to an ordinary trait. The companion entry in
        // `marker_traits` records the marker-ness for impl-body checks.
        let supertraits: Vec<String> = t
            .supertraits
            .iter()
            .map(|b| b.path.last().cloned().unwrap_or_default())
            .collect();
        self.env.traits.insert(
            t.name.clone(),
            TraitInfo {
                assoc_types: Vec::new(),
                supertraits,
            },
        );
        self.env.marker_traits.insert(t.name.clone());
    }

    fn env_add_type_alias(&mut self, t: &TypeAliasDef) {
        let gp = Self::generic_param_names(&t.generic_params);
        let ty = self.lower_type_expr(&t.ty, &gp);
        self.env.type_aliases.insert(t.name.clone(), ty);
    }

    fn env_add_distinct_type(&mut self, d: &crate::ast::DistinctTypeDef) {
        let derived = extract_derived_traits(&d.attributes);
        self.env.distinct_types.insert(d.name.clone(), derived);
    }

    fn env_add_opaque_foreign_type(&mut self, o: &crate::ast::OpaqueTypeDecl) {
        // Register the name so downstream type-resolution sees the
        // identifier as a known type (not an unknown-symbol error). Slice
        // 1b's use-site precision diagnostics (`E_OPAQUE_TYPE_REQUIRES_INDIRECTION`,
        // `E_OPAQUE_TYPE_NO_FIELDS`, `E_OPAQUE_TYPE_NO_INHERENT_OR_TRAIT_IMPLS`)
        // consult this set at `lower_type_expr_inner`, `infer_field_access`,
        // and `env_add_impl` respectively. The fourth code from
        // `design.md § Opaque Foreign Types` —
        // `E_OPAQUE_TYPE_NO_KNOWN_SIZE` (`size_of[Foo]()` /
        // `align_of[Foo]()`) — lands with the `offset_of[T](field)`
        // intrinsic family per `design.md § Field Offsets`; the
        // intrinsic surface does not exist in user code today.
        self.env.opaque_foreign_types.insert(o.name.clone());
    }

    fn env_add_extern_function(&mut self, e: &ExternFunction) {
        let param_names: Vec<Option<String>> = e
            .params
            .iter()
            .map(|p| p.name().map(|s| s.to_string()))
            .collect();
        let params: Vec<Type> = e
            .params
            .iter()
            .map(|p| self.lower_type_expr(&p.ty, &[]))
            .collect();
        let return_type = e
            .return_type
            .as_ref()
            .map(|t| self.lower_type_expr(t, &[]))
            .unwrap_or(Type::Unit);
        self.env.functions.insert(
            e.name.clone(),
            FunctionSig {
                generic_params: Vec::new(),
                param_names,
                params,
                return_type,
                where_clause: None,
            },
        );
    }

    // ── Check Items (Pass 2) ────────────────────────────────────

    fn check_items(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) => {
                    // `#[compiler_builtin]` declarations carry a placeholder
                    // body that is replaced by Rust dispatch at runtime
                    // (CR-202 slice 2). The signature is the contract callers
                    // are checked against; the body itself is irrelevant, so
                    // skip body-checking entirely. This lets stdlib source
                    // pair an attribute with whatever body keeps the parser
                    // happy without that body being held to type-correctness.
                    if self.env.compiler_builtins.contains(&f.name) {
                        continue;
                    }
                    self.check_function(f, None, &[]);
                }
                Item::ImplBlock(imp) => self.check_impl_block(imp),
                Item::TraitDef(t) => self.check_trait_def(t),
                Item::ConstDecl(c) => self.check_const_decl(c),
                Item::StructDef(s) => {
                    let gp = Self::generic_param_names(&s.generic_params);
                    self.validate_all_bounds(&s.generic_params, &s.where_clause, &gp);
                }
                Item::EnumDef(e) => {
                    let gp = Self::generic_param_names(&e.generic_params);
                    self.validate_all_bounds(&e.generic_params, &e.where_clause, &gp);
                }
                _ => {}
            }
        }
    }

    /// Type-check default method bodies inside a trait declaration.
    /// `Self` is treated as an abstract type parameter (`Type::TypeParam("Self")`)
    /// so signature and body references to `Self`/`self` resolve consistently.
    fn check_trait_def(&mut self, t: &TraitDef) {
        let mut enclosing = vec!["Self".to_string()];
        if let Some(ref generics) = t.generic_params {
            for p in &generics.params {
                enclosing.push(p.name.clone());
            }
        }

        // Validate inline bounds and where clause on the trait itself
        self.validate_all_bounds(&t.generic_params, &t.where_clause, &enclosing);

        // Save outer bounds. Trait-level generics' bounds + supertraits-as-Self
        // are visible to default method bodies. Restored after the trait's
        // items are checked.
        let saved_bounds = self.enclosing_bounds.clone();
        for (name, bounds) in Self::collect_param_bounds(&t.generic_params, &t.where_clause) {
            self.enclosing_bounds.insert(name, bounds);
        }
        if !t.supertraits.is_empty() {
            self.enclosing_bounds
                .entry("Self".to_string())
                .or_default()
                .extend(t.supertraits.iter().cloned());
        }

        // Slice 3.5 of the method-resolution CR: track the enclosing trait so
        // `self.method()` in a default body dispatches through the trait's
        // own methods + supertrait closure rather than silently falling
        // through.
        let saved_enclosing_trait = self.enclosing_trait.take();
        self.enclosing_trait = Some(t.name.clone());

        let self_type = Type::TypeParam("Self".to_string());
        for item in &t.items {
            if let TraitItem::Method(method) = item {
                if let Some(ref body) = method.body {
                    let synthesized = Function {
                        span: method.span.clone(),
                        attributes: Vec::new(),
                        doc_comment: None,
                        is_pub: false,
                        is_private: false,
                        is_unsafe: false,
                        name: method.name.clone(),
                        generic_params: method.generic_params.clone(),
                        params: method.params.clone(),
                        self_param: method.self_param.clone(),
                        return_type: method.return_type.clone(),
                        effects: method.effects.clone(),
                        requires: method.requires.clone(),
                        ensures: method.ensures.clone(),
                        where_clause: method.where_clause.clone(),
                        body: body.clone(),
                        stdlib_origin: t.stdlib_origin,
                    };
                    self.check_function(&synthesized, Some(&self_type), &enclosing);
                }
            }
        }

        self.enclosing_bounds = saved_bounds;
        self.enclosing_trait = saved_enclosing_trait;
    }

    /// Build a map of user-defined type names → `is_pub`. Types absent from the
    /// map are treated as public (builtins, primitives, stdlib-registered types
    /// like `Option` / `Result` / `F32` live outside the user AST).
    ///
    /// CR-24 slice 6b: imported types are folded in under their local name
    /// (alias-aware) with the *origin* module's visibility. An imported type
    /// whose origin is `Default` or `Private` behaves identically to a
    /// locally-declared non-`pub` type when it appears in a `pub` signature
    /// — the type is not part of the current package's public API, so
    /// leaking it through one trips `E0221 PrivateTypeInPublicSignature`.
    fn collect_type_visibility(&self) -> HashMap<String, bool> {
        let mut map: HashMap<String, bool> = HashMap::new();
        for item in &self.program.items {
            match item {
                Item::StructDef(s) => {
                    map.insert(s.name.clone(), s.is_pub);
                }
                Item::EnumDef(e) => {
                    map.insert(e.name.clone(), e.is_pub);
                }
                Item::TraitDef(t) => {
                    map.insert(t.name.clone(), t.is_pub);
                }
                Item::TypeAlias(t) => {
                    map.insert(t.name.clone(), t.is_pub);
                }
                Item::DistinctType(d) => {
                    map.insert(d.name.clone(), d.is_pub);
                }
                _ => {}
            }
        }
        for (name, (_origin_path, _origin_name, vis)) in &self.type_origins {
            // Only overwrite when we don't already have a local entry for
            // this name; a local declaration shadows an import for purposes
            // of the signature check.
            map.entry(name.clone()).or_insert_with(|| vis.is_pub());
        }
        map
    }

    /// Walk a `TypeExpr` and emit `PrivateTypeInPublicSignature` for every
    /// reference to a non-`pub` user-defined type. `generic_scope` suppresses
    /// single-segment paths that name an in-scope generic parameter (e.g. `T`
    /// in `fn foo[T](x: T)`).
    ///
    /// Note on scope: the check fires on name-visible leaks only. Cross-module
    /// private-field access (`user.password_hash` from outside the defining
    /// module) is part of CR-18 but gated on the module system (CR-24) — with
    /// a single-module compilation unit, every access is "same project" per
    /// design.md § Three-level visibility, so the field rule has no firing
    /// sites today.
    fn check_type_expr_visibility(
        &mut self,
        ty: &TypeExpr,
        generic_scope: &[String],
        type_vis: &HashMap<String, bool>,
        context: &str,
        owner: &str,
    ) {
        match &ty.kind {
            TypeKind::Path(p) => {
                if let Some(ref args) = p.generic_args {
                    for a in args {
                        if let GenericArg::Type(t) = a {
                            self.check_type_expr_visibility(
                                t,
                                generic_scope,
                                type_vis,
                                context,
                                owner,
                            );
                        }
                    }
                }
                let last = match p.segments.last() {
                    Some(s) => s.clone(),
                    None => return,
                };
                if p.segments.len() == 1 && generic_scope.iter().any(|g| g == &last) {
                    return;
                }
                if let Some(false) = type_vis.get(&last).copied() {
                    self.type_error(
                        format!(
                            "private type '{}' leaks through {} of '{}'; mark the type `pub` or remove it from the public surface",
                            last, context, owner
                        ),
                        ty.span.clone(),
                        TypeErrorKind::PrivateTypeInPublicSignature,
                    );
                }
            }
            TypeKind::Tuple(ts) => {
                for t in ts {
                    self.check_type_expr_visibility(t, generic_scope, type_vis, context, owner);
                }
            }
            TypeKind::Array { element, .. } => {
                self.check_type_expr_visibility(element, generic_scope, type_vis, context, owner);
            }
            TypeKind::Pointer { inner, .. }
            | TypeKind::Ref(inner)
            | TypeKind::MutRef(inner)
            | TypeKind::MutSlice(inner)
            | TypeKind::Weak(inner) => {
                self.check_type_expr_visibility(inner, generic_scope, type_vis, context, owner);
            }
            TypeKind::FnType {
                params,
                return_type,
                ..
            } => {
                for p in params {
                    self.check_type_expr_visibility(p, generic_scope, type_vis, context, owner);
                }
                if let Some(ref rt) = return_type {
                    self.check_type_expr_visibility(rt, generic_scope, type_vis, context, owner);
                }
            }
            TypeKind::Unit | TypeKind::Error => {}
        }
    }

    /// Flag non-`pub` types appearing in `pub` signature positions across
    /// functions, methods, extern functions, struct fields, enum variant
    /// payloads, type aliases, and constants. See CR-18.
    fn check_signature_visibility(&mut self) {
        let type_vis = self.collect_type_visibility();
        let items = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) if f.is_pub => {
                    let scope = Self::generic_param_names(&f.generic_params);
                    for p in &f.params {
                        self.check_type_expr_visibility(
                            &p.ty,
                            &scope,
                            &type_vis,
                            "parameter",
                            &f.name,
                        );
                    }
                    if let Some(ref rt) = f.return_type {
                        self.check_type_expr_visibility(
                            rt,
                            &scope,
                            &type_vis,
                            "return type",
                            &f.name,
                        );
                    }
                }
                Item::ExternFunction(e) if e.is_pub => {
                    for p in &e.params {
                        self.check_type_expr_visibility(
                            &p.ty,
                            &[],
                            &type_vis,
                            "extern parameter",
                            &e.name,
                        );
                    }
                    if let Some(ref rt) = e.return_type {
                        self.check_type_expr_visibility(
                            rt,
                            &[],
                            &type_vis,
                            "extern return type",
                            &e.name,
                        );
                    }
                }
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(e) if e.is_pub => {
                                for p in &e.params {
                                    self.check_type_expr_visibility(
                                        &p.ty,
                                        &[],
                                        &type_vis,
                                        "extern parameter",
                                        &e.name,
                                    );
                                }
                                if let Some(ref rt) = e.return_type {
                                    self.check_type_expr_visibility(
                                        rt,
                                        &[],
                                        &type_vis,
                                        "extern return type",
                                        &e.name,
                                    );
                                }
                            }
                            ExternItem::Function(_) => {}
                            // Opaque foreign type declarations have no
                            // type-expression surface to visibility-check
                            // — the declaration *is* the type.
                            ExternItem::OpaqueType(_) => {}
                        }
                    }
                }
                Item::StructDef(s) if s.is_pub => {
                    let scope = Self::generic_param_names(&s.generic_params);
                    for f in &s.fields {
                        if f.is_pub {
                            let owner = format!("{}.{}", s.name, f.name);
                            self.check_type_expr_visibility(
                                &f.ty,
                                &scope,
                                &type_vis,
                                "struct field",
                                &owner,
                            );
                        }
                    }
                }
                Item::EnumDef(e) if e.is_pub => {
                    let scope = Self::generic_param_names(&e.generic_params);
                    for v in &e.variants {
                        match &v.kind {
                            VariantKind::Unit => {}
                            VariantKind::Tuple(ts) => {
                                let owner = format!("{}.{}", e.name, v.name);
                                for t in ts {
                                    self.check_type_expr_visibility(
                                        t,
                                        &scope,
                                        &type_vis,
                                        "enum variant payload",
                                        &owner,
                                    );
                                }
                            }
                            VariantKind::Struct(fs) => {
                                for f in fs {
                                    let owner = format!("{}.{}.{}", e.name, v.name, f.name);
                                    self.check_type_expr_visibility(
                                        &f.ty,
                                        &scope,
                                        &type_vis,
                                        "enum variant field",
                                        &owner,
                                    );
                                }
                            }
                        }
                    }
                }
                Item::TypeAlias(t) if t.is_pub => {
                    let scope = Self::generic_param_names(&t.generic_params);
                    self.check_type_expr_visibility(
                        &t.ty,
                        &scope,
                        &type_vis,
                        "type alias",
                        &t.name,
                    );
                }
                Item::DistinctType(d) if d.is_pub => {
                    let scope = Self::generic_param_names(&d.generic_params);
                    self.check_type_expr_visibility(
                        &d.base_type,
                        &scope,
                        &type_vis,
                        "distinct type base",
                        &d.name,
                    );
                }
                Item::ConstDecl(c) if c.is_pub => {
                    self.check_type_expr_visibility(&c.ty, &[], &type_vis, "constant", &c.name);
                }
                Item::ImplBlock(imp) => {
                    let impl_scope = Self::generic_param_names(&imp.generic_params);
                    for ii in &imp.items {
                        if let ImplItem::Method(m) = ii {
                            if m.is_pub {
                                let mut scope = impl_scope.clone();
                                scope.extend(Self::generic_param_names(&m.generic_params));
                                for p in &m.params {
                                    self.check_type_expr_visibility(
                                        &p.ty,
                                        &scope,
                                        &type_vis,
                                        "method parameter",
                                        &m.name,
                                    );
                                }
                                if let Some(ref rt) = m.return_type {
                                    self.check_type_expr_visibility(
                                        rt,
                                        &scope,
                                        &type_vis,
                                        "method return type",
                                        &m.name,
                                    );
                                }
                            }
                        }
                    }
                }
                Item::TraitDef(t) if t.is_pub => {
                    let trait_scope = Self::generic_param_names(&t.generic_params);
                    for ti in &t.items {
                        if let TraitItem::Method(m) = ti {
                            let mut scope = trait_scope.clone();
                            scope.extend(Self::generic_param_names(&m.generic_params));
                            for p in &m.params {
                                self.check_type_expr_visibility(
                                    &p.ty,
                                    &scope,
                                    &type_vis,
                                    "trait method parameter",
                                    &m.name,
                                );
                            }
                            if let Some(ref rt) = m.return_type {
                                self.check_type_expr_visibility(
                                    rt,
                                    &scope,
                                    &type_vis,
                                    "trait method return type",
                                    &m.name,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Validate where clause constraints: type params exist, trait names are known.
    /// Returns true when `trait_name` is a trait the typechecker recognises:
    /// registered stdlib traits, derive-only builtins, and user-defined traits
    /// in the current program.
    fn is_known_trait(&self, trait_name: &str) -> bool {
        const DERIVE_ONLY_BUILTINS: &[&str] = &[
            "Hash",
            "Clone",
            "Copy",
            "PartialEq",
            "PartialOrd",
            "Debug",
            "Default",
            "Iterator",
        ];
        self.env.traits.contains_key(trait_name)
            || self.env.trait_aliases.contains(trait_name)
            || DERIVE_ONLY_BUILTINS.contains(&trait_name)
            || self.program.items.iter().any(|item| match item {
                Item::TraitDef(t) => t.name == trait_name,
                Item::TraitAlias(t) => t.name == trait_name,
                _ => false,
            })
    }

    /// True iff `trait_name` was declared as `trait NAME = bound1 + ...;`
    /// rather than a regular trait. v1 stubs use this to emit
    /// `E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET` at every use site (bound /
    /// where-clause / dyn). Bound substitution lands in P1.
    fn is_trait_alias(&self, trait_name: &str) -> bool {
        self.env.trait_aliases.contains(trait_name)
            || self
                .program
                .items
                .iter()
                .any(|item| matches!(item, Item::TraitAlias(t) if t.name == trait_name))
    }

    /// Bound list of a declared trait alias for inclusion in the v1 stub
    /// diagnostic — copy-pasting the bound list back lets the user apply
    /// the workaround directly. Returns `None` when the name is not an
    /// alias or its declaration is not in the current program.
    fn trait_alias_bound_list(&self, trait_name: &str) -> Option<String> {
        for item in &self.program.items {
            if let Item::TraitAlias(alias) = item {
                if alias.name == trait_name {
                    let parts: Vec<String> =
                        alias.bounds.iter().map(|b| b.path.join(".")).collect();
                    return Some(parts.join(" + "));
                }
            }
        }
        None
    }

    /// Emit the v1 trait-alias stub diagnostic at a use site.
    fn report_trait_alias_use(&mut self, trait_name: &str, span: &Span) {
        let bound_list = self
            .trait_alias_bound_list(trait_name)
            .unwrap_or_else(|| "<bounds>".to_string());
        self.type_error(
            format!(
                "error[E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET]: trait alias \
                 '{trait_name}' is recognized but not yet expanded; the \
                 implementation lands in P1 — write the bound list \
                 explicitly for now: `{bound_list}`"
            ),
            span.clone(),
            TypeErrorKind::TypeMismatch,
        );
    }

    /// Validate inline bounds on generic parameters (e.g. `fn sort[T: Ord]`).
    /// Emits an error when a bound names an unknown trait.
    fn validate_inline_generic_bounds(&mut self, generics: &Option<GenericParams>) {
        let Some(ref gp) = generics else { return };
        let params: Vec<_> = gp.params.clone();
        for param in &params {
            for bound in &param.bounds {
                let trait_name = bound.path.last().cloned().unwrap_or_default();
                if self.is_trait_alias(&trait_name) {
                    self.report_trait_alias_use(&trait_name, &bound.span);
                } else if !self.is_known_trait(&trait_name) {
                    self.type_error(
                        format!(
                            "unknown trait '{}' in inline bound on type parameter '{}'",
                            trait_name, param.name
                        ),
                        bound.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
        }
    }

    /// Verify each `const N: T` generic parameter's declared type `T` is in
    /// the spec-allowed set (see `design.md § Type Inference > Const generic
    /// parameters`): integers `i8`/`i16`/`i32`/`i64`, `bool`, `char`, or a
    /// fieldless enum. Rejected: `usize` and other unsigned widths, float
    /// widths, `String`, fielded enums, refinement types, distinct types.
    fn validate_const_param_types(
        &mut self,
        generics: &Option<GenericParams>,
        generic_scope: &[String],
    ) {
        let Some(ref gp) = generics else { return };
        let params: Vec<_> = gp.params.clone();
        for param in &params {
            if !param.is_const {
                continue;
            }
            let Some(ref ty_expr) = param.const_type else {
                continue;
            };
            let lowered = self.lower_type_expr(ty_expr, generic_scope);
            if !self.is_permitted_const_param_type(&lowered) {
                self.type_error(
                    format!(
                        "type '{}' is not permitted as a const generic parameter type; \
                         allowed types are i8, i16, i32, i64, bool, char, and fieldless enums \
                         (see design.md § Type Inference > Const generic parameters)",
                        type_display(&lowered)
                    ),
                    ty_expr.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
            }
        }
    }

    fn is_permitted_const_param_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_) => true,
            Type::Bool => true,
            Type::Char => true,
            Type::Named { name, args } if args.is_empty() => {
                // Fieldless enum: every variant is `Unit`.
                self.env
                    .enums
                    .get(name)
                    .map(|info| {
                        info.variants
                            .iter()
                            .all(|(_, kind)| matches!(kind, VariantTypeInfo::Unit))
                    })
                    .unwrap_or(false)
            }
            _ => false,
        }
    }

    fn validate_where_clause(&mut self, where_clause: &WhereClause, generic_scope: &[String]) {
        for constraint in &where_clause.constraints {
            match constraint {
                WhereConstraint::TypeBound {
                    type_name,
                    bounds,
                    span,
                } => {
                    // Verify the type parameter exists in generic scope
                    if !generic_scope.contains(type_name) {
                        self.type_error(
                            format!(
                                "where clause references unknown type parameter '{}'",
                                type_name
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                    // Verify each bound trait is a known trait or built-in
                    for bound in bounds {
                        let trait_name = bound.path.last().cloned().unwrap_or_default();
                        if self.is_trait_alias(&trait_name) {
                            self.report_trait_alias_use(&trait_name, &bound.span);
                        } else if !self.is_known_trait(&trait_name) {
                            self.type_error(
                                format!("unknown trait '{}' in where clause", trait_name),
                                bound.span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                        }
                    }
                }
                WhereConstraint::AssocTypeEq {
                    type_name,
                    span,
                    ty,
                    ..
                } => {
                    // Verify the type parameter exists
                    if !generic_scope.contains(type_name) {
                        self.type_error(
                            format!(
                                "where clause references unknown type parameter '{}'",
                                type_name
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                    // Resolve the associated type expression
                    self.lower_type_expr(ty, generic_scope);
                }
                WhereConstraint::ConstPredicate { .. } => {
                    // TODO(const generics slice 3): bound-discharge engine.
                    // Slice 2 builds `eval_const_expr` (above); slice 3
                    // wires it into per-call-site predicate evaluation
                    // here, emitting `const constraint violated` with the
                    // concrete const-arg values when the predicate is
                    // `false`. Slice 2 intentionally does not evaluate the
                    // predicate at the declaration site — evaluation
                    // requires the const-args bound in scope, which only
                    // exist at call sites.
                }
            }
        }
    }

    /// Validate both inline bounds and a where clause together — the merged
    /// bound set for a single declaration. Both inline and where-clause bounds
    /// apply simultaneously; they may coexist on the same type parameter.
    fn validate_all_bounds(
        &mut self,
        generics: &Option<GenericParams>,
        where_clause: &Option<WhereClause>,
        generic_scope: &[String],
    ) {
        self.validate_inline_generic_bounds(generics);
        self.validate_const_param_types(generics, generic_scope);
        if let Some(ref wc) = where_clause {
            self.validate_where_clause(wc, generic_scope);
        }
    }

    /// Verify `expr` is a valid constant for a default-parameter
    /// position. Tuple and array literals recurse element-wise; other
    /// shapes route through `eval_const_expr`. The composite recursion
    /// uses an i64 placeholder for sub-element target types — sub-element
    /// arithmetic overflow still surfaces (against i64's range) even
    /// when the actual surface type is narrower.
    fn validate_default_value_is_const(&mut self, expr: &Expr, target_ty: &Type) {
        match &expr.kind {
            ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
                for e in elems {
                    self.validate_default_value_is_const(e, &Type::Int(IntSize::I64));
                }
            }
            _ => {
                if let Err(err) = self.eval_const_expr(expr, target_ty) {
                    match err {
                        ConstEvalError::NonConstShape(span) => self.type_error(
                            "default parameter value must be a constant expression \
                             (no function calls, closures, or runtime-only values)"
                                .to_string(),
                            span,
                            TypeErrorKind::TypeMismatch,
                        ),
                        other => self.emit_const_eval_error(other),
                    }
                }
            }
        }
    }

    /// Validate default parameter values: trailing-only, type-compatible.
    fn validate_default_params(&mut self, params: &[Param], generic_scope: &[String]) {
        // Collect all sibling parameter names for the "no cross-param reference" check.
        let sibling_names: Vec<String> = params
            .iter()
            .flat_map(|p| p.pattern.binding_names())
            .collect();

        let mut seen_default = false;
        for param in params {
            if let Some(ref default_expr) = param.default_value {
                seen_default = true;
                // Type-check the default value against the parameter type
                let param_ty = self.lower_type_expr(&param.ty, generic_scope);
                let default_ty = self.infer_expr(default_expr);
                self.check_assignable(&param_ty, &default_ty, default_expr.span.clone());
                // Verify the default is a constant expression. Route
                // through the const-expression evaluator (slice 2) for
                // leaf expressions; recurse on tuple / array-literal
                // shapes (which the evaluator rejects as `NonConstShape`
                // because it has no single-`ConstValue` representation
                // for composites — but default-param validation is a
                // shape check, not a value resolution).
                self.validate_default_value_is_const(default_expr, &param_ty);
                // Verify the default does not reference sibling parameters
                let own_names: Vec<String> = param.pattern.binding_names();
                for sibling in &sibling_names {
                    if !own_names.contains(sibling)
                        && Self::expr_references_name(default_expr, sibling)
                    {
                        self.type_error(
                            format!(
                                "default parameter value must not reference \
                                 another parameter ('{}')",
                                sibling
                            ),
                            default_expr.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
            } else if seen_default {
                // Non-defaulted param after a defaulted one
                self.type_error(
                    "non-defaulted parameter cannot follow a defaulted parameter".to_string(),
                    param.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
            }
        }
    }

    /// Const-expression evaluator (const generics slice 2). Walks `expr`
    /// against a target `Type`, returning either a resolved `ConstValue`
    /// or a `ConstEvalError`.
    ///
    /// Operand-type propagation: arithmetic / bitwise / shift ops propagate
    /// `target_ty` to both operands (recursing with the same target ensures
    /// `2 + 3` against `target_ty=i8` walks both literals as i8). Comparison
    /// and logical ops infer their operand target type from the operand
    /// expressions themselves (a comparison's result is `Bool`; the operand
    /// type comes from their literal suffixes / `ConstDecl` types). For
    /// comparisons, both sides must produce `ConstValue` variants from the
    /// same comparable family (int/int, bool/bool, char/char,
    /// enum-variant/enum-variant from the same enum).
    ///
    /// Identifier resolution at slice 2: tries `ConstDecl` lookup in
    /// `program.items`. Const-generic parameters via slice 1's `SubstValue`
    /// substitution context are not threaded here yet (slice 3 wires the
    /// inference solver to pass `SubstValue` through to the evaluator).
    pub(crate) fn eval_const_expr(
        &mut self,
        expr: &Expr,
        target_ty: &Type,
    ) -> Result<crate::prelude::ConstValue, ConstEvalError> {
        self.eval_const_expr_with_chain(expr, target_ty, &mut Vec::new())
    }

    fn eval_const_expr_with_chain(
        &mut self,
        expr: &Expr,
        target_ty: &Type,
        chain: &mut Vec<String>,
    ) -> Result<crate::prelude::ConstValue, ConstEvalError> {
        use crate::prelude::ConstValue;
        match &expr.kind {
            ExprKind::Integer(n, sfx) => {
                let ty = match sfx {
                    Some(IntSuffix::I8) => Type::Int(IntSize::I8),
                    Some(IntSuffix::I16) => Type::Int(IntSize::I16),
                    Some(IntSuffix::I32) => Type::Int(IntSize::I32),
                    Some(IntSuffix::I64) => Type::Int(IntSize::I64),
                    Some(IntSuffix::U8) => Type::UInt(UIntSize::U8),
                    Some(IntSuffix::U16) => Type::UInt(UIntSize::U16),
                    Some(IntSuffix::U32) => Type::UInt(UIntSize::U32),
                    Some(IntSuffix::U64) => Type::UInt(UIntSize::U64),
                    Some(IntSuffix::I128) => Type::Int(IntSize::I128),
                    Some(IntSuffix::U128) => Type::UInt(UIntSize::U128),
                    None => {
                        if matches!(target_ty, Type::Int(_) | Type::UInt(_)) {
                            target_ty.clone()
                        } else {
                            Type::Int(IntSize::I64)
                        }
                    }
                };
                integer_to_const_value(*n, &ty, &expr.span)
            }
            ExprKind::Bool(b) => Ok(ConstValue::Bool(*b)),
            ExprKind::CharLit(c) => Ok(ConstValue::Char(*c)),
            ExprKind::Identifier(name) => {
                if chain.iter().any(|n| n == name) {
                    let mut chain_with_self = chain.clone();
                    chain_with_self.push(name.clone());
                    return Err(ConstEvalError::CyclicConstDef {
                        chain: chain_with_self,
                        span: expr.span.clone(),
                    });
                }
                for item in &self.program.items {
                    if let Item::ConstDecl(c) = item {
                        if c.name == *name {
                            // Evaluate the const's value against the
                            // surrounding context's target type rather
                            // than the const's own declared type so
                            // `const TEN: i64 = 10` used in an Array
                            // size position (target = usize) flows as
                            // Usize(10), not I64(10) — preventing a
                            // spurious cross-width mismatch in the
                            // surrounding binary op (`TEN + 1`). The
                            // const's declared-type vs use-site
                            // compatibility is enforced by the regular
                            // typechecker elsewhere; here we just
                            // produce the value at the use site's
                            // width.
                            chain.push(name.clone());
                            let res = self.eval_const_expr_with_chain(&c.value, target_ty, chain);
                            chain.pop();
                            return res;
                        }
                    }
                }
                Err(ConstEvalError::UndefinedConst {
                    name: name.clone(),
                    span: expr.span.clone(),
                })
            }
            ExprKind::Path { segments, .. } if segments.len() == 2 => {
                let enum_name = &segments[0];
                let variant_name = &segments[1];
                if let Some(info) = self.env.enums.get(enum_name) {
                    for (discriminant, (vname, vkind)) in info.variants.iter().enumerate() {
                        if vname == variant_name {
                            if !matches!(vkind, VariantTypeInfo::Unit) {
                                return Err(ConstEvalError::NonConstShape(expr.span.clone()));
                            }
                            return Ok(ConstValue::EnumVariant {
                                enum_name: enum_name.clone(),
                                variant_name: variant_name.clone(),
                                discriminant: discriminant as i64,
                            });
                        }
                    }
                }
                Err(ConstEvalError::UndefinedConst {
                    name: format!("{}.{}", enum_name, variant_name),
                    span: expr.span.clone(),
                })
            }
            ExprKind::Unary { op, operand } => {
                let val = self.eval_const_expr_with_chain(operand, target_ty, chain)?;
                apply_unary(op.clone(), val, &expr.span)
            }
            ExprKind::Binary { op, left, right } => {
                let operand_target = match op {
                    BinOp::And | BinOp::Or => Type::Bool,
                    BinOp::Eq
                    | BinOp::NotEq
                    | BinOp::Lt
                    | BinOp::LtEq
                    | BinOp::Gt
                    | BinOp::GtEq => {
                        infer_operand_target_ty(left, right).unwrap_or(Type::Int(IntSize::I64))
                    }
                    _ => target_ty.clone(),
                };
                // Short-circuit at evaluator level for And/Or so
                // `false && (1 / 0 == 1)` doesn't fire DivByZero.
                if matches!(op, BinOp::And | BinOp::Or) {
                    let lhs = self.eval_const_expr_with_chain(left, &operand_target, chain)?;
                    match (&lhs, op) {
                        (ConstValue::Bool(false), BinOp::And) => {
                            return Ok(ConstValue::Bool(false))
                        }
                        (ConstValue::Bool(true), BinOp::Or) => return Ok(ConstValue::Bool(true)),
                        (ConstValue::Bool(_), _) => {}
                        _ => {
                            return Err(ConstEvalError::LogicalOnNonBool {
                                ty: const_value_type(&lhs),
                                op: op.clone(),
                                span: left.span.clone(),
                            });
                        }
                    }
                    let rhs = self.eval_const_expr_with_chain(right, &operand_target, chain)?;
                    return apply_binary(op.clone(), lhs, rhs, &expr.span);
                }
                let lhs = self.eval_const_expr_with_chain(left, &operand_target, chain)?;
                let rhs = self.eval_const_expr_with_chain(right, &operand_target, chain)?;
                apply_binary(op.clone(), lhs, rhs, &expr.span)
            }
            _ => Err(ConstEvalError::NonConstShape(expr.span.clone())),
        }
    }

    /// Returns true if `expr` contains a bare `Identifier` node with exactly
    /// `name`. Used to detect cross-parameter references in default values.
    fn expr_references_name(expr: &Expr, name: &str) -> bool {
        match &expr.kind {
            ExprKind::Identifier(n) => n == name,
            ExprKind::Unary { operand: inner, .. } => Self::expr_references_name(inner, name),
            ExprKind::Binary { left, right, .. } => {
                Self::expr_references_name(left, name) || Self::expr_references_name(right, name)
            }
            ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
                elems.iter().any(|e| Self::expr_references_name(e, name))
            }
            _ => false,
        }
    }

    // ── Closure once-callability inference (round 12.44 — Step 2) ──

    /// Snapshot of every `name → Type` currently visible in `local_scope`,
    /// flattened across the scope stack with innermost scopes winning on
    /// collisions. Captured at the start of a closure-expression typecheck
    /// (BEFORE the closure pushes its own param scope) so the once-
    /// callability walker can identify which body identifiers refer to
    /// outer bindings without holding a reference to the live stack.
    fn flatten_local_scope_snapshot(&self) -> HashMap<String, Type> {
        let mut out: HashMap<String, Type> = HashMap::new();
        for scope in &self.local_scope.scopes {
            for (k, v) in scope {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Lightweight Copy classification using the in-progress type env.
    /// Mirrors `ownership::is_copy_type` but reads from `self.env.structs`
    /// / `self.env.enums` / `self.env.distinct_types` directly — the
    /// typechecker is mid-build, so the canonical `TypeCheckResult` does
    /// not yet exist. Used by the once-callability walker to decide
    /// whether a captured outer binding's type is Copy (no consume
    /// possible) or non-Copy (consume promotes the closure to OnceFn).
    fn is_copy_type_during_check(&self, ty: &Type) -> bool {
        if matches!(
            ty,
            Type::Int(_)
                | Type::UInt(_)
                | Type::Float(_)
                | Type::Bool
                | Type::Char
                | Type::Unit
                | Type::Never
                | Type::Error
        ) {
            return true;
        }
        match ty {
            Type::Tuple(types) => types.iter().all(|t| self.is_copy_type_during_check(t)),
            Type::Array { element, .. } => self.is_copy_type_during_check(element),
            Type::Slice { mutable, .. } => !mutable,
            Type::Named { name, args } => {
                if matches!(name.as_str(), "Option" | "Result") {
                    return args.iter().all(|a| self.is_copy_type_during_check(a));
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Copy")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Copy")
                } else if let Some(traits) = self.env.distinct_types.get(name) {
                    traits.contains("Copy")
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Decide the closure expression's type based on capture-mode prefix
    /// and a body walk for capture-consumes. Round 12.44 (Step 2) wires
    /// this in BOTH the synth path (`infer_expr`'s `Closure` arm) and the
    /// expected-type pushdown (`check_expr`'s `Closure` arm) so the type
    /// the typechecker assigns to a closure expression reflects whether
    /// it consumes a captured outer non-Copy binding.
    ///
    /// `Some(CaptureMode::Ref)` / `Some(CaptureMode::MutRef)` force
    /// `Type::Function` regardless of body — the explicit prefix is
    /// the user's promise that captures are borrowed, never moved
    /// (matches round 12.6's repeatable-closure rule). `None` /
    /// `Some(CaptureMode::Own)` (capture-by-ownership) walk the body.
    ///
    /// Round 12.45 (Step 3): when the walk produces a reason, the reason
    /// is recorded in `closure_once_reasons` keyed by the closure-expr
    /// span so the slot-rejection diagnostic can name the consumed binding.
    #[allow(clippy::too_many_arguments)]
    fn closure_type_with_capture_inference(
        &mut self,
        closure_span: &Span,
        capture_mode: Option<CaptureMode>,
        closure_param_names: &[String],
        body: &Expr,
        outer_bindings: &HashMap<String, Type>,
        param_types: Vec<Type>,
        body_ty: Type,
    ) -> Type {
        let return_type = Box::new(body_ty);
        let force_repeatable = matches!(
            capture_mode,
            Some(CaptureMode::Ref) | Some(CaptureMode::MutRef)
        );
        let reason = if force_repeatable {
            None
        } else {
            self.closure_consumes_captured_non_copy(body, closure_param_names, outer_bindings)
        };
        match reason {
            Some(r) => {
                self.closure_once_reasons
                    .insert(SpanKey::from_span(closure_span), r);
                Type::OnceFunction {
                    params: param_types,
                    return_type,
                }
            }
            None => Type::Function {
                params: param_types,
                return_type,
            },
        }
    }

    /// Returns `true` iff `body` consumes at least one captured outer
    /// non-Copy binding — the criterion that flips a closure's type from
    /// `Function` to `OnceFunction`. Mirrors the legacy ownership-side
    /// detection (`use_classifier::once_callable_closures`, populated
    /// when a `let p = closure_expr;` body walk produces a
    /// `ConsumeOrigin::ClosureCapture`-tagged consume) directly inside
    /// the typechecker, so the closure expression's inferred type stays
    /// self-consistent without a cross-phase rewrite of `expr_types`.
    ///
    /// `outer_bindings` is the `flatten_local_scope_snapshot` taken just
    /// BEFORE the closure pushes its own param scope. Closure params
    /// themselves are pushed onto the shadow stack (`closure_param_names`)
    /// so a body identifier matching a param name isn't mistaken for a
    /// capture. Body-local `let`/`for`/`match`/`if let` bindings push
    /// their own shadow scopes during the walk.
    ///
    /// The walker tracks Reading vs Consuming mode mirroring
    /// `use_classifier::walk_expr`. Owned-arg slots in `Call` (decided
    /// by the callee's signature) and the owned positions in
    /// `MethodCall` / `StructLiteral` / `Return(Some)` / `Question` /
    /// `Break(Some)` flip into Consuming. An identifier-leaf in
    /// Consuming mode whose name resolves to an outer non-Copy binding
    /// flags the closure as once-callable. The `MethodCall` receiver is
    /// walked in Reading mode (a conservative simplification — the
    /// typechecker doesn't currently track per-method `SelfParam` modes;
    /// the classifier on the ownership side does, and Step 3 closes any
    /// remaining slot-rejection gap).
    fn closure_consumes_captured_non_copy(
        &self,
        body: &Expr,
        closure_param_names: &[String],
        outer_bindings: &HashMap<String, Type>,
    ) -> Option<OnceReason> {
        let mut shadow_stack: Vec<HashSet<String>> = Vec::new();
        let mut params_set: HashSet<String> = HashSet::new();
        for n in closure_param_names {
            params_set.insert(n.clone());
        }
        shadow_stack.push(params_set);
        let mut reason: Option<OnceReason> = None;
        self.walk_capture_consume(
            body,
            CaptureWalkMode::Reading,
            outer_bindings,
            &mut shadow_stack,
            &mut reason,
        );
        reason
    }

    fn name_is_shadowed(name: &str, shadow_stack: &[HashSet<String>]) -> bool {
        shadow_stack.iter().any(|s| s.contains(name))
    }

    fn walk_capture_consume_block(
        &self,
        block: &Block,
        terminal_mode: CaptureWalkMode,
        outer: &HashMap<String, Type>,
        shadows: &mut Vec<HashSet<String>>,
        reason: &mut Option<OnceReason>,
    ) {
        if reason.is_some() {
            return;
        }
        shadows.push(HashSet::new());
        for stmt in &block.stmts {
            if reason.is_some() {
                break;
            }
            match &stmt.kind {
                StmtKind::Let { pattern, value, .. } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                    let names = pattern.binding_names();
                    if let Some(top) = shadows.last_mut() {
                        for n in names {
                            top.insert(n);
                        }
                    }
                }
                StmtKind::LetUninit { name, .. } => {
                    if let Some(top) = shadows.last_mut() {
                        top.insert(name.clone());
                    }
                }
                StmtKind::LetElse {
                    pattern,
                    value,
                    else_block,
                    ..
                } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                    self.walk_capture_consume_block(
                        else_block,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                    let names = pattern.binding_names();
                    if let Some(top) = shadows.last_mut() {
                        for n in names {
                            top.insert(n);
                        }
                    }
                }
                StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                    self.walk_capture_consume_block(
                        body,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                }
                StmtKind::Assign { target, value } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                    self.walk_capture_consume(
                        target,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                    self.walk_capture_consume(
                        target,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                }
                StmtKind::Expr(e) => {
                    self.walk_capture_consume(e, CaptureWalkMode::Reading, outer, shadows, reason);
                }
            }
        }
        if reason.is_none() {
            if let Some(tail) = &block.final_expr {
                self.walk_capture_consume(tail, terminal_mode, outer, shadows, reason);
            }
        }
        shadows.pop();
    }

    fn walk_capture_consume(
        &self,
        expr: &Expr,
        mode: CaptureWalkMode,
        outer: &HashMap<String, Type>,
        shadows: &mut Vec<HashSet<String>>,
        reason: &mut Option<OnceReason>,
    ) {
        if reason.is_some() {
            return;
        }
        match &expr.kind {
            ExprKind::Identifier(name) => {
                if mode == CaptureWalkMode::Consuming && !Self::name_is_shadowed(name, shadows) {
                    if let Some(ty) = outer.get(name) {
                        if !self.is_copy_type_during_check(ty) {
                            *reason = Some(OnceReason {
                                consumed_binding: name.clone(),
                                consumed_span: expr.span.clone(),
                            });
                        }
                    }
                }
            }
            ExprKind::SelfValue => {
                if mode == CaptureWalkMode::Consuming && !Self::name_is_shadowed("self", shadows) {
                    if let Some(ty) = outer.get("self") {
                        if !self.is_copy_type_during_check(ty) {
                            *reason = Some(OnceReason {
                                consumed_binding: "self".to_string(),
                                consumed_span: expr.span.clone(),
                            });
                        }
                    }
                }
            }

            ExprKind::Integer(..)
            | ExprKind::Float(..)
            | ExprKind::Bool(..)
            | ExprKind::CharLit(..)
            | ExprKind::StringLit(..)
            | ExprKind::MultiStringLit(..)
            | ExprKind::InterpolatedStringLit(..)
            | ExprKind::Path { .. }
            | ExprKind::SelfType
            | ExprKind::PipePlaceholder
            | ExprKind::OffsetOf { .. }
            | ExprKind::Error => {}

            ExprKind::Binary { left, right, .. }
            | ExprKind::Pipe { left, right }
            | ExprKind::NilCoalesce { left, right } => {
                self.walk_capture_consume(left, CaptureWalkMode::Reading, outer, shadows, reason);
                self.walk_capture_consume(right, CaptureWalkMode::Reading, outer, shadows, reason);
            }
            ExprKind::Unary { operand, .. } => {
                self.walk_capture_consume(
                    operand,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }

            ExprKind::Call { callee, args } => {
                self.walk_capture_consume(callee, CaptureWalkMode::Reading, outer, shadows, reason);
                let borrow_modes = self.callee_borrow_positions(callee);
                for (i, arg) in args.iter().enumerate() {
                    let is_borrow = arg.mut_marker
                        || borrow_modes
                            .as_ref()
                            .and_then(|m| m.get(i))
                            .copied()
                            .unwrap_or(false);
                    let arg_mode = if is_borrow {
                        CaptureWalkMode::Reading
                    } else {
                        CaptureWalkMode::Consuming
                    };
                    self.walk_capture_consume(&arg.value, arg_mode, outer, shadows, reason);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
                for arg in args {
                    let arg_mode = if arg.mut_marker {
                        CaptureWalkMode::Reading
                    } else {
                        CaptureWalkMode::Consuming
                    };
                    self.walk_capture_consume(&arg.value, arg_mode, outer, shadows, reason);
                }
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
            }
            ExprKind::Index { object, index } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
                self.walk_capture_consume(index, CaptureWalkMode::Reading, outer, shadows, reason);
            }

            ExprKind::Block(block) => {
                self.walk_capture_consume_block(block, mode, outer, shadows, reason);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_capture_consume(
                    condition,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                self.walk_capture_consume_block(then_block, mode, outer, shadows, reason);
                if let Some(eb) = else_branch {
                    self.walk_capture_consume(eb, mode, outer, shadows, reason);
                }
            }
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.walk_capture_consume(value, CaptureWalkMode::Reading, outer, shadows, reason);
                let mut arm_scope: HashSet<String> = HashSet::new();
                for n in pattern.binding_names() {
                    arm_scope.insert(n);
                }
                shadows.push(arm_scope);
                self.walk_capture_consume_block(then_block, mode, outer, shadows, reason);
                shadows.pop();
                if let Some(eb) = else_branch {
                    self.walk_capture_consume(eb, mode, outer, shadows, reason);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_capture_consume(
                    scrutinee,
                    CaptureWalkMode::Consuming,
                    outer,
                    shadows,
                    reason,
                );
                for arm in arms {
                    let mut arm_scope: HashSet<String> = HashSet::new();
                    for n in arm.pattern.binding_names() {
                        arm_scope.insert(n);
                    }
                    shadows.push(arm_scope);
                    if let Some(g) = &arm.guard {
                        self.walk_capture_consume(
                            g,
                            CaptureWalkMode::Reading,
                            outer,
                            shadows,
                            reason,
                        );
                    }
                    self.walk_capture_consume(&arm.body, mode, outer, shadows, reason);
                    shadows.pop();
                }
            }

            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_capture_consume(
                    condition,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }
            ExprKind::WhileLet {
                pattern,
                value,
                body,
                ..
            } => {
                self.walk_capture_consume(value, CaptureWalkMode::Reading, outer, shadows, reason);
                let mut arm_scope: HashSet<String> = HashSet::new();
                for n in pattern.binding_names() {
                    arm_scope.insert(n);
                }
                shadows.push(arm_scope);
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                shadows.pop();
            }
            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                self.walk_capture_consume(
                    iterable,
                    CaptureWalkMode::Consuming,
                    outer,
                    shadows,
                    reason,
                );
                let mut arm_scope: HashSet<String> = HashSet::new();
                for n in pattern.binding_names() {
                    arm_scope.insert(n);
                }
                shadows.push(arm_scope);
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                shadows.pop();
            }
            ExprKind::Loop { body, .. } => {
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }

            ExprKind::LabeledBlock { body, .. } => {
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }

            ExprKind::Break { value: Some(v), .. } | ExprKind::Return(Some(v)) => {
                self.walk_capture_consume(v, CaptureWalkMode::Consuming, outer, shadows, reason);
            }
            ExprKind::Break { value: None, .. }
            | ExprKind::Continue { .. }
            | ExprKind::Return(None) => {}

            ExprKind::Question(inner) => {
                self.walk_capture_consume(
                    inner,
                    CaptureWalkMode::Consuming,
                    outer,
                    shadows,
                    reason,
                );
            }
            ExprKind::OptionalChain { object, args, .. } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
                if let Some(arg_list) = args {
                    for arg in arg_list {
                        let arg_mode = if arg.mut_marker {
                            CaptureWalkMode::Reading
                        } else {
                            CaptureWalkMode::Consuming
                        };
                        self.walk_capture_consume(&arg.value, arg_mode, outer, shadows, reason);
                    }
                }
            }

            ExprKind::Closure {
                params: nested_params,
                body: nested_body,
                ..
            } => {
                let mut nested_scope: HashSet<String> = HashSet::new();
                for p in nested_params {
                    for n in p.pattern.binding_names() {
                        nested_scope.insert(n);
                    }
                }
                shadows.push(nested_scope);
                self.walk_capture_consume(
                    nested_body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                shadows.pop();
            }

            ExprKind::Cast { expr: inner, .. } => {
                self.walk_capture_consume(inner, mode, outer, shadows, reason);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_capture_consume(s, CaptureWalkMode::Reading, outer, shadows, reason);
                }
                if let Some(e) = end {
                    self.walk_capture_consume(e, CaptureWalkMode::Reading, outer, shadows, reason);
                }
            }

            ExprKind::Tuple(es) | ExprKind::ArrayLiteral(es) => {
                for e in es {
                    self.walk_capture_consume(e, mode, outer, shadows, reason);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_capture_consume(e, mode, outer, shadows, reason);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_capture_consume(value, mode, outer, shadows, reason);
                self.walk_capture_consume(count, CaptureWalkMode::Reading, outer, shadows, reason);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.walk_capture_consume(k, mode, outer, shadows, reason);
                    self.walk_capture_consume(v, mode, outer, shadows, reason);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.walk_capture_consume(
                        &f.value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                }
                if let Some(s) = spread {
                    self.walk_capture_consume(
                        s,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                }
            }

            ExprKind::Par(body)
            | ExprKind::Seq(body)
            | ExprKind::Unsafe(body)
            | ExprKind::Try(body) => {
                self.walk_capture_consume_block(body, mode, outer, shadows, reason);
            }
            ExprKind::Lock { body, .. } => {
                self.walk_capture_consume_block(body, mode, outer, shadows, reason);
            }
            ExprKind::Providers { bindings, body } => {
                for binding in bindings {
                    self.walk_capture_consume(
                        &binding.value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                }
                self.walk_capture_consume_block(body, mode, outer, shadows, reason);
            }
        }
    }

    /// Per-position "is this param a borrow slot?" lookup for the
    /// `Call` arm of the once-callability walker. Returns
    /// `Some(Vec<bool>)` where each `true` means "borrow position
    /// (`ref T` / `mut ref T` / `mut Slice[T]`), so the arg is read,
    /// not consumed". `None` when the callee's signature is unknown
    /// (function-pointer call, type-param method, builtin without an
    /// `env.functions` entry) — the caller falls back to per-arg
    /// defaults (Consuming). Mirrors `ownership::param_modes_from_signature`
    /// without depending on it, by reading directly from `self.env`.
    fn callee_borrow_positions(&self, callee: &Expr) -> Option<Vec<bool>> {
        let key = match &callee.kind {
            ExprKind::Identifier(name) => name.clone(),
            ExprKind::Path { segments, .. } => segments.join("."),
            _ => return None,
        };
        if let Some(sig) = self.env.functions.get(&key) {
            return Some(sig.params.iter().map(Self::is_borrow_param_type).collect());
        }
        if let Some((target, method)) = key.split_once('.') {
            for imp in &self.env.impls {
                // No call-site args context here — borrow-position lookup
                // works off the syntactic `Type.method` key. Conservative
                // post-Theme-4: only generic-on-name impls participate;
                // specialized impls would need an args-aware lookup that
                // this site doesn't carry. Slice-scope deviation (no
                // currently-realistic specialized-impl case for borrow
                // positions).
                if imp.target_type == target && imp.target_args.is_empty() {
                    if let Some(sig) = imp.methods.get(method) {
                        return Some(sig.params.iter().map(Self::is_borrow_param_type).collect());
                    }
                }
            }
        }
        None
    }

    fn is_borrow_param_type(t: &Type) -> bool {
        matches!(
            t,
            Type::Ref(_) | Type::MutRef(_) | Type::Slice { mutable: true, .. }
        )
    }

    fn check_function(
        &mut self,
        f: &Function,
        self_type: Option<&Type>,
        enclosing_generics: &[String],
    ) {
        self.local_scope = LocalTypeScope::new();

        let mut gp = enclosing_generics.to_vec();
        gp.extend(Self::generic_param_names(&f.generic_params));

        // Save outer bounds, merge in function-level bounds. Restored after
        // the body is checked so sibling functions don't see this fn's
        // generics. `merge` semantics: function-level entries shadow outer
        // entries with the same name (innermost wins, mirroring scope).
        let saved_bounds = self.enclosing_bounds.clone();
        for (name, bounds) in Self::collect_param_bounds(&f.generic_params, &f.where_clause) {
            self.enclosing_bounds.insert(name, bounds);
        }

        // Validate default parameter values
        self.validate_default_params(&f.params, &gp);

        // Validate and bind parameters
        for param in &f.params {
            let ty = self.lower_type_expr(&param.ty, &gp);
            self.check_param_irrefutable(param, &ty);
            self.bind_pattern_types(&param.pattern, &ty);
        }

        // Validate inline bounds and where clause (merged — both apply)
        self.validate_all_bounds(&f.generic_params, &f.where_clause, &gp);

        // Bind self
        if f.self_param.is_some() {
            if let Some(st) = self_type {
                self.local_scope.insert("self".to_string(), st.clone());
                self.current_self_type = Some(st.clone());
            }
        }

        let return_type = f
            .return_type
            .as_ref()
            .map(|t| self.lower_type_expr(t, &gp))
            .unwrap_or(Type::Unit);
        self.current_return_type = Some(return_type.clone());

        // Type-check body — thread the expected return type through so that
        // a `.into()` in tail position can resolve against it.
        if f.body.final_expr.is_some() {
            self.check_block_against(&f.body, &return_type);
        } else {
            self.infer_block(&f.body);
        }

        self.current_return_type = None;
        self.current_self_type = None;
        self.enclosing_bounds = saved_bounds;
    }

    /// Like `infer_block`, but type-checks the block's final expression
    /// against an expected type so expected-type threading (e.g. `.into()`)
    /// sees the target.
    fn check_block_against(&mut self, block: &Block, expected: &Type) -> Type {
        self.local_scope.push();
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        let ty = if let Some(ref expr) = block.final_expr {
            self.check_expr(expr, expected)
        } else {
            Type::Unit
        };
        self.local_scope.pop();
        ty
    }

    fn check_impl_block(&mut self, imp: &ImplBlock) {
        let type_name = match &imp.target_type.kind {
            TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
            _ => return,
        };
        // Slice 1b: `env_add_impl` already emitted
        // `E_OPAQUE_TYPE_NO_INHERENT_OR_TRAIT_IMPLS` for impls on opaque
        // foreign types and skipped registration. Silently skip method-body
        // checking here too so the user sees one focused diagnostic, not a
        // cascade of `self`-argument REQUIRES_INDIRECTION + missing-supertrait
        // noise from the unregistered impl.
        if self.env.opaque_foreign_types.contains(&type_name) {
            return;
        }
        let self_type = Type::Named {
            name: type_name.clone(),
            args: Vec::new(),
        };

        // Validate inline bounds and where clause on the impl block itself
        let gp = Self::generic_param_names(&imp.generic_params);
        self.validate_all_bounds(&imp.generic_params, &imp.where_clause, &gp);

        // Check that trait impls provide all required associated types,
        // and that all supertrait impls exist for the same target type.
        if let Some(ref trait_path) = imp.trait_name {
            let trait_name = trait_path.segments.last().cloned().unwrap_or_default();
            // `impl MarkerTrait for T { fn ... }` — the body of an impl
            // for a marker trait must be empty. Per design.md § Marker
            // Traits.
            if self.env.marker_traits.contains(&trait_name) {
                let has_items = imp
                    .items
                    .iter()
                    .any(|item| matches!(item, ImplItem::Method(_) | ImplItem::AssocType(_)));
                if has_items {
                    self.type_error(
                        format!(
                            "error[E_MARKER_IMPL_HAS_METHOD]: impl of marker trait \
                             '{trait_name}' cannot contain methods or items; \
                             the body must be empty"
                        ),
                        imp.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
            // `impl TraitAlias for T` is rejected at v1: trait aliases are
            // not implementable directly. Per design.md § Trait Aliases —
            // implement each component trait separately. The bound list is
            // copy-pasted into the diagnostic so the user can apply the
            // workaround inline.
            if self.is_trait_alias(&trait_name) {
                let bound_list = self
                    .trait_alias_bound_list(&trait_name)
                    .unwrap_or_else(|| "<bounds>".to_string());
                self.type_error(
                    format!(
                        "error[E_IMPL_TRAIT_ALIAS]: cannot implement trait alias \
                         '{trait_name}'; implement each component trait \
                         separately: `{bound_list}`"
                    ),
                    imp.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
            }
            if let Some(trait_info) = self.env.traits.get(&trait_name).cloned() {
                let provided: HashSet<String> = imp
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        ImplItem::AssocType(binding) => Some(binding.name.clone()),
                        _ => None,
                    })
                    .collect();
                for required in &trait_info.assoc_types {
                    if !provided.contains(required) {
                        self.type_error(
                            format!(
                                "impl of trait '{}' is missing associated type '{}'",
                                trait_name, required
                            ),
                            imp.span.clone(),
                            TypeErrorKind::MissingField,
                        );
                    }
                }
                // Supertrait constraint: every supertrait of `trait_name` must
                // have an impl for the same target type. Theme-4 deviation:
                // when `imp` is specialized (`impl Foo for Bar[i32]`), the
                // ideal supertrait check would require `impl SuperFoo for
                // Bar[i32]` specifically; currently we accept either a
                // matching specialized supertrait OR a generic-on-name
                // supertrait. Tightening is out of scope until a real
                // specialized-with-supertrait case appears.
                for supertrait in &trait_info.supertraits {
                    let has_impl = self.env.impls.iter().any(|info| {
                        info.trait_name.as_deref() == Some(supertrait.as_str())
                            && info.target_type == type_name
                    });
                    if !has_impl {
                        self.type_error(
                            format!(
                                "impl {} for {} requires impl {} for {}",
                                trait_name, type_name, supertrait, type_name
                            ),
                            imp.span.clone(),
                            TypeErrorKind::MissingSupertrait,
                        );
                    }
                }
            }
        }

        // Store assoc type bindings so `resolve_assoc_projections` can look
        // them up when substituting `T.Item` after `T` is solved to this type.
        let gp = Self::generic_param_names(&imp.generic_params);

        // Save outer bounds, merge in impl-level bounds. Method bodies see
        // both the impl's generic params and their own; `check_function`
        // further merges method-level bounds and restores after each method.
        let saved_bounds = self.enclosing_bounds.clone();
        for (name, bounds) in Self::collect_param_bounds(&imp.generic_params, &imp.where_clause) {
            self.enclosing_bounds.insert(name, bounds);
        }

        for item in &imp.items {
            match item {
                ImplItem::Method(method) => self.check_function(method, Some(&self_type), &[]),
                ImplItem::AssocType(binding) => {
                    let bound_ty = self.lower_type_expr(&binding.ty, &gp);
                    self.env
                        .impl_assoc_types
                        .insert((type_name.clone(), binding.name.clone()), bound_ty);
                }
            }
        }

        self.enclosing_bounds = saved_bounds;
    }

    fn check_const_decl(&mut self, c: &ConstDecl) {
        let declared_ty = self.lower_type_expr(&c.ty, &[]);
        let value_ty = self.infer_expr(&c.value);
        self.check_assignable(&declared_ty, &value_ty, c.value.span.clone());
    }

    // ── Block & Statement ───────────────────────────────────────

    fn infer_block(&mut self, block: &Block) -> Type {
        self.local_scope.push();
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        let ty = if let Some(ref expr) = block.final_expr {
            self.infer_expr(expr)
        } else {
            Type::Unit
        };
        self.local_scope.pop();
        ty
    }

    /// Diagnose unsolved generic type parameters in a synthesis-mode
    /// inferred type. Currently called from `let x = e;` and
    /// `let pat = e else …` when the user supplied no type annotation:
    /// without a check-mode expected type to pin them, any `TypeParam(T)`
    /// in `inferred` that isn't an enclosing function/impl generic is
    /// unsolvable at this site. Item 131 sub-step 2a.
    fn check_unsolved_type_param(&mut self, inferred: &Type, span: &Span) {
        if matches!(inferred, Type::Error) {
            return;
        }
        let unbound_type: Option<String> = {
            let in_scope: HashSet<&str> =
                self.enclosing_bounds.keys().map(|s| s.as_str()).collect();
            find_unbound_type_param(inferred, &in_scope).map(|s| s.to_string())
        };
        if let Some(name) = unbound_type {
            self.type_error(
                format!(
                    "cannot infer type parameter '{}'; add a type annotation to this binding",
                    name
                ),
                span.clone(),
                TypeErrorKind::CannotInferTypeParam,
            );
        }
        // Const generics slice 3b sub-step (h): const-param analog.
        // Surfaces `cannot infer const parameter 'N'` for return-only
        // / bounds-only const-params that the call-site solver
        // couldn't pin from arguments (e.g.
        // `fn f[const N: i64]() -> Array[i64, N]` called as `let x = f();`
        // without an annotation).
        let unbound_const: Option<String> = {
            let in_scope: HashSet<&str> =
                self.enclosing_bounds.keys().map(|s| s.as_str()).collect();
            find_unbound_const_param(inferred, &in_scope).map(|s| s.to_string())
        };
        if let Some(name) = unbound_const {
            self.type_error(
                format!(
                    "cannot infer const parameter '{}'; provide explicit generic args \
                     (e.g. `f[..., 8](...)`) or add a type annotation to this binding",
                    name
                ),
                span.clone(),
                TypeErrorKind::CannotInferTypeParam,
            );
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                is_mut: _,
                pattern,
                ty,
                value,
            } => {
                let expected_ty = if let Some(ty_expr) = ty {
                    let declared = self.lower_type_expr(ty_expr, &[]);
                    self.check_expr(value, &declared);
                    declared
                } else {
                    let inferred = self.infer_expr(value);
                    self.check_unsolved_type_param(&inferred, &value.span);
                    inferred
                };
                // Per design.md: `let PAT = expr;` requires `PAT` to be
                // irrefutable (the binding has no else-arm; a missed
                // pattern would have nowhere to dispatch). Refutable
                // patterns must use `let ... else { … }` (which has its
                // own check at `StmtKind::LetElse`) or `if let` /
                // `while let`. The check inherits through `@` bindings
                // — `let x @ Option.Some(y) = opt` is rejected because
                // the inner `Option.Some(y)` is refutable.
                if !self.is_irrefutable_pattern(pattern, &expected_ty) {
                    self.type_error(
                        "refutable pattern in `let` binding; use `let ... else { ... }`, \
                         `if let`, or `match` for patterns that may not match"
                            .to_string(),
                        pattern.span.clone(),
                        TypeErrorKind::RefutablePattern,
                    );
                }
                self.bind_pattern_types(pattern, &expected_ty);
            }
            StmtKind::LetUninit {
                is_mut: _,
                name,
                name_span,
                ty,
            } => {
                let declared = self.lower_type_expr(ty, &[]);
                // Expose the declared type at the binding's name span so later
                // phases (ownership) can recover it without reaching into
                // `local_scope`. The Let arm above stores via bind_pattern_types;
                // LetUninit has no RHS so we record directly.
                self.expr_types
                    .insert(SpanKey::from_span(name_span), declared.clone());
                self.local_scope.insert(name.clone(), declared);
            }
            StmtKind::LetElse {
                pattern,
                ty,
                value,
                else_block,
            } => {
                let expected_ty = if let Some(ty_expr) = ty {
                    let declared = self.lower_type_expr(ty_expr, &[]);
                    self.check_expr(value, &declared);
                    declared
                } else {
                    let inferred = self.infer_expr(value);
                    self.check_unsolved_type_param(&inferred, &value.span);
                    inferred
                };
                self.bind_pattern_types(pattern, &expected_ty);
                let else_ty = self.infer_block(else_block);
                if else_ty != Type::Never && else_ty != Type::Error {
                    self.type_error(
                        "let...else block must diverge (return, break, continue, or panic)"
                            .to_string(),
                        else_block.span.clone(),
                        TypeErrorKind::BranchTypeMismatch,
                    );
                }
            }
            StmtKind::Defer { body } => {
                let prev = self.in_defer;
                self.in_defer = true;
                self.infer_block(body);
                self.in_defer = prev;
            }
            StmtKind::ErrDefer { binding, body } => {
                let prev = self.in_defer;
                self.in_defer = true;
                // If errdefer(e), bind `e` in a new scope — typed as the Err
                // variant of the enclosing function's return type (stubbed as
                // Error for now since Result type is not yet fully implemented).
                if let Some(name) = binding {
                    self.local_scope.push();
                    self.local_scope.insert(name.clone(), Type::Error);
                }
                self.infer_block(body);
                if binding.is_some() {
                    self.local_scope.pop();
                }
                self.in_defer = prev;
            }
            StmtKind::Assign { target, value } => {
                // Reject `*r = v` when `r: ref T` — shared borrow is read-only.
                if let ExprKind::Unary {
                    op: UnaryOp::Deref,
                    operand,
                } = &target.kind
                {
                    let ref_ty = self.infer_expr(operand);
                    if matches!(ref_ty, Type::Ref(_)) {
                        self.type_error(
                            "cannot assign through a shared reference ('ref T'); use 'mut ref T'"
                                .to_string(),
                            target.span.clone(),
                            TypeErrorKind::InvalidUnaryOp,
                        );
                    }
                }
                let target_ty = self.infer_expr(target);
                self.check_expr(value, &target_ty);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                self.infer_expr(target);
                self.infer_expr(value);
            }
            StmtKind::Expr(expr) => {
                self.infer_expr(expr);
            }
        }
    }

    // ── Expression Type Inference ───────────────────────────────

    /// Type-check an expression against an expected type. Recognizes `x.into()`
    /// against a Named expected type and records a rewrite to `Target.from(x)`
    /// in `into_conversions`; for everything else, falls back to `infer_expr`
    /// plus a `check_assignable` boundary.
    fn check_expr(&mut self, expr: &Expr, expected: &Type) -> Type {
        // Built-in collection constructors at check-mode: `Vec.new()` /
        // `VecDeque.new()` / `Set.new()` / `SortedSet.new()` / `Map.new()`
        // resolve to the expected type directly when the surface names
        // line up. Without this short-circuit the constructor's synth-
        // mode return (`Vec[?T]` minted by `infer_call`) flows through
        // `types_compatible`, which can't unify the fresh typevar
        // against `Vec<Fn()>` etc. (the existing legacy callers' shape).
        if let ExprKind::Call { callee, args } = &expr.kind {
            if args.is_empty() {
                if let ExprKind::Path { segments, .. } = &callee.kind {
                    if segments.len() == 2 && segments[1] == "new" {
                        let collection = segments[0].as_str();
                        let matches_expected = match (collection, expected) {
                            ("Vec", Type::Named { name, .. }) => name == "Vec",
                            ("VecDeque", Type::Named { name, .. }) => name == "VecDeque",
                            ("Set", Type::Named { name, .. }) => name == "Set",
                            ("SortedSet", Type::Named { name, .. }) => name == "SortedSet",
                            ("Map", Type::Named { name, .. }) => name == "Map",
                            _ => false,
                        };
                        if matches_expected {
                            self.record_expr_type(&expr.span, expected);
                            return expected.clone();
                        }
                    }
                }
            }
        }

        // Empty prefix-literal (`Vec[]` / `Array[]` / `Set[]` / `Map[]`) at
        // a check-mode position: recover via the expected type. Synthesis-
        // mode use (no annotation, no expected-type carrier) hits the
        // matching arm in `infer_expr_inner` and emits
        // `E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION`. Per design.md
        // § Collection Literals: an empty prefix-literal has no element
        // type to infer.
        if let ExprKind::PrefixCollectionLiteral { type_name, items } = &expr.kind {
            if items.is_empty() {
                let matches_expected = match (type_name.as_str(), expected) {
                    ("Vec", Type::Named { name, .. }) => name == "Vec",
                    ("Set", Type::Named { name, .. }) => name == "Set",
                    ("Map", Type::Named { name, .. }) => name == "Map" || name == "HashMap",
                    ("Array", Type::Array { .. }) => true,
                    _ => false,
                };
                if matches_expected {
                    self.record_expr_type(&expr.span, expected);
                    return expected.clone();
                }
            }
        }
        // Bare-identifier call at an expected-type position: `default()` where
        // expected is `T: Default` or a concrete type with an `impl Default`.
        // Intercepts before normal inference so the typechecker can substitute
        // the missing receiver (`T.default()` / `Wrapper.default()`).
        if let ExprKind::Call { callee, args } = &expr.kind {
            if let ExprKind::Identifier(name) = &callee.kind {
                if let Some(ty) =
                    self.try_apply_expected_assoc_fn_inference(name, args, expected, &expr.span)
                {
                    return ty;
                }
            }
        }

        // Check-mode coercion: bare `[...]` literal → `Array[T, N]` when the
        // expected type is a fixed-size array. This overrides the synthesis-mode
        // default of Vec[T] so annotated lets and typed call arguments work.
        if let (ExprKind::ArrayLiteral(elements), Type::Array { element, size }) =
            (&expr.kind, expected)
        {
            // Length-mismatch check skipped for non-literal sizes (slice 3
            // `ConstParam` / `ConstVar` resolve at mono-emission time).
            if let Some(n) = size.as_usize() {
                if elements.len() != n {
                    self.type_error(
                        format!(
                            "array literal has {} element(s), expected {}",
                            elements.len(),
                            n
                        ),
                        expr.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
            for elem in elements {
                self.check_expr(elem, element);
            }
            self.record_expr_type(&expr.span, expected);
            return expected.clone();
        }
        // Same coercion for bare `[v; n]` against an `Array[T, N]` expected:
        // the literal's count must equal N, and the value's type must match T.
        if let (
            ExprKind::RepeatLiteral {
                type_name: None,
                value,
                count,
            },
            Type::Array { element, size },
        ) = (&expr.kind, expected)
        {
            if let ExprKind::Integer(n, _) = &count.kind {
                // Length-mismatch check skipped for non-literal sizes
                // (slice 3 `ConstParam` / `ConstVar` resolve at mono-
                // emission time).
                if let Some(expected_size) = size.as_usize() {
                    if *n < 0 || *n as usize != expected_size {
                        self.type_error(
                            format!(
                                "repeat-literal count {} does not match expected array length {}",
                                n, expected_size
                            ),
                            count.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
            } else {
                self.type_error(
                    "Array[T, N] repeat-literal requires a non-negative integer literal count"
                        .to_string(),
                    count.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                self.infer_expr(count);
            }
            self.check_expr(value, element);
            self.record_expr_type(&expr.span, expected);
            return expected.clone();
        }
        if let Some(coerced) = self.try_apply_into_coercion(expr, expected) {
            return coerced;
        }
        if let Some(coerced) = self.try_apply_tryinto_coercion(expr, expected) {
            return coerced;
        }
        // Closure pushdown: when expected is `Type::Function { params, return }`
        // (or `Type::OnceFunction { ... }`, item 131 sub-step 3) and `expr` is
        // a closure literal, seed each closure param's type from the expected
        // param type instead of letting the synth path fall back to
        // `fresh_type_var()`. Required for compound type+effect polymorphism
        // (round 10.1 step 2): once the call site has solved `T = Iter[i32]`
        // and substituted `T.Item -> &i32` into the param's `Fn(T.Item) -> ...`,
        // the closure body must be type-checked against that concrete shape.
        // Explicit param annotations on the closure still take priority.
        // OnceFunction slots use the same pushdown — the slot's signature
        // describes call arity/types regardless of repeat-callability, and
        // sub-step 3's `is_subtype` then admits a Function-typed closure
        // into an OnceFunction slot via the cross-arm subsumption rule.
        let expected_fn_shape = match expected {
            Type::Function {
                params,
                return_type,
            }
            | Type::OnceFunction {
                params,
                return_type,
            } => Some((params.as_slice(), return_type.as_ref())),
            _ => None,
        };
        if let (
            ExprKind::Closure {
                params,
                capture_mode,
                prefix_span: _,
                body,
            },
            Some((expected_params, expected_ret)),
        ) = (&expr.kind, expected_fn_shape)
        {
            if params.len() == expected_params.len() {
                // Round 12.44 (Step 2) — once-callability inference must run
                // here too so the closure's actual type reflects whether it
                // consumes a captured outer non-Copy binding. When `expected`
                // is `Type::Function` and the body promotes the closure to
                // `OnceFunction`, the trailing `check_assignable` correctly
                // rejects the cross-pair (Step 1's identity-only subtyping).
                let outer_bindings = self.flatten_local_scope_snapshot();
                let closure_param_names: Vec<String> = params
                    .iter()
                    .flat_map(|p| p.pattern.binding_names())
                    .collect();
                self.local_scope.push();
                let param_types: Vec<Type> = params
                    .iter()
                    .zip(expected_params.iter())
                    .map(|(p, expected_pty)| {
                        let ty = p
                            .ty
                            .as_ref()
                            .map(|t| self.lower_type_expr(t, &[]))
                            .unwrap_or_else(|| expected_pty.clone());
                        if !self.is_irrefutable_pattern(&p.pattern, &ty) {
                            self.type_error(
                                "refutable pattern in closure parameter; use `if let` or `match` for patterns that may not match".to_string(),
                                p.pattern.span.clone(),
                                TypeErrorKind::RefutablePattern,
                            );
                        }
                        self.bind_pattern_types(&p.pattern, &ty);
                        ty
                    })
                    .collect();
                let body_ty = self.check_expr(body, expected_ret);
                self.local_scope.pop();
                let actual = self.closure_type_with_capture_inference(
                    &expr.span,
                    *capture_mode,
                    &closure_param_names,
                    body,
                    &outer_bindings,
                    param_types,
                    body_ty,
                );
                self.check_assignable(expected, &actual, expr.span.clone());
                return actual;
            }
            // Arity mismatch: fall through to the synth path so the existing
            // `check_assignable` produces a normal `Fn` arity diagnostic.
        }

        // Block at check position: thread `expected` through to the
        // trailing expression so closures inside `let x: T = { ...; |a| body }`
        // see `T`'s shape. `check_block_against` already routes the final
        // expression through `check_expr`.
        if let ExprKind::Block(block) = &expr.kind {
            let ty = self.check_block_against(block, expected);
            self.record_expr_type(&expr.span, &ty);
            return ty;
        }

        // If/IfLet at check position: push `expected` into both branches.
        // Each branch's `check_expr` enforces assignability against the
        // expected type independently, so divergent branches surface a
        // per-branch TypeMismatch rather than the synth-mode aggregate
        // BranchTypeMismatch (more specific, points at the offending
        // branch). Condition typing is unchanged.
        if let ExprKind::If {
            condition,
            then_block,
            else_branch,
        } = &expr.kind
        {
            let ty = self.check_if_against(
                condition,
                then_block,
                else_branch.as_deref(),
                expected,
                &expr.span,
            );
            return ty;
        }
        if let ExprKind::IfLet {
            pattern,
            value,
            then_block,
            else_branch,
        } = &expr.kind
        {
            let ty = self.check_if_let_against(
                pattern,
                value,
                then_block,
                else_branch.as_deref(),
                expected,
                &expr.span,
            );
            return ty;
        }

        // Match at check position: each arm body is checked against
        // `expected` so closures in arm bodies (and other check-mode-
        // sensitive shapes) see the target type.
        if let ExprKind::Match { scrutinee, arms } = &expr.kind {
            let ty = self.check_match_against(scrutinee, arms, expected, &expr.span);
            return ty;
        }

        let actual = self.infer_expr(expr);
        // Expected-type-driven generic resolution: when a generic call's
        // return type came back as `TypeParam(T)` (the solver had no arg
        // information to fix `T`), `expected` lets us bind `T` to a concrete
        // name for the interpreter's runtime dispatch stack. Only fires for
        // `Call` expressions — other shapes don't introduce per-call generic
        // bindings.
        if matches!(expr.kind, ExprKind::Call { .. }) {
            if let Type::TypeParam(t_name) = &actual {
                if let Some(target) = type_to_concrete_or_param_name(expected) {
                    if target != *t_name {
                        self.call_type_subs
                            .entry(SpanKey::from_span(&expr.span))
                            .or_default()
                            .insert(t_name.clone(), target);
                    }
                }
            }
        }
        self.check_assignable(expected, &actual, expr.span.clone());
        actual
    }

    /// Recognize `x.into()` at an expected-type position. When `expr` is a
    /// zero-argument method call named `into` and `expected` is a Named type
    /// `T` with a registered `impl From[S] for T` (where `S` is the receiver's
    /// inferred type), record the conversion and return `expected`. Returns
    /// `Some(Error)` when `.into()` matches shape but no suitable From impl
    /// exists (emits a diagnostic). Returns `None` when the expression is not
    /// a `.into()` call — caller falls back to regular inference.
    /// Bare-call expected-type inference: `name(args)` at an expected-type
    /// position resolves to `Target.name(args)` when the expected type narrows
    /// to a single trait (or impl) declaring an associated function called
    /// `name`. Returns `Some(return_type)` on dispatch, `None` to fall through
    /// to the existing inference path. Multiple matching traits → ambiguity
    /// error + `Type::Error`.
    ///
    /// `Type::TypeParam(t)` looks up `t`'s trait bounds via `enclosing_bounds`.
    /// `Type::Named { name }` looks up the type's `impl Trait for Name` blocks
    /// in `env.impls` and uses the registered impl method signature directly.
    fn try_apply_expected_assoc_fn_inference(
        &mut self,
        name: &str,
        args: &[CallArg],
        expected: &Type,
        span: &Span,
    ) -> Option<Type> {
        // If `name` is already a known function, builtin, or local, fall
        // through. Bare-call inference only applies to identifiers that
        // would otherwise be unresolvable at the value layer.
        if self.local_scope.lookup(name).is_some()
            || self.env.functions.contains_key(name)
            || self.env.constants.contains_key(name)
            || matches!(
                name,
                "todo" | "unreachable" | "println" | "print" | "eprintln" | "panic"
            )
        {
            return None;
        }

        match expected {
            Type::TypeParam(target) => {
                let bounds = self.enclosing_bounds.get(target).cloned()?;
                let candidates: Vec<String> = bounds
                    .iter()
                    .filter_map(|b| b.path.last().cloned())
                    .filter(|trait_name| self.find_trait_method(trait_name, name).is_some())
                    .collect();
                match candidates.len() {
                    0 => None,
                    1 => {
                        let trait_method = self.find_trait_method(&candidates[0], name)?.clone();
                        // Record the typeparam target so lowering rewrites
                        // the bare call to `T.name(args)`. At runtime the
                        // interpreter resolves `T` through its substitution
                        // stack to find the concrete impl.
                        self.bare_assoc_fn_targets
                            .insert(SpanKey::from_span(span), target.clone());
                        Some(self.dispatch_trait_assoc_fn(target, &trait_method, args, span))
                    }
                    _ => {
                        let trait_list = candidates
                            .iter()
                            .map(|c| format!("`{}`", c))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.type_error(
                            format!(
                                "ambiguous associated function '{}' on type parameter '{}': declared by {}. \
                                 Use UFCS `Trait.{}(...)` to disambiguate.",
                                name, target, trait_list, name,
                            ),
                            span.clone(),
                            TypeErrorKind::AmbiguousAssocFn,
                        );
                        Some(Type::Error)
                    }
                }
            }
            Type::Named {
                name: target_name,
                args: target_args,
            } => {
                // Match against impl methods registered on this concrete type.
                // Trait impls and inherent impls share the same `env.impls`
                // table; we collect every impl whose target is `target_name`,
                // whose method set contains `name`, and whose impl-level
                // bounds discharge against the receiver's concrete generic
                // args (slice 1 of the method-resolution CR — see
                // `phase-4-interpreter.md`).
                let matching: Vec<&ImplInfo> = self
                    .env
                    .impls
                    .iter()
                    .filter(|imp| {
                        imp.target_type == *target_name
                            && impl_args_match(&imp.target_args, target_args)
                            && imp.methods.contains_key(name)
                            && self.env.impl_bounds_discharge(imp, target_args)
                    })
                    .collect();
                match matching.len() {
                    0 => None,
                    1 => {
                        let sig = matching[0].methods.get(name)?.clone();
                        // Record the resolved target so lowering can rewrite
                        // the bare call to `Target.name(args)` for the
                        // interpreter / codegen.
                        self.bare_assoc_fn_targets
                            .insert(SpanKey::from_span(span), target_name.clone());
                        Some(self.validate_args_against_sig(name, &sig, args, span))
                    }
                    _ => {
                        let trait_list = matching
                            .iter()
                            .filter_map(|imp| imp.trait_name.clone())
                            .map(|t| format!("`{}`", t))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.type_error(
                            format!(
                                "ambiguous associated function '{}' on type '{}': declared by {}. \
                                 Use `Trait.{}(...)` to disambiguate.",
                                name, target_name, trait_list, name,
                            ),
                            span.clone(),
                            TypeErrorKind::AmbiguousAssocFn,
                        );
                        Some(Type::Error)
                    }
                }
            }
            _ => None,
        }
    }

    /// Record per-call generic-param substitutions for use by the interpreter
    /// at runtime. Each entry maps a generic param name to a concrete type
    /// name — or to another generic param name when the caller is itself
    /// generic and propagates the binding (the interpreter resolves these
    /// transitively against its runtime substitution stack).
    fn record_call_type_subs(&mut self, span: &Span, solutions: &HashMap<String, Type>) {
        if solutions.is_empty() {
            return;
        }
        let mut frame: HashMap<String, String> = HashMap::new();
        for (name, ty) in solutions {
            if let Some(resolved) = type_to_concrete_or_param_name(ty) {
                frame.insert(name.clone(), resolved);
            }
        }
        if !frame.is_empty() {
            self.call_type_subs.insert(SpanKey::from_span(span), frame);
        }
    }

    /// Type-check call arguments against `(params, return_type)` with the
    /// round-10.1 closure-pushdown logic, returning the (possibly-substituted)
    /// return type. Shared by `infer_call` and the user-defined-method branch
    /// of `infer_method_call` so generic methods get the same inference fix as
    /// generic free functions.
    ///
    /// Behavior:
    /// - Non-generic signature: each arg checked against its slot via
    ///   `check_expr` (already does closure pushdown for monomorphic `Fn(...)`).
    /// - Generic signature: two-pass — non-closure args inferred eagerly to
    ///   solve `T`s, then closure args checked against the substituted slot
    ///   via `check_expr` (so a closure's params see the solved `T`, not a
    ///   fresh var). The substitution is recorded under
    ///   `record_subs_for_span` for downstream consumers (interpreter,
    ///   codegen).
    ///
    /// `apply_call_site_marker` controls the `mut` marker check; pass `false`
    /// for method calls (per design.md, the call-site marker rule applies only
    /// to free-function calls).
    fn check_call_args_with_substitution(
        &mut self,
        args: &[CallArg],
        params: &[Type],
        return_type: &Type,
        record_subs_for_span: &Span,
        apply_call_site_marker: bool,
    ) -> Type {
        self.check_call_args_with_substitution_full(
            args,
            params,
            return_type,
            record_subs_for_span,
            apply_call_site_marker,
            None,
            None,
            None,
            record_subs_for_span,
        )
    }

    /// Extended variant of `check_call_args_with_substitution` that
    /// accepts explicit call-site generic args + the function's
    /// declaration-order generic-param names (const generics slice 1c)
    /// and the callee's where-clause for bound discharge (slice 3c).
    /// When `explicit_generic_args` and `formal_generic_params` are
    /// both supplied, each (formal_name, explicit_arg) pair pre-binds
    /// the corresponding metavar so subsequent arg-position
    /// unification flows from the explicit binding. After the
    /// inference solver runs, each `WhereConstraint::ConstPredicate`
    /// in `where_clause` is evaluated against the resolved const-args;
    /// `Bool(false)` triggers a `"const constraint violated"`
    /// diagnostic at `discharge_span`.
    #[allow(clippy::too_many_arguments)]
    fn check_call_args_with_substitution_full(
        &mut self,
        args: &[CallArg],
        params: &[Type],
        return_type: &Type,
        record_subs_for_span: &Span,
        apply_call_site_marker: bool,
        explicit_generic_args: Option<&[GenericArg]>,
        formal_generic_params: Option<&[String]>,
        where_clause: Option<&WhereClause>,
        discharge_span: &Span,
    ) -> Type {
        // Const generics slice 3c: when the callee declares a
        // where-clause with `ConstPredicate`s, force the full
        // instantiate+unify+resolve+discharge path even if neither
        // params nor return reference a generic — the predicate may
        // reference const-params that don't appear in the signature's
        // types (`fn f[const N: i64]() where N >= 0`). Without this
        // override the early-return below skips discharge entirely.
        let has_where_const_predicate = where_clause
            .map(|wc| {
                wc.constraints
                    .iter()
                    .any(|c| matches!(c, WhereConstraint::ConstPredicate { .. }))
            })
            .unwrap_or(false);
        let has_generic = params.iter().any(contains_type_param)
            || contains_type_param(return_type)
            || has_where_const_predicate;
        if !has_generic {
            for (arg, param_ty) in args.iter().zip(params.iter()) {
                let arg_ty = self.check_expr(&arg.value, param_ty);
                if apply_call_site_marker {
                    self.check_call_site_marker(arg, param_ty, &arg_ty);
                }
            }
            return return_type.clone();
        }
        // Generic case: types-first / effects-second per design.md
        // § Monomorphization order for compound polymorphism. Item 131
        // sub-step 2b — replaces the per-call ad-hoc `solve_type_params`
        // with fresh-metavariable instantiation: each `TypeParam(T)` in
        // the callee's signature becomes a fresh `TypeVar(?M_n)` for
        // this call only, so cross-call collisions are impossible.
        // Pass 1 infers non-closure args and unifies them against the
        // instantiated slot types; pass 2 checks each arg (including
        // closures) against the resolved slot, with check_expr's
        // pushdown seeing concrete (i.e. solved) slot types when
        // available.
        let InstantiatedSignature {
            params: sub_params,
            return_type: sub_ret,
            name_to_id,
            id_to_name,
            name_to_const_id,
            const_id_to_name,
        } = instantiate_signature_with_fresh_vars(
            params,
            return_type,
            &mut self.env.next_type_var,
            &mut self.env.next_const_var,
        );

        // Const generics slice 1c: pre-bind metavars from explicit
        // call-site generic args. Walk the formal-param names and the
        // user-supplied args in lockstep; each `GenericArg::Const`
        // literal binds the corresponding `ConstVar`, each
        // `GenericArg::Type` binds the corresponding `TypeVar`. The
        // subsequent arg-position unification flow runs against these
        // pre-bindings (so a mismatch between explicit and inferred
        // const-args surfaces at the per-position unify call).
        if let (Some(explicit), Some(formal_names)) = (explicit_generic_args, formal_generic_params)
        {
            for (formal_name, explicit_arg) in formal_names.iter().zip(explicit.iter()) {
                if let Some(&const_id) = name_to_const_id.get(formal_name) {
                    if let GenericArg::Const(expr) = explicit_arg {
                        if let Some(cv) = const_value_from_literal(expr) {
                            self.env
                                .const_substitutions
                                .insert(const_id, ConstArg::Literal(cv));
                        }
                    }
                } else if let Some(&type_id) = name_to_id.get(formal_name) {
                    if let GenericArg::Type(te) = explicit_arg {
                        let ty = self.lower_type_expr(te, &[]);
                        self.env.substitutions.insert(type_id, ty);
                    }
                }
            }
        }

        let mut arg_tys: Vec<Option<Type>> = Vec::with_capacity(args.len());
        for arg in args {
            if matches!(arg.value.kind, ExprKind::Closure { .. }) {
                arg_tys.push(None);
            } else {
                arg_tys.push(Some(self.infer_expr(&arg.value)));
            }
        }
        // Pass 1: unify non-closure arg types into the instantiated
        // slot types so the metavars get bound from arguments. Failure
        // is silent here — pass 2's `check_assignable` produces the
        // user-facing diagnostic, and unify already records partial
        // structural matches.
        for (sub_param_ty, arg_ty_opt) in sub_params.iter().zip(arg_tys.iter()) {
            if let Some(arg_ty) = arg_ty_opt {
                unify_types(
                    sub_param_ty,
                    arg_ty,
                    &mut self.env.substitutions,
                    &mut self.env.const_substitutions,
                );
            }
        }
        // Pass 2: check each arg against the resolved slot. For
        // closure args, the resolved slot may be a concrete
        // `Fn(i64) -> i64` (when T solved) and check_expr's pushdown
        // gives the closure params their types.
        for ((arg, sub_param_ty), arg_ty_opt) in
            args.iter().zip(sub_params.iter()).zip(arg_tys.iter())
        {
            let resolved = resolve_type_vars(
                sub_param_ty,
                &self.env.substitutions,
                &id_to_name,
                &self.env.const_substitutions,
                &const_id_to_name,
            );
            let resolved = self.resolve_assoc_projections(&resolved);
            match arg_ty_opt {
                Some(arg_ty) => {
                    self.check_assignable(&resolved, arg_ty, arg.value.span.clone());
                    if apply_call_site_marker {
                        self.check_call_site_marker(arg, &resolved, arg_ty);
                    }
                }
                None => {
                    let arg_ty = self.check_expr(&arg.value, &resolved);
                    if apply_call_site_marker {
                        self.check_call_site_marker(arg, &resolved, &arg_ty);
                    }
                }
            }
        }
        // Translate solved metavars back to the original `T → ConcreteType`
        // shape `record_call_type_subs` expects — this is what the
        // interpreter's runtime dispatch consumes for generic-method
        // resolution. Only entries that resolved to something other
        // than the originating TypeParam are recorded; unsolved ones
        // are skipped so the interpreter's resolution stack doesn't
        // see a self-referential `T → T` binding.
        let mut solutions: HashMap<String, Type> = HashMap::new();
        for (name, &id) in &name_to_id {
            let resolved = resolve_type_vars(
                &Type::TypeVar(id),
                &self.env.substitutions,
                &id_to_name,
                &self.env.const_substitutions,
                &const_id_to_name,
            );
            if !matches!(&resolved, Type::TypeParam(n) if n == name) {
                solutions.insert(name.clone(), resolved);
            }
        }
        self.record_call_type_subs(record_subs_for_span, &solutions);

        // Resolve the return type. Unsolved metavars come back as
        // `TypeParam(originating_name)` so the caller's
        // `find_unbound_type_param` (slice 2a) still surfaces the
        // unsolved-T diagnostic.
        let ret = resolve_type_vars(
            &sub_ret,
            &self.env.substitutions,
            &id_to_name,
            &self.env.const_substitutions,
            &const_id_to_name,
        );
        let ret = self.resolve_assoc_projections(&ret);

        // Const generics slice 3c: discharge `WhereConstraint::ConstPredicate`
        // entries against the resolved const-args. The substitution
        // map is built from two sources: inferred const-args (via
        // `name_to_const_id` + `env.const_substitutions` resolved
        // through `resolve_const_arg`), and explicit call-site args
        // (when supplied — formal-param names paired with
        // `explicit_generic_args` positions). Explicit args win on
        // collision (the user-supplied value pins the predicate
        // discharge directly without needing the inference solver to
        // have minted a ConstVar for the param). Slice 2's
        // `eval_const_expr` consumes the substituted predicate.
        if let Some(wc) = where_clause {
            let mut const_arg_subst: HashMap<String, i64> = HashMap::new();
            for (name, &id) in &name_to_const_id {
                let resolved = resolve_const_arg(
                    &ConstArg::ConstVar(id),
                    &self.env.const_substitutions,
                    &const_id_to_name,
                );
                if let ConstArg::Literal(n) = resolved {
                    const_arg_subst.insert(name.clone(), n);
                }
            }
            if let (Some(explicit), Some(formal_names)) =
                (explicit_generic_args, formal_generic_params)
            {
                for (formal_name, explicit_arg) in formal_names.iter().zip(explicit.iter()) {
                    if let GenericArg::Const(e) = explicit_arg {
                        if let Some(v) = const_value_from_literal(e) {
                            const_arg_subst.insert(formal_name.clone(), v);
                        }
                    }
                }
            }
            self.discharge_const_predicates(wc, &const_arg_subst, discharge_span);
            // Trait-bounds-at-codegen enforcement (slice 0.a, sub-step 1
            // of monomorphized collections prereq). Walks
            // `WhereConstraint::TypeBound` predicates in the same where-
            // clause and verifies each formal type-param's concrete
            // binding satisfies its declared bounds. Inline param bounds
            // (`fn f[T: Hash + Eq](...)`) were normalized into the
            // where-clause at FunctionSig construction
            // (`normalize_bounds_into_where_clause`) so this single
            // discharge call covers both inline and where-clause surfaces.
            self.discharge_type_bounds(wc, &solutions, discharge_span);
        }

        ret
    }

    /// Walk a where-clause and discharge each `TypeBound { T: Trait, ... }`
    /// predicate against the resolved type substitution. For each formal
    /// type-param T bound to a concrete type via `solutions`, check that
    /// the concrete type satisfies the trait. Emits a `TypeMismatch`
    /// diagnostic on miss.
    ///
    /// Built-in trait coverage (Hash / Eq / PartialEq / Ord / PartialOrd /
    /// Display on primitives, plus `#[derive(...)]` on named struct/enum
    /// types) flows through `type_satisfies_bound`, which consults the
    /// existing `type_supports_*` helpers before falling back to the
    /// `env.impls` table lookup.
    ///
    /// Slice 0.a, sub-step 1 of monomorphized collections prereq
    /// ([`phase-7-codegen.md`](../docs/implementation_checklist/phase-7-codegen.md)).
    /// Counterpart to `discharge_const_predicates` for ConstPredicate
    /// where-clauses (const generics slice 3c).
    fn discharge_type_bounds(
        &mut self,
        where_clause: &WhereClause,
        solutions: &HashMap<String, Type>,
        discharge_span: &Span,
    ) {
        for constraint in &where_clause.constraints {
            let WhereConstraint::TypeBound {
                type_name, bounds, ..
            } = constraint
            else {
                continue;
            };
            let Some(concrete_ty) = solutions.get(type_name) else {
                // Param unbound at this call site — the unsolved-T
                // diagnostic (slice 2a) handles this; don't double-report.
                continue;
            };
            if matches!(
                concrete_ty,
                Type::TypeParam(_) | Type::TypeVar(_) | Type::Error
            ) {
                // Unresolved metavar / propagating-param / already-error —
                // upstream diagnostics handle. Avoid noise.
                continue;
            }
            for bound in bounds {
                let Some(trait_name) = bound.path.last() else {
                    continue;
                };
                if self.type_satisfies_bound(concrete_ty, trait_name) {
                    continue;
                }
                self.type_error(
                    format!(
                        "trait bound `{}: {}` is not satisfied; `{}` does not implement `{}`",
                        type_name,
                        trait_name,
                        type_display(concrete_ty),
                        trait_name
                    ),
                    discharge_span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
            }
        }
    }

    /// Check whether `ty` satisfies the named trait. Consults three
    /// sources in order:
    ///
    /// 1. **Built-in primitive coverage** for standard traits (Hash, Eq,
    ///    PartialEq, Ord, PartialOrd, Display) — primitives like `i64` /
    ///    `char` / `bool` satisfy these implicitly. The existing
    ///    `type_supports_*` helpers carry this knowledge, including
    ///    `#[derive(...)]`-driven satisfaction on named struct / enum types.
    /// 2. **Other named traits** via the impl table — direct impl lookup
    ///    plus supertrait closure walk via `env.type_satisfies_trait`.
    ///
    /// Returns `false` for types that can't satisfy nominal trait bounds
    /// (function types, raw pointers, type variables) — the discharge
    /// engine guards `TypeVar` / `TypeParam` / `Error` upstream so those
    /// don't reach here in practice.
    fn type_satisfies_bound(&self, ty: &Type, trait_name: &str) -> bool {
        // Built-in coverage via the type_supports_* helpers — these
        // recognize primitives implicitly + named types via
        // `#[derive(Trait)]` registration.
        match trait_name {
            "Hash" => return self.type_supports_hash(ty),
            "Eq" => return self.type_supports_eq(ty),
            "PartialEq" => return self.type_supports_partial_eq(ty),
            "Ord" => return self.type_supports_ord(ty),
            "PartialOrd" => return self.type_supports_partial_ord(ty),
            "Display" => return self.type_supports_display(ty),
            _ => {}
        }
        // Other traits: explicit impl in the table, with supertrait closure.
        let Some((ty_name, ty_args)) = impl_table_key(ty) else {
            return false;
        };
        self.env
            .type_satisfies_trait(&ty_name, &ty_args, trait_name)
    }

    /// Walk a where-clause and discharge each `ConstPredicate(expr)`
    /// against the resolved const-args (const generics slice 3c).
    /// Substitutes `Identifier(name)` references in the predicate with
    /// `Integer(value)` literals from `const_arg_subst`, then evaluates
    /// via `eval_const_expr` against `Type::Bool`. Emits a focused
    /// `"const constraint violated"` diagnostic on `Bool(false)`; other
    /// eval errors propagate via the existing `emit_const_eval_error`.
    fn discharge_const_predicates(
        &mut self,
        where_clause: &WhereClause,
        const_arg_subst: &HashMap<String, i64>,
        discharge_span: &Span,
    ) {
        for constraint in &where_clause.constraints {
            let WhereConstraint::ConstPredicate { expr, .. } = constraint else {
                continue;
            };
            let substituted = substitute_const_idents_in_expr(expr, const_arg_subst);
            match self.eval_const_expr(&substituted, &Type::Bool) {
                Ok(crate::prelude::ConstValue::Bool(true)) => {}
                Ok(crate::prelude::ConstValue::Bool(false)) => {
                    let bindings_summary: Vec<String> = const_arg_subst
                        .iter()
                        .map(|(n, v)| format!("{}={}", n, v))
                        .collect();
                    let bindings_str = if bindings_summary.is_empty() {
                        String::new()
                    } else {
                        format!(" with {}", bindings_summary.join(", "))
                    };
                    self.type_error(
                        format!(
                            "const constraint violated: predicate is false{}",
                            bindings_str
                        ),
                        discharge_span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Ok(_) => {
                    // Non-Bool result — the predicate expression isn't a
                    // boolean test. Slice 2's evaluator routes type
                    // mismatches through ConstEvalError, but the
                    // surface here is "predicate must return bool" —
                    // skip silently (slice 2's per-operator checks
                    // already surfaced any type errors).
                }
                Err(e) => self.emit_const_eval_error(e),
            }
        }
    }

    /// Validate `args` against a concrete `FunctionSig`. Used by the
    /// expected-type bare-call dispatch when the target is a concrete type and
    /// the impl's stored signature is the source of truth (no Self
    /// substitution needed).
    fn validate_args_against_sig(
        &mut self,
        name: &str,
        sig: &FunctionSig,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        if args.len() != sig.params.len() {
            self.type_error(
                format!(
                    "associated function '{}' expects {} argument(s), found {}",
                    name,
                    sig.params.len(),
                    args.len()
                ),
                span.clone(),
                TypeErrorKind::WrongNumberOfArgs,
            );
            for arg in args {
                self.infer_expr(&arg.value);
            }
            return sig.return_type.clone();
        }
        for (arg, param_ty) in args.iter().zip(sig.params.iter()) {
            let arg_ty = self.infer_expr(&arg.value);
            self.check_assignable(param_ty, &arg_ty, arg.value.span.clone());
        }
        sig.return_type.clone()
    }

    fn try_apply_into_coercion(&mut self, expr: &Expr, expected: &Type) -> Option<Type> {
        let ExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        if method != "into" || !args.is_empty() {
            return None;
        }
        let target_name = match expected {
            Type::Named { name, .. } => name.clone(),
            Type::Int(_) | Type::UInt(_) | Type::Float(_) | Type::Bool | Type::Char | Type::Str => {
                type_display(expected)
            }
            _ => return None,
        };
        let src_ty = self.infer_expr(object);
        if src_ty == Type::Error {
            self.record_expr_type(&expr.span, &Type::Error);
            return Some(Type::Error);
        }
        if self
            .env
            .find_from_impl(&src_ty, &target_name, &[])
            .is_some()
        {
            self.into_conversions
                .insert(SpanKey::from_span(&expr.span), target_name);
            self.record_expr_type(&expr.span, expected);
            return Some(expected.clone());
        }
        self.type_error(
            format!(
                "no `impl From[{}] for {}` is in scope; cannot `.into()`",
                type_display(&src_ty),
                target_name
            ),
            expr.span.clone(),
            TypeErrorKind::TypeMismatch,
        );
        self.record_expr_type(&expr.span, &Type::Error);
        Some(Type::Error)
    }

    /// Recognize `x.try_into()` at an expected `Result[Target, _]` position.
    /// Mirrors `try_apply_into_coercion` with one twist: the target type is
    /// `Result.args[0]`, not the bare expected type. On a hit (matching
    /// `impl TryFrom[S] for Target`), records the rewrite span in
    /// `try_into_conversions` and returns the expected `Result[Target, E]`.
    /// On a miss, emits a "no `impl TryFrom[S] for T`" diagnostic and returns
    /// `Type::Error`. Returns `None` (caller falls through) when the
    /// expression isn't a zero-arg `.try_into()` call or when the expected
    /// type isn't `Result[_, _]`.
    fn try_apply_tryinto_coercion(&mut self, expr: &Expr, expected: &Type) -> Option<Type> {
        let ExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        if method != "try_into" || !args.is_empty() {
            return None;
        }
        // Expected must be `Result[Target, _]`. Extract Target.
        let target_ty = match expected {
            Type::Named { name, args } if name == "Result" && args.len() == 2 => &args[0],
            _ => return None,
        };
        let target_name = match target_ty {
            Type::Named { name, .. } => name.clone(),
            Type::Int(_) | Type::UInt(_) | Type::Float(_) | Type::Bool | Type::Char | Type::Str => {
                type_display(target_ty)
            }
            _ => return None,
        };
        let src_ty = self.infer_expr(object);
        if src_ty == Type::Error {
            self.record_expr_type(&expr.span, &Type::Error);
            return Some(Type::Error);
        }
        if self
            .env
            .find_tryfrom_impl(&src_ty, &target_name, &[])
            .is_some()
        {
            self.try_into_conversions
                .insert(SpanKey::from_span(&expr.span), target_name);
            self.record_expr_type(&expr.span, expected);
            return Some(expected.clone());
        }
        self.type_error(
            format!(
                "no `impl TryFrom[{}] for {}` is in scope; cannot `.try_into()`",
                type_display(&src_ty),
                target_name
            ),
            expr.span.clone(),
            TypeErrorKind::TypeMismatch,
        );
        self.record_expr_type(&expr.span, &Type::Error);
        Some(Type::Error)
    }

    fn infer_expr(&mut self, expr: &Expr) -> Type {
        let ty = self.infer_expr_inner(expr);
        self.record_expr_type(&expr.span, &ty);
        ty
    }

    fn infer_expr_inner(&mut self, expr: &Expr) -> Type {
        match &expr.kind {
            // Literals
            ExprKind::Integer(_, sfx) => self.type_from_int_suffix(*sfx, expr.span.clone()),
            ExprKind::Float(_, sfx) => Self::type_from_float_suffix(*sfx),
            ExprKind::CharLit(_) => Type::Char,
            ExprKind::StringLit(_) | ExprKind::MultiStringLit(_) => Type::Str,
            ExprKind::InterpolatedStringLit(parts) => {
                for part in parts {
                    if let ParsedInterpolationPart::Expr(inner_expr) = part {
                        let ty = self.infer_expr(inner_expr);
                        if ty != Type::Error && !self.type_supports_display(&ty) {
                            self.type_error(
                                format!(
                                    "type '{}' does not implement Display; \
                                     cannot interpolate in f-string",
                                    type_display(&ty)
                                ),
                                inner_expr.span.clone(),
                                TypeErrorKind::TraitBoundNotSatisfied,
                            );
                        }
                    }
                }
                Type::Str
            }
            ExprKind::Bool(_) => Type::Bool,

            // Identifiers
            ExprKind::Identifier(name) => self.resolve_identifier_type(name, &expr.span),
            ExprKind::Path { segments, .. } => self.resolve_path_type(segments, &expr.span),

            ExprKind::SelfValue => self.current_self_type.clone().unwrap_or(Type::Error),
            ExprKind::SelfType => self.current_self_type.clone().unwrap_or(Type::Error),

            // Operators
            ExprKind::Binary { op, left, right } => self.infer_binary(op, left, right, &expr.span),
            ExprKind::Pipe { left, right } => self.infer_pipe(left, right, &expr.span),
            ExprKind::Unary { op, operand } => self.infer_unary(op, operand, &expr.span),

            // Postfix
            ExprKind::Question(inner) => {
                if self.in_defer {
                    self.type_error(
                        "'?' operator is not allowed inside defer/errdefer blocks".to_string(),
                        expr.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                self.infer_question(inner, &expr.span)
            }

            ExprKind::OptionalChain { object, .. } => {
                let _obj_ty = self.infer_expr(object);
                Type::Error // Needs advanced option handling, stubbed for now
            }

            // Infix
            ExprKind::NilCoalesce { left, right } => {
                let l_ty = self.infer_expr(left);
                let r_ty = self.infer_expr(right);
                if l_ty != Type::Error && r_ty != Type::Error {
                    if let Type::Named { name, args } = &l_ty {
                        if name == "Option" && args.len() == 1 {
                            self.check_assignable(&args[0], &r_ty, right.span.clone());
                            return args[0].clone();
                        }
                    }
                }
                Type::Error
            }

            ExprKind::Call { callee, args } => self.infer_call(callee, args, &expr.span),

            ExprKind::MethodCall {
                object,
                method,
                args,
                turbofish: _,
            } => self.infer_method_call(object, method, args, &expr.span),

            ExprKind::FieldAccess { object, field } => {
                self.infer_field_access(object, field, &expr.span)
            }

            ExprKind::TupleIndex { object, index } => {
                let obj_ty = self.infer_expr(object);
                match &obj_ty {
                    Type::Tuple(types) => {
                        let idx = *index as usize;
                        if idx < types.len() {
                            types[idx].clone()
                        } else {
                            self.type_error(
                                format!(
                                    "tuple index {} out of bounds for tuple of length {}",
                                    idx,
                                    types.len()
                                ),
                                expr.span.clone(),
                                TypeErrorKind::InvalidTupleIndex,
                            );
                            Type::Error
                        }
                    }
                    Type::Error => Type::Error,
                    _ => {
                        self.type_error(
                            format!("tuple index on non-tuple type '{}'", type_display(&obj_ty)),
                            expr.span.clone(),
                            TypeErrorKind::InvalidTupleIndex,
                        );
                        Type::Error
                    }
                }
            }

            ExprKind::Index { object, index } => {
                let obj_ty = self.infer_expr(object);
                let idx_ty = self.infer_expr(index);
                let is_range_idx = matches!(&idx_ty, Type::Named { name, .. }
                    if matches!(name.as_str(), "Range" | "RangeInclusive" | "RangeFrom"
                        | "RangeTo" | "RangeToInclusive" | "RangeFull"));
                if !is_integer(&idx_ty) && !is_range_idx && idx_ty != Type::Error {
                    self.type_error(
                        format!(
                            "index must be an integer or range, found '{}'",
                            type_display(&idx_ty)
                        ),
                        index.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if is_range_idx {
                    // Range indexing: `collection[a..b]` → `Slice[T]` where T
                    // is the element type of the indexed collection. See
                    // design.md § Slices and § Subscript Trait.
                    let element_ty = match &obj_ty {
                        Type::Array { element, .. } => Some(*element.clone()),
                        Type::Slice { element, .. } => Some(*element.clone()),
                        Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                            Some(args[0].clone())
                        }
                        Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                            Type::Array { element, .. } => Some(*element.clone()),
                            Type::Slice { element, .. } => Some(*element.clone()),
                            Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                                Some(args[0].clone())
                            }
                            _ => None,
                        },
                        Type::Error => return Type::Error,
                        _ => None,
                    };
                    return match element_ty {
                        Some(el) => Type::Slice {
                            element: Box::new(el),
                            mutable: false,
                        },
                        None => {
                            self.type_error(
                                format!(
                                    "range indexing requires a Vec, Array, or Slice; found '{}'",
                                    type_display(&obj_ty)
                                ),
                                expr.span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            Type::Error
                        }
                    };
                }
                match &obj_ty {
                    Type::Array { element, .. } => *element.clone(),
                    Type::Slice { element, .. } => *element.clone(),
                    Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                        args[0].clone()
                    }
                    Type::Error => Type::Error,
                    _ => Type::Error,
                }
            }

            // Compound
            ExprKind::Block(block) => self.infer_block(block),

            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                let cond_ty = self.infer_expr(condition);
                if cond_ty != Type::Bool && cond_ty != Type::Error {
                    self.type_error(
                        format!(
                            "condition must be 'bool', found '{}'",
                            type_display(&cond_ty)
                        ),
                        condition.span.clone(),
                        TypeErrorKind::ConditionNotBool,
                    );
                }
                let then_ty = self.infer_block(then_block);
                if let Some(ref else_expr) = else_branch {
                    let else_ty = self.infer_expr(else_expr);
                    if then_ty == Type::Never {
                        return else_ty;
                    }
                    if else_ty == Type::Never {
                        return then_ty;
                    }
                    if !types_compatible(&then_ty, &else_ty)
                        && then_ty != Type::Error
                        && else_ty != Type::Error
                    {
                        self.type_error(
                            format!(
                                "if/else branches have incompatible types: '{}' and '{}'",
                                type_display(&then_ty),
                                type_display(&else_ty)
                            ),
                            expr.span.clone(),
                            TypeErrorKind::BranchTypeMismatch,
                        );
                    }
                    then_ty
                } else {
                    Type::Unit
                }
            }

            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                let scrut_ty = self.infer_expr(value);
                // Bind the pattern's variables for the duration of the
                // then-block so identifier-leaf bindings inside if-let
                // (e.g. `if let Some(l) = cur.left { queue.push_back(l); }`)
                // get the right scrutinee-derived type. Without this the
                // pattern's bindings stay un-typed (silent fall-through
                // to `Type::Error`), which breaks downstream
                // `pattern_binding_types` recording, codegen's
                // `var_type_names` propagation, and method dispatch.
                let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
                let dispatch_ty = dispatch_ty.clone();
                self.local_scope.push();
                self.check_pattern_against(pattern, &dispatch_ty, mode);
                let then_ty = self.infer_block(then_block);
                self.local_scope.pop();
                if let Some(ref else_expr) = else_branch {
                    let else_ty = self.infer_expr(else_expr);
                    if then_ty == Type::Never {
                        return else_ty;
                    }
                    if else_ty == Type::Never {
                        return then_ty;
                    }
                    if !types_compatible(&then_ty, &else_ty)
                        && then_ty != Type::Error
                        && else_ty != Type::Error
                    {
                        self.type_error(
                            format!(
                                "if let/else branches have incompatible types: '{}' and '{}'",
                                type_display(&then_ty),
                                type_display(&else_ty)
                            ),
                            expr.span.clone(),
                            TypeErrorKind::BranchTypeMismatch,
                        );
                    }
                    then_ty
                } else {
                    Type::Unit
                }
            }

            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms, &expr.span),

            ExprKind::While {
                condition, body, ..
            } => {
                let cond_ty = self.infer_expr(condition);
                if cond_ty != Type::Bool && cond_ty != Type::Error {
                    self.type_error(
                        format!(
                            "while condition must be 'bool', found '{}'",
                            type_display(&cond_ty)
                        ),
                        condition.span.clone(),
                        TypeErrorKind::ConditionNotBool,
                    );
                }
                self.infer_block(body);
                Type::Unit
            }

            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                let iter_ty = self.infer_expr(iterable);
                self.local_scope.push();
                // Resolve element type via IntoIterator.Item (impl_assoc_types),
                // covering Vec, Map, SortedSet, Set, Slice, Array, Range* and
                // any user type that has registered an "Item" assoc binding.
                let elem_ty = self.element_type_of(&iter_ty);
                self.bind_pattern_types(pattern, &elem_ty);
                for stmt in &body.stmts {
                    self.check_stmt(stmt);
                }
                if let Some(ref final_expr) = body.final_expr {
                    self.infer_expr(final_expr);
                }
                self.local_scope.pop();
                Type::Unit
            }

            ExprKind::Loop { body, .. } => {
                self.infer_block(body);
                Type::Never
            }

            ExprKind::LabeledBlock { label, body, .. } => {
                // LB3 — push a fresh per-label collector frame, infer the
                // body's tail type, pop the frame, and compute the block's
                // type as the LUB of `tail_type` and the collected
                // `break label expr` value types.
                self.break_value_types.push((label.clone(), Vec::new()));
                let tail_ty = self.infer_block(body);
                let frame = self
                    .break_value_types
                    .pop()
                    .map(|(_, v)| v)
                    .unwrap_or_default();
                lub_block_type(tail_ty, &frame)
            }

            ExprKind::Closure {
                params,
                capture_mode,
                prefix_span: _,
                body,
            } => {
                // Round 12.44 (Step 2) — once-callability inference at construction.
                // Snapshot the OUTER local scope before pushing the closure's
                // own param scope so the body walker can identify which
                // identifiers refer to outer bindings (captures).
                let outer_bindings = self.flatten_local_scope_snapshot();
                let closure_param_names: Vec<String> = params
                    .iter()
                    .flat_map(|p| p.pattern.binding_names())
                    .collect();
                // LB4 — closure-boundary rule for the LUB collector. A
                // `break label` inside a closure body cannot target an
                // enclosing labeled block (the resolver rejects it as
                // `undefined loop label`), but we still save/restore the
                // collector stack defensively so an inner labeled-block
                // frame doesn't leak across closure bodies if the
                // resolver's check is bypassed (e.g., during
                // single-phase typechecker tests). Closure bodies start
                // with a fresh empty stack; restored on exit.
                let saved_break_values = std::mem::take(&mut self.break_value_types);
                self.local_scope.push();
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|p| {
                        let ty =
                            p.ty.as_ref()
                                .map(|t| self.lower_type_expr(t, &[]))
                                .unwrap_or_else(|| self.env.fresh_type_var());
                        if !self.is_irrefutable_pattern(&p.pattern, &ty) {
                            self.type_error(
                                "refutable pattern in closure parameter; use `if let` or `match` for patterns that may not match".to_string(),
                                p.pattern.span.clone(),
                                TypeErrorKind::RefutablePattern,
                            );
                        }
                        self.bind_pattern_types(&p.pattern, &ty);
                        ty
                    })
                    .collect();
                let body_ty = self.infer_expr(body);
                self.local_scope.pop();
                self.break_value_types = saved_break_values;
                self.closure_type_with_capture_inference(
                    &expr.span,
                    *capture_mode,
                    &closure_param_names,
                    body,
                    &outer_bindings,
                    param_types,
                    body_ty,
                )
            }

            ExprKind::Return(inner) => {
                if let Some(ref expr) = inner {
                    if let Some(ref ret_ty) = self.current_return_type.clone() {
                        self.check_expr(expr, ret_ty);
                    } else {
                        self.infer_expr(expr);
                    }
                } else if let Some(ref ret_ty) = self.current_return_type.clone() {
                    if *ret_ty != Type::Unit && *ret_ty != Type::Error {
                        self.type_error(
                            format!("expected return value of type '{}'", type_display(ret_ty)),
                            expr.span.clone(),
                            TypeErrorKind::ReturnTypeMismatch,
                        );
                    }
                }
                Type::Never
            }

            ExprKind::Break { label, value } => {
                let val_ty = if let Some(ref e) = value {
                    self.infer_expr(e)
                } else {
                    Type::Unit
                };
                // LB3 — feed the per-label LUB collector for labeled
                // blocks. Find the matching frame by label name (innermost
                // wins) and append the value type. Unlabeled `break`s
                // and breaks targeting a labeled loop have no matching
                // collector frame and are ignored here — loops keep
                // their `Type::Never`-by-default behavior.
                if let Some(name) = label {
                    if let Some(frame) = self
                        .break_value_types
                        .iter_mut()
                        .rev()
                        .find(|(n, _)| n == name)
                    {
                        frame.1.push(val_ty);
                    }
                }
                Type::Never
            }
            ExprKind::Continue { .. } => Type::Never,

            ExprKind::Tuple(exprs) => {
                let types: Vec<Type> = exprs.iter().map(|e| self.infer_expr(e)).collect();
                Type::Tuple(types)
            }

            ExprKind::StructLiteral {
                path,
                fields,
                spread,
            } => {
                if let Some(ref spread_expr) = spread {
                    self.infer_expr(spread_expr);
                }
                self.infer_struct_literal(path, fields, &expr.span)
            }

            ExprKind::Cast { expr: inner, ty } => {
                let from_ty = self.infer_expr(inner);
                let to_ty = self.lower_type_expr(ty, &[]);
                self.check_cast_pair(&from_ty, &to_ty, &inner.span);
                to_ty
            }

            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let start_ty = start.as_deref().map(|e| self.infer_expr(e));
                let end_ty = end.as_deref().map(|e| self.infer_expr(e));
                // When both bounds are present, verify they share a type.
                if let (Some(ref s), Some(ref e)) = (&start_ty, &end_ty) {
                    if !types_compatible(s, e) && *s != Type::Error && *e != Type::Error {
                        self.type_error(
                            format!(
                                "range bounds must have same type: '{}' and '{}'",
                                type_display(s),
                                type_display(e)
                            ),
                            expr.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                // Synthesise the appropriate Range variant.
                let elem_ty = start_ty.or(end_ty).unwrap_or(Type::Int(IntSize::I64));
                let name = match (start.is_some(), end.is_some(), inclusive) {
                    (true, true, false) => "Range",
                    (true, true, true) => "RangeInclusive",
                    (true, false, _) => "RangeFrom",
                    (false, true, false) => "RangeTo",
                    (false, true, true) => "RangeToInclusive",
                    (false, false, _) => "RangeFull",
                };
                if name == "RangeFull" {
                    Type::Named {
                        name: "RangeFull".to_string(),
                        args: vec![],
                    }
                } else {
                    Type::Named {
                        name: name.to_string(),
                        args: vec![elem_ty],
                    }
                }
            }

            ExprKind::Unsafe(block) => self.infer_block(block),

            ExprKind::Try(block) => {
                // v1 stub — typechecker pipeline (?-retargeting against
                // the block, error-type unification, From-chain coercion)
                // lands in P1 per design.md § Error Handling > Try Blocks.
                // We still type-check inner expressions so unrelated
                // errors inside the body still surface; the block's
                // overall type is the error sentinel.
                self.infer_block(block);
                self.type_error(
                    "error[E_TRY_BLOCK_NOT_IMPLEMENTED_YET]: try block syntax \
                     is recognized but the typechecker pipeline lands in P1 \
                     — extract the body into a helper function returning \
                     Result for now"
                        .to_string(),
                    expr.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }

            ExprKind::WhileLet { value, body, .. } => {
                self.infer_expr(value);
                self.infer_block(body);
                Type::Unit
            }

            ExprKind::Seq(block) => self.infer_block(block),
            ExprKind::Par(block) => self.infer_block(block),

            ExprKind::Lock { body, .. } => self.infer_block(body),

            ExprKind::Providers { bindings, body } => {
                // Provider values are plain expressions; infer their types
                // for side effects (diagnostics, subexpression typing). The
                // block's type is the body's type. Full provider-trait
                // conformance — verifying each provider implements the
                // resource's declared `ProviderTrait` — is deferred along
                // with the `Send + Sync` auto-trait enforcement tracked at
                // `docs/deferred.md § Send + Sync Enforcement on
                // with_provider Concrete Provider Type`.
                for b in bindings {
                    self.infer_expr(&b.value);
                }
                self.infer_block(body)
            }

            ExprKind::ArrayLiteral(elements) => {
                // Bare `[...]` defaults to `Vec[T]` in synthesis mode.
                // Use check_expr when an Array annotation is present (handled in check_expr).
                if elements.is_empty() {
                    Type::Named {
                        name: "Vec".to_string(),
                        args: vec![Type::Error],
                    }
                } else {
                    let first_ty = self.infer_expr(&elements[0]);
                    for elem in &elements[1..] {
                        let elem_ty = self.infer_expr(elem);
                        self.check_assignable(&first_ty, &elem_ty, elem.span.clone());
                    }
                    Type::Named {
                        name: "Vec".to_string(),
                        args: vec![first_ty],
                    }
                }
            }

            ExprKind::PrefixCollectionLiteral { type_name, items } => {
                // Empty prefix-literal in synthesis mode — no element type
                // to infer. Check-mode (`let v: Vec[T] = Vec[]`, typed call
                // arguments, typed struct-field initializers) intercepts
                // earlier in `check_expr` and recovers via the expected
                // type. Anything that reaches this branch had no annotation
                // and gets the focused
                // `E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION` diagnostic per
                // design.md § Collection Literals.
                if items.is_empty() {
                    self.report_empty_prefix_literal(type_name, &expr.span);
                    return match type_name.as_str() {
                        "Array" => Type::Array {
                            element: Box::new(Type::Error),
                            size: ConstArg::Literal(0),
                        },
                        _ => Type::Named {
                            name: type_name.clone(),
                            args: vec![Type::Error],
                        },
                    };
                }
                match type_name.as_str() {
                    "Array" => {
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            let ty = self.infer_expr(item);
                            self.check_assignable(&first_ty, &ty, item.span.clone());
                        }
                        Type::Array {
                            element: Box::new(first_ty),
                            size: ConstArg::Literal(items.len() as i64),
                        }
                    }
                    "Vec" => {
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            let ty = self.infer_expr(item);
                            self.check_assignable(&first_ty, &ty, item.span.clone());
                        }
                        Type::Named {
                            name: "Vec".to_string(),
                            args: vec![first_ty],
                        }
                    }
                    "Set" => {
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            let ty = self.infer_expr(item);
                            self.check_assignable(&first_ty, &ty, item.span.clone());
                        }
                        Type::Named {
                            name: "Set".to_string(),
                            args: vec![first_ty],
                        }
                    }
                    other => {
                        // Map's `Map[k: v, ...]` form goes through
                        // `ExprKind::MapLiteral` separately; this arm
                        // catches future prefix-literal types and the
                        // `Map[v1, v2, ...]` (positional-only, no `:`) shape
                        // — which the parser does not emit today but is
                        // future-compatible.
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            self.infer_expr(item);
                        }
                        Type::Named {
                            name: other.to_string(),
                            args: vec![first_ty],
                        }
                    }
                }
            }

            ExprKind::RepeatLiteral {
                type_name,
                value,
                count,
            } => {
                let elem_ty = self.infer_expr(value);
                let count_ty = self.infer_expr(count);
                // Count must be an integer type; report otherwise but keep going.
                let count_is_int = matches!(count_ty, Type::Int(_) | Type::UInt(_) | Type::Error);
                if !count_is_int {
                    self.type_error(
                        format!(
                            "repeat-literal count must be an integer, found '{}'",
                            type_display(&count_ty)
                        ),
                        count.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                match type_name.as_deref() {
                    Some("Array") => {
                        // `Array[v; n]` requires a compile-time integer literal.
                        let size = match &count.kind {
                            ExprKind::Integer(n, _) if *n >= 0 => *n as usize,
                            _ => {
                                self.type_error(
                                    "Array[v; n] requires n to be a non-negative integer literal"
                                        .to_string(),
                                    count.span.clone(),
                                    TypeErrorKind::TypeMismatch,
                                );
                                0
                            }
                        };
                        Type::Array {
                            element: Box::new(elem_ty),
                            size: ConstArg::Literal(size as i64),
                        }
                    }
                    None | Some("Vec") => {
                        // Bare `[v; n]` defaults to `Vec[T]` in synthesis mode
                        // (check_expr coerces against `Array[T, N]` when an
                        // array annotation is present).
                        Type::Named {
                            name: "Vec".to_string(),
                            args: vec![elem_ty],
                        }
                    }
                    Some(other) => {
                        self.type_error(
                            format!(
                                "{}[v; n] is not supported; repeat literals only apply to `Vec` and `Array`",
                                other
                            ),
                            expr.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        Type::Error
                    }
                }
            }

            ExprKind::MapLiteral(entries) => {
                let (first_key, first_val) = &entries[0];
                let key_ty = self.infer_expr(first_key);
                let val_ty = self.infer_expr(first_val);
                for (k, v) in &entries[1..] {
                    let kt = self.infer_expr(k);
                    let vt = self.infer_expr(v);
                    self.check_assignable(&key_ty, &kt, k.span.clone());
                    self.check_assignable(&val_ty, &vt, v.span.clone());
                }
                Type::Named {
                    name: "HashMap".to_string(),
                    args: vec![key_ty, val_ty],
                }
            }

            ExprKind::PipePlaceholder => {
                self.type_error(
                    "'_' placeholder is only valid inside a pipe expression argument list"
                        .to_string(),
                    expr.span.clone(),
                    TypeErrorKind::InvalidPipePlaceholder,
                );
                Type::Error
            }

            ExprKind::OffsetOf { ty, field_path } => {
                self.infer_offset_of(ty, field_path, &expr.span)
            }

            ExprKind::Error => Type::Error,
        }
    }

    /// Type-check `offset_of[T](field.path)`. Per `design.md § Field
    /// Offsets`, the target type must be a struct (concrete or
    /// generic-with-fully-resolved args); opaque foreign types and
    /// generic type parameters are rejected at the first segment.
    /// Each path segment must name a field of the type at the previous
    /// segment's resolved type. Returns `usize` (also `Type::Error` on
    /// failure for downstream tolerance).
    fn infer_offset_of(&mut self, ty: &TypeExpr, field_path: &[String], span: &Span) -> Type {
        let usize_ty = Type::UInt(UIntSize::Usize);
        // Lower the target with `parent_is_ref = true` so the slice-1b
        // walker doesn't fire E_OPAQUE_TYPE_REQUIRES_INDIRECTION; this
        // intrinsic emits E_OFFSET_OF_OPAQUE_TYPE instead.
        let resolved = self.lower_type_expr_inner(ty, &[], true);
        let (mut current_struct_name, _initial_args) = match &resolved {
            Type::Named { name, args } => (name.clone(), args.clone()),
            // Per design.md, generic type-parameter targets are rejected:
            // the typechecker can't see a layout without a concrete
            // instantiation. `Type::TypeParam` and other non-Named
            // shapes route here.
            Type::TypeParam(name) => {
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_GENERIC_PARAM]: offset_of requires a \
                         concrete type; the type parameter '{name}' is not \
                         resolvable to a layout at this call site"
                    ),
                    ty.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
            _ => {
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_NON_STRUCT_TARGET]: offset_of requires a \
                         struct target; got '{}'",
                        type_display(&resolved)
                    ),
                    ty.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };
        if self.env.opaque_foreign_types.contains(&current_struct_name) {
            self.type_error(
                format!(
                    "error[E_OFFSET_OF_OPAQUE_TYPE]: offset_of cannot be applied to \
                     opaque foreign type '{current_struct_name}'; the type's layout \
                     is unknown to Kāra"
                ),
                ty.span.clone(),
                TypeErrorKind::TypeMismatch,
            );
            return Type::Error;
        }
        if field_path.is_empty() {
            self.type_error(
                "error[E_OFFSET_OF_INVALID_PATH]: offset_of requires at least \
                 one field-name segment"
                    .to_string(),
                span.clone(),
                TypeErrorKind::WrongNumberOfArgs,
            );
            return Type::Error;
        }
        // Walk each segment, validating membership in the current struct's
        // declared field set and chasing the field's type for the next
        // segment. At each segment, the current struct is looked up by
        // name in `env.structs`; if absent (e.g., the surface type is an
        // enum or a primitive), `E_OFFSET_OF_NON_STRUCT_TARGET` fires.
        for (segment_idx, segment_name) in field_path.iter().enumerate() {
            let Some(info) = self.env.structs.get(&current_struct_name).cloned() else {
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_NON_STRUCT_TARGET]: offset_of cannot \
                         walk into '{current_struct_name}'; only struct types \
                         have field offsets"
                    ),
                    ty.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            };
            let mut found = None;
            for (fname, ftype, is_pub) in &info.fields {
                if fname == segment_name {
                    found = Some((ftype.clone(), *is_pub));
                    break;
                }
            }
            let Some((field_ty, is_pub)) = found else {
                let available: Vec<&str> = info.fields.iter().map(|(n, _, _)| n.as_str()).collect();
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_UNKNOWN_FIELD]: type '{current_struct_name}' \
                         has no field '{segment_name}'; available fields are: {}",
                        available.join(", ")
                    ),
                    span.clone(),
                    TypeErrorKind::UndefinedField,
                );
                return Type::Error;
            };
            if !is_pub {
                self.check_cross_module_field_access(&current_struct_name, segment_name, span);
            }
            // If this is the last segment, we're done — return usize.
            if segment_idx + 1 == field_path.len() {
                return usize_ty;
            }
            // Otherwise, the field's type must itself be a struct so the
            // next segment can walk into it.
            current_struct_name = match field_ty {
                Type::Named { name, .. } => name,
                _ => {
                    self.type_error(
                        format!(
                            "error[E_OFFSET_OF_NON_STRUCT_TARGET]: field \
                             '{segment_name}' is not a struct type; cannot walk \
                             further into the offset_of path"
                        ),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    return Type::Error;
                }
            };
        }
        usize_ty
    }

    // ── Identifier Resolution ───────────────────────────────────

    fn resolve_identifier_type(&mut self, name: &str, span: &Span) -> Type {
        // Check local scope first
        if let Some(ty) = self.local_scope.lookup(name) {
            return ty.clone();
        }
        // Check functions
        if let Some(sig) = self.env.functions.get(name) {
            return Type::Function {
                params: sig.params.clone(),
                return_type: Box::new(sig.return_type.clone()),
            };
        }
        // Check constants
        if let Some(ty) = self.env.constants.get(name) {
            return ty.clone();
        }
        // Check enum variants (unit variants used as values; tuple variants
        // as constructor functions). Generic enums thread their declared
        // type parameters through the return type's `args` so call-site
        // inference can solve them (see `infer_call`).
        //
        // **Variant-name shadow rule (Slice F).** Skip variants whose
        // bare name collides with a primitive type name (`String`,
        // `Array`, `Map`, `Set`, etc.) — those identifiers are
        // overwhelmingly used as type/module aliases at the call-site
        // (`String.from(...)`, `Map.new()`, `Vec.new()`), not as
        // variant constructors. Without this skip, declaring an enum
        // like `Json.String(String)` retroactively breaks every
        // pre-existing `String.from("...")` call by routing it through
        // the variant-as-function dispatch instead of the impl
        // resolution. Variants are still reachable through the
        // qualified path form (`Json.String(...)`) — `resolve_path_type`
        // above runs before this fallback and finds them by enum name.
        for (enum_name, enum_info) in &self.env.enums {
            for (variant_name, variant_type) in &enum_info.variants {
                if variant_name == name {
                    if is_prelude_type_or_module_name(name) {
                        continue;
                    }
                    let return_args: Vec<Type> = enum_info
                        .generic_params
                        .iter()
                        .map(|p| Type::TypeParam(p.clone()))
                        .collect();
                    let return_ty = Type::Named {
                        name: enum_name.clone(),
                        args: return_args,
                    };
                    match variant_type {
                        VariantTypeInfo::Unit => return return_ty,
                        VariantTypeInfo::Tuple(fields) => {
                            return Type::Function {
                                params: fields.clone(),
                                return_type: Box::new(return_ty),
                            };
                        }
                        _ => {}
                    }
                }
            }
        }
        // Fallback — likely a name the resolver already handled
        // Return Error silently (resolver already reported it)
        let _ = span;
        Type::Error
    }

    fn resolve_path_type(&mut self, segments: &[String], span: &Span) -> Type {
        if segments.len() == 2 {
            let type_name = &segments[0];
            let member = &segments[1];

            // Check for enum variant. Generic enums thread their declared
            // type parameters through the return type's `args` so call-site
            // inference can solve them (see `infer_call`).
            if let Some(enum_info) = self.env.enums.get(type_name).cloned() {
                for (variant_name, variant_type) in &enum_info.variants {
                    if variant_name == member {
                        let return_args: Vec<Type> = enum_info
                            .generic_params
                            .iter()
                            .map(|p| Type::TypeParam(p.clone()))
                            .collect();
                        let return_ty = Type::Named {
                            name: type_name.clone(),
                            args: return_args,
                        };
                        match variant_type {
                            VariantTypeInfo::Unit => return return_ty,
                            VariantTypeInfo::Tuple(fields) => {
                                return Type::Function {
                                    params: fields.clone(),
                                    return_type: Box::new(return_ty),
                                };
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Check for associated function (from impl). No call-site args
            // context — type_name comes from a Path expression without
            // generic args. Theme-4 conservative: only generic-on-name
            // impls participate; specialized impls (`impl Foo for
            // Bar[i32]`) need an args-aware path-expr lookup that this
            // site doesn't carry.
            for imp in &self.env.impls.clone() {
                if imp.target_type == *type_name && imp.target_args.is_empty() {
                    if let Some(sig) = imp.methods.get(member) {
                        return Type::Function {
                            params: sig.params.clone(),
                            return_type: Box::new(sig.return_type.clone()),
                        };
                    }
                }
            }

            // Module-path free functions registered as "module.fn" in the
            // function table — `process.exit`, `env.args`, `env.var`. The
            // ambient effect-resource methods (`Stdin.read_line`,
            // `FileSystem.write`, …) used to land here too, but the slice-1
            // through slice-3 migration moved every `Type.method` entry into
            // `env.impls` via baked source, so this fallback now only serves
            // module-path free functions.
            let dotted = format!("{}.{}", type_name, member);
            if let Some(sig) = self.env.functions.get(&dotted) {
                return Type::Function {
                    params: sig.params.clone(),
                    return_type: Box::new(sig.return_type.clone()),
                };
            }
        }
        // First segment as identifier
        if let Some(first) = segments.first() {
            return self.resolve_identifier_type(first, span);
        }
        Type::Error
    }

    // ── Binary / Unary Operators ────────────────────────────────

    fn infer_binary(&mut self, op: &BinOp, left: &Expr, right: &Expr, span: &Span) -> Type {
        let left_ty = self.infer_expr(left);
        let right_ty = self.infer_expr(right);

        if left_ty == Type::Error || right_ty == Type::Error {
            return Type::Error;
        }

        // Q4 literal promotion: for arithmetic, comparison, and equality ops,
        // when one operand is a suffix-free numeric literal and the other is a
        // concrete numeric type T, re-record the literal's span with type T so
        // the lowering pass sees a homogeneous pair. `effective_ty` tracks the
        // canonical type for the whole expression after promotion.
        let is_promotable_op = matches!(
            op,
            BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Div
                | BinOp::Mod
                | BinOp::Lt
                | BinOp::LtEq
                | BinOp::Gt
                | BinOp::GtEq
                | BinOp::Eq
                | BinOp::NotEq
        );
        // After promotion these hold the effective operand types seen by the
        // match arms below. Initialised to the inferred types; overwritten when
        // promotion fires.
        let (eff_left_ty, eff_right_ty) = if is_promotable_op {
            let left_is_unsuffixed = matches!(
                &left.kind,
                ExprKind::Integer(_, None) | ExprKind::Float(_, None)
            );
            let right_is_unsuffixed = matches!(
                &right.kind,
                ExprKind::Integer(_, None) | ExprKind::Float(_, None)
            );
            if right_is_unsuffixed && !left_is_unsuffixed && is_numeric(&left_ty) {
                // Float literal cannot be promoted to an integer type.
                let can_promote = !(matches!(&right.kind, ExprKind::Float(_, None))
                    && matches!(left_ty, Type::Int(_) | Type::UInt(_)));
                if can_promote {
                    self.record_expr_type(&right.span, &left_ty);
                    (left_ty.clone(), left_ty.clone())
                } else {
                    (left_ty.clone(), right_ty.clone())
                }
            } else if left_is_unsuffixed && !right_is_unsuffixed && is_numeric(&right_ty) {
                let can_promote = !(matches!(&left.kind, ExprKind::Float(_, None))
                    && matches!(right_ty, Type::Int(_) | Type::UInt(_)));
                if can_promote {
                    self.record_expr_type(&left.span, &right_ty);
                    (right_ty.clone(), right_ty.clone())
                } else {
                    (left_ty.clone(), right_ty.clone())
                }
            } else {
                (left_ty.clone(), right_ty.clone())
            }
        } else {
            (left_ty.clone(), right_ty.clone())
        };
        let left_ty = eff_left_ty;
        let right_ty = eff_right_ty;

        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                if is_numeric(&left_ty) {
                    if !types_compatible(&left_ty, &right_ty) {
                        self.type_error(
                            format!(
                                "expected '{}', found '{}'",
                                type_display(&left_ty),
                                type_display(&right_ty)
                            ),
                            right.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                    left_ty
                } else if self.distinct_type_has_arithmetic(&left_ty) {
                    // Arithmetic on a distinct type: both operands must be the same type.
                    if left_ty != right_ty {
                        self.type_error(
                            format!(
                                "arithmetic on distinct type '{}' requires both operands to have \
                                 the same type, found '{}'",
                                type_display(&left_ty),
                                type_display(&right_ty)
                            ),
                            right.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                    left_ty
                } else {
                    self.type_error(
                        format!(
                            "arithmetic operator requires numeric type, found '{}'",
                            type_display(&left_ty)
                        ),
                        left.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                    Type::Error
                }
            }
            BinOp::Eq | BinOp::NotEq => {
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        format!(
                            "cannot compare '{}' and '{}'",
                            type_display(&left_ty),
                            type_display(&right_ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                } else if !self.type_supports_partial_eq(&left_ty) {
                    self.type_error(
                        format!(
                            "type '{}' does not implement Eq; add #[derive(Eq)] to use == or !=",
                            type_display(&left_ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                Type::Bool
            }
            BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        format!(
                            "cannot compare '{}' and '{}'",
                            type_display(&left_ty),
                            type_display(&right_ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                Type::Bool
            }
            BinOp::And | BinOp::Or => {
                if left_ty != Type::Bool {
                    self.type_error(
                        format!(
                            "logical operator requires 'bool', found '{}'",
                            type_display(&left_ty)
                        ),
                        left.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                if right_ty != Type::Bool {
                    self.type_error(
                        format!(
                            "logical operator requires 'bool', found '{}'",
                            type_display(&right_ty)
                        ),
                        right.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                Type::Bool
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                if !is_integer(&left_ty) {
                    self.type_error(
                        format!(
                            "bitwise operator requires integer type, found '{}'",
                            type_display(&left_ty)
                        ),
                        left.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                    return Type::Error;
                }
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        format!(
                            "expected '{}', found '{}'",
                            type_display(&left_ty),
                            type_display(&right_ty)
                        ),
                        right.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                left_ty
            }
            BinOp::Range | BinOp::RangeInclusive => {
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        "range bounds must have same type".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Type::Named {
                    name: "Range".to_string(),
                    args: vec![left_ty],
                }
            }
        }
    }

    fn infer_unary(&mut self, op: &UnaryOp, operand: &Expr, span: &Span) -> Type {
        let ty = self.infer_expr(operand);
        if ty == Type::Error {
            return Type::Error;
        }

        match op {
            UnaryOp::Neg => {
                if !is_numeric(&ty) && !self.distinct_type_has_arithmetic(&ty) {
                    self.type_error(
                        format!(
                            "unary '-' requires numeric type, found '{}'",
                            type_display(&ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                } else {
                    ty
                }
            }
            UnaryOp::Not => {
                if ty != Type::Bool {
                    self.type_error(
                        format!("unary '!' requires 'bool', found '{}'", type_display(&ty)),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                } else {
                    Type::Bool
                }
            }
            UnaryOp::BitNot => {
                if !is_integer(&ty) {
                    self.type_error(
                        format!(
                            "unary '~' requires integer type, found '{}'",
                            type_display(&ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                } else {
                    ty
                }
            }
            UnaryOp::Deref => match ty {
                Type::Ref(inner) | Type::MutRef(inner) => *inner,
                // Raw-pointer dereference (`*const T` / `*mut T`) typechecks
                // to the pointee type. The operation itself is *unsafe* — the
                // `unsafe_op_in_unsafe_fn` lint (`src/unsafe_lint.rs`) rejects
                // it outside an `unsafe { }` block. Soundness lives at the
                // lint layer, not the type layer, so callers can still reason
                // about the deref's result type.
                Type::Pointer { inner, .. } => *inner,
                _ => {
                    self.type_error(
                        format!(
                            "unary '*' requires 'ref T', 'mut ref T', or a raw pointer \
                             ('*const T' / '*mut T'), found '{}'",
                            type_display(&ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                }
            },
        }
    }

    // ── Function Calls ──────────────────────────────────────────

    /// Const generics slice 1c shared path: dispatch a generic free
    /// function call that carries an explicit generic-args list.
    /// Looks up the function's `FunctionSig`, threads
    /// `(explicit_args, formal_generic_params)` into the call-args
    /// substitution flow so the inference solver pre-binds each
    /// ConstVar / TypeVar to its user-supplied value before
    /// arg-position unification.
    fn infer_explicit_generic_args_call(
        &mut self,
        name: &str,
        explicit_args: &[GenericArg],
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let Some(sig) = self.env.functions.get(name).cloned() else {
            // No matching function — fall through to the bare-identifier
            // dispatch via a synthetic Identifier callee so existing
            // error reporting fires.
            let synthetic = Expr {
                kind: ExprKind::Identifier(name.to_string()),
                span: span.clone(),
            };
            return self.infer_call(&synthetic, args, span);
        };
        if args.len() != sig.params.len() {
            self.type_error(
                format!(
                    "expected {} argument(s), found {}",
                    sig.params.len(),
                    args.len()
                ),
                span.clone(),
                TypeErrorKind::WrongNumberOfArgs,
            );
            for arg in args {
                self.infer_expr(&arg.value);
            }
            return sig.return_type;
        }
        let formal_generic_params = sig.generic_params.clone();
        let where_clause = sig.where_clause.clone();
        self.check_call_args_with_substitution_full(
            args,
            &sig.params,
            &sig.return_type,
            span,
            true,
            Some(explicit_args),
            Some(&formal_generic_params),
            where_clause.as_ref(),
            span,
        )
    }

    /// Type-check a call to a layout-introspection intrinsic
    /// (`size_of[T]()` / `align_of[T]()`). Both share the same shape:
    /// exactly one type argument, no value arguments, returns `usize`.
    /// Per `design.md § Field Offsets`, opaque foreign types are
    /// rejected with `error[E_OPAQUE_TYPE_NO_KNOWN_SIZE]` since their
    /// layout is unknown to Kāra.
    ///
    /// The type argument is lowered via `lower_type_expr_inner(_, _, true)`
    /// so the slice-1b walker's `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`
    /// emission is suppressed — for layout queries, "wrap in `ref T`"
    /// is the wrong remediation hint (`size_of[ref Foo]()` measures
    /// the reference, not Foo). The `parent_is_ref = true` flag is a
    /// minor semantic misnomer here ("opaque is allowed at this leaf
    /// because the caller will check it explicitly"), but reusing the
    /// existing flag keeps `lower_type_expr_inner` from sprouting a
    /// second control parameter.
    fn infer_layout_query_intrinsic(
        &mut self,
        name: &str,
        explicit_args: &[GenericArg],
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        if !args.is_empty() {
            self.type_error(
                format!(
                    "error[E_LAYOUT_QUERY_TAKES_NO_ARGS]: `{name}` takes a type \
                     argument only — call shape is `{name}[T]()`, no value arguments"
                ),
                span.clone(),
                TypeErrorKind::WrongNumberOfArgs,
            );
            for a in args {
                self.infer_expr(&a.value);
            }
        }
        let usize_ty = Type::UInt(UIntSize::Usize);
        let type_arg_expr = match explicit_args {
            [GenericArg::Type(te)] => te,
            _ => {
                self.type_error(
                    format!(
                        "error[E_LAYOUT_QUERY_TYPE_ARG_REQUIRED]: `{name}` requires \
                         exactly one type argument — call shape is `{name}[T]()`"
                    ),
                    span.clone(),
                    TypeErrorKind::WrongNumberOfArgs,
                );
                return usize_ty;
            }
        };
        let resolved = self.lower_type_expr_inner(type_arg_expr, &[], true);
        if let Type::Named {
            name: ref ty_name, ..
        } = resolved
        {
            if self.env.opaque_foreign_types.contains(ty_name) {
                self.type_error(
                    format!(
                        "error[E_OPAQUE_TYPE_NO_KNOWN_SIZE]: `{name}` cannot be \
                         applied to opaque foreign type '{ty_name}'; the type's \
                         size and alignment are unknown to Kāra"
                    ),
                    type_arg_expr.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
            }
        }
        usize_ty
    }

    fn infer_call(&mut self, callee: &Expr, args: &[CallArg], span: &Span) -> Type {
        // Const generics slice 1b + 1c: explicit-generic-args call
        // shapes. Two forms reach here:
        //
        //   1. `Path { segments: [name], generic_args: Some(args) }` —
        //      multi-arg shape `name[T, 4](args)` recognized by the
        //      parser's `lookahead_generic_args_call` (requires a
        //      top-level `,` inside the brackets).
        //   2. `Index { object: Identifier(name), index: literal }` —
        //      single-arg shape `name[8](args)` that the parser
        //      can't disambiguate from `callbacks[0]()`. The Vec-of-
        //      functions case at interpreter:1985 must keep working,
        //      so we only treat as a generic-args call when `name`
        //      resolves to a generic free function in `env.functions`.
        //
        // Both shapes route through `infer_explicit_generic_args_call`,
        // which threads the formal-param names + explicit args into
        // `check_call_args_with_substitution_full` so the inference
        // solver pre-binds each ConstVar / TypeVar to its
        // user-supplied value before arg-position unification.
        // Layout-introspection intrinsics: `size_of[T]()` / `align_of[T]()`.
        // Intercepted before the regular generic-call dispatch so the
        // slice-1b walker's `E_OPAQUE_TYPE_REQUIRES_INDIRECTION` emission
        // on the type argument is suppressed (the "wrap in `ref T`" hint
        // would be misleading for a layout query — `size_of[ref Foo]()`
        // measures the reference, not Foo). The intrinsic emits the
        // focused `E_OPAQUE_TYPE_NO_KNOWN_SIZE` instead. See
        // `runtime/stdlib/intrinsics.kara` for the placeholder
        // declarations and `compile_call` for the codegen counterpart.
        //
        // Two AST shapes reach here. Multi-arg generic calls
        // (`size_of[T, _]()`, never used today but kept symmetric) parse
        // as `Path { generic_args: Some([T]) }` because
        // `lookahead_generic_args_call` requires a top-level comma.
        // Single-arg `size_of[T]()` cannot be disambiguated from
        // `arr[i]()` so it parses as `Call { callee: Index { Ident, T } }`
        // — `T` is a value-position `Expr` that actually denotes a type.
        if let ExprKind::Path {
            segments,
            generic_args: Some(ga),
        } = &callee.kind
        {
            if segments.len() == 1 {
                let name = &segments[0];
                if name == "size_of" || name == "align_of" {
                    return self.infer_layout_query_intrinsic(name, ga, args, span);
                }
            }
        }
        if let ExprKind::Index { object, index } = &callee.kind {
            if let ExprKind::Identifier(name) = &object.kind {
                if name == "size_of" || name == "align_of" {
                    if let Some(te) = expr_as_type_expr(index) {
                        let synth = vec![GenericArg::Type(te)];
                        return self.infer_layout_query_intrinsic(name, &synth, args, span);
                    }
                    self.type_error(
                        format!(
                            "error[E_LAYOUT_QUERY_TYPE_ARG_REQUIRED]: `{name}` requires \
                             a type argument — call shape is `{name}[T]()`"
                        ),
                        callee.span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    return Type::UInt(UIntSize::Usize);
                }
            }
        }

        if let Some((name, explicit_args)) = match &callee.kind {
            ExprKind::Path {
                segments,
                generic_args: Some(ga),
            } if segments.len() == 1 => Some((segments[0].clone(), ga.clone())),
            ExprKind::Index { object, index } if is_literal_const_arg_expr(index) => {
                if let ExprKind::Identifier(name) = &object.kind {
                    if self
                        .env
                        .functions
                        .get(name)
                        .map(|s| !s.generic_params.is_empty())
                        .unwrap_or(false)
                    {
                        Some((name.clone(), vec![GenericArg::Const((**index).clone())]))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        } {
            return self.infer_explicit_generic_args_call(&name, &explicit_args, args, span);
        }

        // Type-parameter associated calls: `T.method(args)` parses as
        // `Call { callee: Path(["T", "method"]), args }`. Intercept this
        // shape before the generic call infrastructure tries to read `T`
        // as a value. Concrete types (`Wrapper.method()`) fall through —
        // `resolve_path_type` already finds their impl methods.
        if let ExprKind::Path { segments, .. } = &callee.kind {
            if segments.len() == 2 {
                if let Some(ty) = self.try_dispatch_typeparam_assoc_fn(
                    &segments[0],
                    &segments[1],
                    &callee.span,
                    args,
                    span,
                ) {
                    return ty;
                }
            }
        }

        // Bare identifier callee that is unresolvable as a value but matches a
        // trait-declared associated function name: the resolver suppressed the
        // undefined-name error for these so the typechecker could dispatch via
        // expected type. We are here because synthesis mode reached `infer_call`
        // — meaning no expected-type slot was available — so emit the
        // "cannot infer type" diagnostic instead of silently returning Error.
        if let ExprKind::Identifier(name) = &callee.kind {
            if self.is_unresolvable_trait_assoc_fn(name) {
                self.type_error(
                    format!(
                        "cannot infer type for associated function call '{}': add a type annotation \
                         (e.g. `let x: T = {}(...)`) or call as `T.{}(...)`",
                        name, name, name,
                    ),
                    span.clone(),
                    TypeErrorKind::CannotInferAssocFn,
                );
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                return Type::Error;
            }
        }

        // Built-in diverging functions: todo() and unreachable()
        // Accept 0 or 1 String argument; return Never (they never return normally).
        if let ExprKind::Identifier(name) = &callee.kind {
            if name == "todo" || name == "unreachable" {
                match args.len() {
                    0 => {}
                    1 => {
                        let arg_ty = self.infer_expr(&args[0].value);
                        if arg_ty != Type::Str && arg_ty != Type::Error {
                            self.type_error(
                                format!(
                                    "{}() message must be a 'str', found '{}'",
                                    name,
                                    type_display(&arg_ty)
                                ),
                                args[0].value.span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                        }
                    }
                    _ => {
                        self.type_error(
                            format!("{}() takes 0 or 1 argument(s), found {}", name, args.len()),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                        for arg in args {
                            self.infer_expr(&arg.value);
                        }
                    }
                }
                return Type::Never;
            }
        }

        // Built-in collection constructors with no syntactic stdlib
        // declaration: `Vec.new()`, `VecDeque.new()`, `Set.new()`,
        // `SortedSet.new()`, `Map.new()`. These are dispatched at runtime
        // by the interpreter / codegen, but the typechecker still needs
        // a meaningful return type at the call site so a downstream
        // `q.push_back(x)` can solve the element typevar (otherwise the
        // binding's type collapses to `Type::Error`, the
        // `pattern_binding_types` / `pattern_binding_inner_types`
        // side-tables stay empty, and codegen's let-statement
        // `vec_elem_types` registration never fires). Returns
        // `Type::Named { name: <coll>, args: [TypeVar(fresh)] }` (or two
        // typevars for `Map`) so the standard inference machinery does
        // the rest. Per design.md § Collections (`Vec.new` / `Map.new`
        // are the canonical constructors).
        if let ExprKind::Path { segments, .. } = &callee.kind {
            if segments.len() == 2 && segments[1] == "new" && args.is_empty() {
                let collection = segments[0].as_str();
                let result_ty = match collection {
                    "Vec" | "VecDeque" | "Set" | "SortedSet" => Some(Type::Named {
                        name: collection.to_string(),
                        args: vec![self.env.fresh_type_var()],
                    }),
                    "Map" => Some(Type::Named {
                        name: "Map".to_string(),
                        args: vec![self.env.fresh_type_var(), self.env.fresh_type_var()],
                    }),
                    _ => None,
                };
                if let Some(ty) = result_ty {
                    self.record_expr_type(span, &ty);
                    return ty;
                }
            }
        }

        // Built-in output functions: println() / print() / eprintln().
        // Accept 0 or 1 Display-implementing argument; return Unit.
        if let ExprKind::Identifier(name) = &callee.kind {
            if name == "println" || name == "print" || name == "eprintln" {
                match args.len() {
                    0 => {}
                    1 => {
                        let arg_ty = self.infer_expr(&args[0].value);
                        if arg_ty != Type::Error && !self.type_supports_display(&arg_ty) {
                            self.type_error(
                                format!(
                                    "{}() argument must implement Display, \
                                     but '{}' does not",
                                    name,
                                    type_display(&arg_ty)
                                ),
                                args[0].value.span.clone(),
                                TypeErrorKind::TraitBoundNotSatisfied,
                            );
                        }
                    }
                    _ => {
                        self.type_error(
                            format!("{}() takes 0 or 1 argument(s), found {}", name, args.len()),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                        for arg in args {
                            self.infer_expr(&arg.value);
                        }
                    }
                }
                return Type::Unit;
            }
        }

        // Look up parameter names for label validation
        let param_names: Option<Vec<Option<String>>> = match &callee.kind {
            ExprKind::Identifier(name) => self
                .env
                .functions
                .get(name)
                .map(|sig| sig.param_names.clone()),
            ExprKind::Path { segments, .. } => segments.last().and_then(|name| {
                self.env
                    .functions
                    .get(name)
                    .map(|sig| sig.param_names.clone())
            }),
            _ => None,
        };

        if let Some(ref names) = param_names {
            self.validate_labels(args, names, span);
        }

        // Const generics slice 3c: look up the callee's where-clause
        // so the regular generic-call dispatch can discharge
        // `ConstPredicate`s against inferred const-args. The
        // explicit-generic-args path (`infer_explicit_generic_args_call`)
        // already threads the where-clause; this branch covers the
        // type-inferred case (`f(arr)` where N is inferred from
        // `arr`'s type).
        let callee_where_clause: Option<WhereClause> = match &callee.kind {
            ExprKind::Identifier(name) => self
                .env
                .functions
                .get(name)
                .and_then(|sig| sig.where_clause.clone()),
            ExprKind::Path { segments, .. } => segments.last().and_then(|name| {
                self.env
                    .functions
                    .get(name)
                    .and_then(|sig| sig.where_clause.clone())
            }),
            _ => None,
        };

        let callee_ty = self.infer_expr(callee);

        match &callee_ty {
            Type::Function {
                params,
                return_type,
            }
            | Type::OnceFunction {
                params,
                return_type,
            } => {
                if args.len() != params.len() {
                    self.type_error(
                        format!(
                            "expected {} argument(s), found {}",
                            params.len(),
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    // Still type-check the args we have
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return *return_type.clone();
                }
                let params = params.clone();
                let return_type = *return_type.clone();
                self.check_call_args_with_substitution_full(
                    args,
                    &params,
                    &return_type,
                    span,
                    /* apply_call_site_marker = */ true,
                    None,
                    None,
                    callee_where_clause.as_ref(),
                    span,
                )
            }
            Type::Error => {
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                Type::Error
            }
            _ => {
                self.type_error(
                    format!("type '{}' is not callable", type_display(&callee_ty)),
                    span.clone(),
                    TypeErrorKind::NotCallable,
                );
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                Type::Error
            }
        }
    }

    // ── Call-Site Mutation Marker (design.md Part 1½) ────────────

    /// Enforces the 1A call-site rule:
    ///   - Fresh binding to `mut ref T` / `mut Slice[T]` param → marker required.
    ///   - Forwarded mut-ref argument → marker not required (accept either).
    ///   - Owned / `ref T` param → marker rejected.
    ///
    /// "Forwarded" is classified by the place-expression root (or the argument's
    /// own type if it is already a mut-ref / mut-slice value — covers nested
    /// mut-ref returns like `other(wrap(mut v))`).
    fn check_call_site_marker(&mut self, arg: &CallArg, param_ty: &Type, arg_ty: &Type) {
        let param_is_mutating = matches!(param_ty, Type::MutRef(_))
            || matches!(param_ty, Type::Slice { mutable: true, .. });

        if !param_is_mutating {
            if arg.mut_marker {
                self.type_error(
                    format!(
                        "`mut` marker is not legal here — parameter expects `{}` \
                         (not a mutable borrow). Remove `mut`.",
                        type_display(param_ty)
                    ),
                    arg.span.clone(),
                    TypeErrorKind::InvalidMutMarker,
                );
            }
            return;
        }

        let forwarded = self.is_arg_forwarded(&arg.value, arg_ty);

        if arg.mut_marker && forwarded {
            // The argument is already a mut-ref (either by type or by
            // place-root) — marking it is redundant and, in the nested
            // mut-ref-return case, actively wrong.
            self.type_error(
                "this argument is already a mut-ref; drop the `mut` marker. \
                 The mutation surface was announced at the callee or enclosing \
                 scope's signature."
                    .to_string(),
                arg.span.clone(),
                TypeErrorKind::InvalidMutMarker,
            );
            return;
        }

        if !arg.mut_marker && !forwarded {
            self.type_error(
                format!(
                    "parameter expects `{}`; call with fresh binding requires \
                     a `mut` marker at this argument to permit the mutation. \
                     Write `mut <expr>`.",
                    type_display(param_ty)
                ),
                arg.span.clone(),
                TypeErrorKind::MissingMutMarker,
            );
        }
    }

    /// An argument is *forwarded* (already a mut-ref handed to this call) if:
    ///   (A) its own inferred type is `mut ref T` / `mut Slice[T]`, or
    ///   (B) it is a place expression whose root binding is typed
    ///       `mut ref T` / `mut Slice[T]` in the current scope.
    /// Otherwise the argument is *fresh* (owned local, temporary, literal,
    /// non-mut-ref call return, etc.).
    fn is_arg_forwarded(&self, expr: &Expr, arg_ty: &Type) -> bool {
        // (A) Argument's own type is already mut-ref / mut-slice.
        if matches!(arg_ty, Type::MutRef(_)) || matches!(arg_ty, Type::Slice { mutable: true, .. })
        {
            return true;
        }
        // (B) Place-expression root is a mut-ref / mut-slice binding.
        self.place_root_is_mut_borrow(expr)
    }

    fn place_root_is_mut_borrow(&self, expr: &Expr) -> bool {
        let mut e = expr;
        loop {
            match &e.kind {
                ExprKind::Identifier(name) => {
                    return matches!(
                        self.local_scope.lookup(name),
                        Some(Type::MutRef(_)) | Some(Type::Slice { mutable: true, .. })
                    );
                }
                ExprKind::SelfValue => {
                    return matches!(
                        self.local_scope.lookup("self"),
                        Some(Type::MutRef(_)) | Some(Type::Slice { mutable: true, .. })
                    );
                }
                ExprKind::FieldAccess { object, .. } => e = object,
                ExprKind::TupleIndex { object, .. } => e = object,
                ExprKind::Index { object, .. } => e = object,
                // Non-place expressions: literal, call, block, binop, etc.
                _ => return false,
            }
        }
    }

    // ── Pipe Desugaring ──────────────────────────────────────────

    fn infer_pipe(&mut self, left: &Expr, right: &Expr, span: &Span) -> Type {
        match &right.kind {
            // a |> f => f(a)
            ExprKind::Identifier(_) | ExprKind::Path { .. } => {
                let synthetic_arg = CallArg {
                    label: None,
                    mut_marker: false,
                    value: left.clone(),
                    span: left.span.clone(),
                };
                self.infer_call(right, &[synthetic_arg], span)
            }

            // a |> f(args...) => f(a, args...) or f(args with _ replaced)
            ExprKind::Call { callee, args } => {
                // Count _ placeholders in args
                let placeholder_count = args
                    .iter()
                    .filter(|arg| matches!(arg.value.kind, ExprKind::PipePlaceholder))
                    .count();

                if placeholder_count > 1 {
                    self.type_error(
                        "at most one '_' placeholder allowed per pipe stage".to_string(),
                        right.span.clone(),
                        TypeErrorKind::InvalidPipePlaceholder,
                    );
                    self.infer_expr(callee);
                    for arg in args {
                        if !matches!(arg.value.kind, ExprKind::PipePlaceholder) {
                            self.infer_expr(&arg.value);
                        }
                    }
                    return Type::Error;
                }

                // Build the desugared argument list
                let desugared_args: Vec<CallArg> = if placeholder_count == 1 {
                    // Replace _ with the left-hand value
                    args.iter()
                        .map(|arg| {
                            if matches!(arg.value.kind, ExprKind::PipePlaceholder) {
                                CallArg {
                                    label: arg.label.clone(),
                                    mut_marker: arg.mut_marker,
                                    value: left.clone(),
                                    span: left.span.clone(),
                                }
                            } else {
                                arg.clone()
                            }
                        })
                        .collect()
                } else {
                    // No placeholder — prepend left as first argument
                    let mut new_args = vec![CallArg {
                        label: None,
                        mut_marker: false,
                        value: left.clone(),
                        span: left.span.clone(),
                    }];
                    new_args.extend(args.iter().cloned());
                    new_args
                };

                self.infer_call(callee, &desugared_args, span)
            }

            _ => {
                self.type_error(
                    "right-hand side of pipe must be a function name or function call".to_string(),
                    right.span.clone(),
                    TypeErrorKind::NotCallable,
                );
                self.infer_expr(right);
                Type::Error
            }
        }
    }

    // ── ? operator ──────────────────────────────────────────────

    /// Type-check `inner?`: validate that the operand is `Result[T, E1]` or
    /// `Option[T]`, that the enclosing function returns a compatible variant,
    /// and (for Result) that error types match exactly or convert via `From`.
    /// Returns the unwrapped success type (`T`).
    fn infer_question(&mut self, inner: &Expr, span: &Span) -> Type {
        let inner_ty = self.infer_expr(inner);
        if inner_ty == Type::Error {
            return Type::Error;
        }

        let (inner_name, inner_args) = match &inner_ty {
            Type::Named { name, args } => (name.clone(), args.clone()),
            _ => {
                self.type_error(
                    format!(
                        "'?' operator requires `Result` or `Option`, found '{}'",
                        type_display(&inner_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };

        let return_ty = match self.current_return_type.clone() {
            Some(t) => t,
            None => {
                self.type_error(
                    "'?' operator used outside a function body".to_string(),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };
        let (ret_name, ret_args) = match &return_ty {
            Type::Named { name, args } => (name.clone(), args.clone()),
            _ => {
                self.type_error(
                    format!(
                        "'?' requires the enclosing function to return `Result` or `Option`, found '{}'",
                        type_display(&return_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };

        match (inner_name.as_str(), ret_name.as_str()) {
            ("Option", "Option") if inner_args.len() == 1 && ret_args.len() == 1 => {
                inner_args[0].clone()
            }
            ("Result", "Result") if inner_args.len() == 2 && ret_args.len() == 2 => {
                let inner_err = &inner_args[1];
                let ret_err = &ret_args[1];
                if inner_err == ret_err {
                    return inner_args[0].clone();
                }
                // Cross-error type: require `impl From[InnerErr] for RetErr`.
                let target_name = match ret_err {
                    Type::Named { name, .. } => name.clone(),
                    _ => {
                        self.type_error(
                            format!(
                                "'?' cannot propagate error '{}' as '{}': target is not a named type",
                                type_display(inner_err),
                                type_display(ret_err)
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        return Type::Error;
                    }
                };
                if self
                    .env
                    .find_from_impl(inner_err, &target_name, &[])
                    .is_some()
                {
                    self.question_conversions
                        .insert(SpanKey::from_span(span), target_name.clone());
                    return inner_args[0].clone();
                }
                self.type_error(
                    format!(
                        "'?' cannot convert error '{}' to '{}': no `impl From[{}] for {}` in scope",
                        type_display(inner_err),
                        type_display(ret_err),
                        type_display(inner_err),
                        target_name
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }
            ("Result", "Option") | ("Option", "Result") => {
                self.type_error(
                    format!(
                        "'?' cannot mix `Result` and `Option`: operand is '{}', function returns '{}'",
                        type_display(&inner_ty),
                        type_display(&return_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }
            _ => {
                self.type_error(
                    format!(
                        "'?' requires operand and return type to be `Result` or `Option`, found '{}' and '{}'",
                        type_display(&inner_ty),
                        type_display(&return_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }
        }
    }

    // ── Method Calls ────────────────────────────────────────────

    /// True when `name` is unresolvable as a value (no local, function,
    /// constant, or builtin), but at least one visible trait declares it as
    /// an associated function. Mirrors the resolver's `is_trait_assoc_fn_name`
    /// suppression rule — used by `infer_call` to surface a "cannot infer"
    /// error in synthesis position rather than silently returning `Type::Error`.
    fn is_unresolvable_trait_assoc_fn(&self, name: &str) -> bool {
        if self.local_scope.lookup(name).is_some()
            || self.env.functions.contains_key(name)
            || self.env.constants.contains_key(name)
            || matches!(
                name,
                "todo" | "unreachable" | "println" | "print" | "eprintln" | "panic"
            )
        {
            return false;
        }
        // Also skip if the name resolves as an enum variant constructor.
        for enum_info in self.env.enums.values() {
            if enum_info.variants.iter().any(|(v, _)| v == name) {
                return false;
            }
        }
        for item in &self.program.items {
            if let Item::TraitDef(t) = item {
                for ti in &t.items {
                    if let TraitItem::Method(m) = ti {
                        if m.name == name && m.self_param.is_none() {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Locate the AST `TraitMethod` declaration for `trait_name.method_name`.
    /// Walks `program.items` looking for a matching `Item::TraitDef`. Returns
    /// `None` if the trait is not declared in the current program (stdlib /
    /// derive-only / built-in traits do not have AST nodes here, so callers
    /// must treat absence as "trait does not declare this method via AST").
    fn find_trait_method<'p>(
        &'p self,
        trait_name: &str,
        method_name: &str,
    ) -> Option<&'p crate::ast::TraitMethod> {
        // User program first (so user-defined traits with the same name
        // shadow stdlib if such a case ever arises — though stdlib trait
        // names are reserved per design.md).
        for item in &self.program.items {
            if let Item::TraitDef(t) = item {
                if t.name == trait_name {
                    for ti in &t.items {
                        if let TraitItem::Method(m) = ti {
                            if m.name == method_name {
                                return Some(m);
                            }
                        }
                    }
                }
            }
        }
        // Baked stdlib (`STDLIB_PROGRAMS`): trait declarations like
        // `Display`, `Iterator`, `Ord`, etc. live here. Walking the
        // baked surface lets `T: Display`-bounded type params resolve
        // their `.to_string()` etc. without requiring user redeclaration.
        // Slice 2 of the method-resolution CR — the receiver-form
        // dispatch path needs this for `T: Display` to find Display's
        // `to_string` method, and the same fix benefits the existing
        // type-prefixed dispatch.
        for (_, program) in crate::prelude::STDLIB_PROGRAMS.iter() {
            for item in &program.items {
                if let Item::TraitDef(t) = item {
                    if t.name == trait_name {
                        for ti in &t.items {
                            if let TraitItem::Method(m) = ti {
                                if m.name == method_name {
                                    return Some(m);
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Attempt to dispatch `T.method(args)` where `T` is a generic type
    /// parameter (resolver records its bounds under the receiver's SymbolId).
    /// `callee_span` is the span of the `Path(["T", "method"])` expression
    /// — the resolver records `T`'s SymbolId there. Returns `Some(return_type)`
    /// when dispatch succeeds, `None` to fall through to the existing
    /// concrete-type / value-receiver paths.
    ///
    /// Multiple bound traits declaring the same method name → ambiguity error
    /// plus `Type::Error`. Exactly one match → lower the trait method's
    /// signature with `Self → Type::TypeParam(type_name)` substitution and
    /// validate args.
    /// Receiver-form complement to [`Self::try_dispatch_typeparam_assoc_fn`].
    /// Slice 2 of the method-resolution CR (see `phase-4-interpreter.md` item
    /// 8). Called from `infer_method_call`'s receiver-type match when the
    /// receiver is `Type::TypeParam(name)`. Looks up `name`'s bounds in
    /// `enclosing_bounds` (populated by `collect_param_bounds`), finds bound
    /// traits that declare a *method* (with `self_param`) of the requested
    /// name, and dispatches.
    ///
    /// Branch on candidate count:
    /// - zero → emit `NoMethodFound` diagnostic, return `Type::Error`.
    /// - one → dispatch via `dispatch_trait_assoc_fn` (which substitutes
    ///   `Self → Type::TypeParam(name)` in the method's signature). The
    ///   trait method's `params` already excludes `self_param` per the
    ///   AST shape, so `args.len()` matches `method.params.len()` — no
    ///   off-by-one for the implicit receiver.
    /// - more → emit `AmbiguousAssocFn` (E0233) listing each candidate
    ///   trait with a UFCS-disambiguation hint.
    ///
    /// Self-mode compatibility (calling a `mut ref self` method on a `ref`
    /// receiver) is the param-binding layer's concern, not this dispatcher's.
    fn dispatch_typeparam_receiver_method(
        &mut self,
        type_param_name: &str,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let bounds = match self.enclosing_bounds.get(type_param_name) {
            Some(b) => b.clone(),
            None => Vec::new(),
        };
        let candidates: Vec<(String, crate::ast::TraitMethod)> = bounds
            .iter()
            .filter_map(|b| b.path.last().cloned())
            .filter_map(|trait_name| {
                let m = self.find_trait_method(&trait_name, method)?;
                // Only methods (with self_param) are receiver-form
                // candidates. Associated functions (no self_param) reach
                // the dispatch only through type-prefixed `T.method()`.
                m.self_param.as_ref()?;
                Some((trait_name, m.clone()))
            })
            .collect();

        match candidates.len() {
            0 => {
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                self.type_error(
                    format!(
                        "no method '{}' on type parameter '{}'; \
                         add a trait bound declaring it (e.g. `{}: SomeTrait`)",
                        method, type_param_name, type_param_name,
                    ),
                    span.clone(),
                    TypeErrorKind::NoMethodFound,
                );
                Type::Error
            }
            1 => {
                let (_trait_name, trait_method) = candidates.into_iter().next().unwrap();
                self.dispatch_trait_assoc_fn(type_param_name, &trait_method, args, span)
            }
            _ => {
                let trait_list = candidates
                    .iter()
                    .map(|(t, _)| format!("`{}`", t))
                    .collect::<Vec<_>>()
                    .join(", ");
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                self.type_error(
                    format!(
                        "ambiguous method '{}' on type parameter '{}': declared by {}. \
                         Use UFCS `Trait.{}(receiver, ...)` to disambiguate.",
                        method, type_param_name, trait_list, method,
                    ),
                    span.clone(),
                    TypeErrorKind::AmbiguousAssocFn,
                );
                Type::Error
            }
        }
    }

    /// Receiver-form `self.method(args)` dispatch inside a trait default
    /// body. Slice 3.5 of the method-resolution CR — see
    /// `phase-4-interpreter.md` item 8. Closes the explicit `name == "Self"`
    /// silent-fallthrough that slice 2 left in place when wiring the
    /// receiver-form `Type::TypeParam` arm.
    ///
    /// Candidates are gathered from the enclosing trait's *own* methods plus
    /// every method on traits in the supertrait closure (filtered to those
    /// declaring a `self_param`, since associated functions reach the
    /// dispatch only through type-prefixed `Type.method()`).
    ///
    /// Branch on candidate count:
    /// - zero → `NoMethodFound` (E0236).
    /// - one → dispatch via `dispatch_trait_assoc_fn` with `target = "Self"`.
    /// - more → `AmbiguousAssocFn` (E0233) listing each declarer with a UFCS
    ///   hint. (Slice 3's `AmbiguousMethod` is for cross-impl ambiguity at
    ///   concrete-receiver sites; the Self-receiver path is closer in shape
    ///   to the type-parameter dispatcher's multi-bound case.)
    ///
    /// Returns `Type::Error` outside a trait body (when `enclosing_trait` is
    /// `None`) so the caller's silent-fallthrough behavior is preserved for
    /// non-trait `Self` cases (impl-method bodies bind `Self` to the impl's
    /// target type via `current_self_type`, a different mechanism).
    fn dispatch_self_receiver_method(
        &mut self,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let trait_name = match self.enclosing_trait.clone() {
            Some(name) => name,
            None => {
                // Not inside a trait body — `Self` here resolves through a
                // different mechanism (impl-method `current_self_type`).
                // Preserve the pre-existing silent fallthrough.
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                return Type::Error;
            }
        };

        // Candidate traits: enclosing trait first, then its supertrait closure.
        let candidate_traits = self.env.supertrait_closure_traits(&trait_name);
        let candidates: Vec<(String, crate::ast::TraitMethod)> = candidate_traits
            .iter()
            .filter_map(|t| {
                let m = self.find_trait_method(t, method)?;
                // Receiver-form requires a self_param.
                m.self_param.as_ref()?;
                Some((t.clone(), m.clone()))
            })
            .collect();

        match candidates.len() {
            0 => {
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                self.type_error(
                    format!(
                        "no method '{}' found on `Self` in trait '{}'; \
                         declare it on the trait or a supertrait",
                        method, trait_name,
                    ),
                    span.clone(),
                    TypeErrorKind::NoMethodFound,
                );
                Type::Error
            }
            1 => {
                let (_t, trait_method) = candidates.into_iter().next().unwrap();
                self.dispatch_trait_assoc_fn("Self", &trait_method, args, span)
            }
            _ => {
                let trait_list = candidates
                    .iter()
                    .map(|(t, _)| format!("`{}`", t))
                    .collect::<Vec<_>>()
                    .join(", ");
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                self.type_error(
                    format!(
                        "ambiguous method '{}' on `Self` in trait '{}': declared by {}. \
                         Use UFCS `Trait.{}(self, ...)` to disambiguate.",
                        method, trait_name, trait_list, method,
                    ),
                    span.clone(),
                    TypeErrorKind::AmbiguousAssocFn,
                );
                Type::Error
            }
        }
    }

    fn try_dispatch_typeparam_assoc_fn(
        &mut self,
        type_name: &str,
        method: &str,
        callee_span: &Span,
        args: &[CallArg],
        call_span: &Span,
    ) -> Option<Type> {
        let span_key = SpanKey::from_span(callee_span);
        let sym_id = self.resolve_result.resolutions.get(&span_key).copied()?;
        let sym = self.resolve_result.symbol_table.get_symbol(sym_id);
        if !matches!(sym.kind, SymbolKind::TypeParam) {
            return None;
        }
        let bounds = self.resolve_result.symbol_table.get_generic_bounds(sym_id);
        let candidates: Vec<String> = bounds
            .iter()
            .filter_map(|b| b.path.last().cloned())
            .filter(|trait_name| self.find_trait_method(trait_name, method).is_some())
            .collect();
        match candidates.len() {
            0 => None,
            1 => {
                let trait_name = candidates[0].clone();
                let trait_method = self.find_trait_method(&trait_name, method)?.clone();
                Some(self.dispatch_trait_assoc_fn(type_name, &trait_method, args, call_span))
            }
            _ => {
                let trait_list = candidates
                    .iter()
                    .map(|c| format!("`{}`", c))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.type_error(
                    format!(
                        "ambiguous associated function '{}' on type parameter '{}': declared by {}. \
                         Use UFCS `Trait.{}(...)` to disambiguate.",
                        method, type_name, trait_list, method,
                    ),
                    call_span.clone(),
                    TypeErrorKind::AmbiguousAssocFn,
                );
                Some(Type::Error)
            }
        }
    }

    /// Lower a trait method's signature with `Self → Type::TypeParam(target)`
    /// substitution, then validate `args` against it. Used for type-parameter
    /// dispatch (`T.method()` where `T: Trait`). The returned type is the
    /// substituted return type; `Unit` for methods with no return.
    fn dispatch_trait_assoc_fn(
        &mut self,
        target: &str,
        method: &crate::ast::TraitMethod,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let mut subs: HashMap<String, SubstValue> = HashMap::new();
        subs.insert(
            "Self".to_string(),
            SubstValue::Type(Type::TypeParam(target.to_string())),
        );

        let mut scope = vec!["Self".to_string()];
        if let Some(ref gp) = method.generic_params {
            scope.extend(gp.params.iter().map(|p| p.name.clone()));
        }

        let param_types: Vec<Type> = method
            .params
            .iter()
            .map(|p| {
                let lowered = self.lower_type_expr(&p.ty, &scope);
                substitute_type_params(&lowered, &subs)
            })
            .collect();

        if args.len() != param_types.len() {
            self.type_error(
                format!(
                    "method '{}' expects {} argument(s), found {}",
                    method.name,
                    param_types.len(),
                    args.len()
                ),
                span.clone(),
                TypeErrorKind::WrongNumberOfArgs,
            );
            for arg in args {
                self.infer_expr(&arg.value);
            }
        } else {
            for (arg, param) in args.iter().zip(param_types.iter()) {
                let arg_ty = self.infer_expr(&arg.value);
                self.check_assignable(param, &arg_ty, arg.value.span.clone());
            }
        }

        let ret = method
            .return_type
            .as_ref()
            .map(|rt| self.lower_type_expr(rt, &scope))
            .unwrap_or(Type::Unit);
        substitute_type_params(&ret, &subs)
    }

    fn infer_method_call(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        // Lowercase stdlib module aliases: `env.args()`, `env.var(name)`.
        // These use lowercase module names (design.md § I/O), distinct from
        // the capitalized resource names used by the effect system. Map each
        // lowercase module to its capitalized resource equivalent so the
        // shared method signatures are found — first in the baked-impl table
        // (`env.impls`, where the slice-2 migration moved `Env.args` /
        // `Env.var`), then in `env.functions` for any future entries that
        // can't be expressed as impl methods.
        if let ExprKind::Identifier(mod_name) = &object.kind {
            let resource_name = match mod_name.as_str() {
                "env" => Some("Env"),
                _ => None,
            };
            if let Some(resource) = resource_name {
                let impl_sig = self.env.impls.iter().find_map(|imp| {
                    // Lowercase-module dispatch (`env.args()`) targets
                    // ambient resource impls registered with empty
                    // target_args; specialized variants of these don't
                    // exist today.
                    if imp.target_type == resource && imp.target_args.is_empty() {
                        imp.methods.get(method).cloned()
                    } else {
                        None
                    }
                });
                let dotted = format!("{}.{}", resource, method);
                let sig_opt = impl_sig.or_else(|| self.env.functions.get(&dotted).cloned());
                if let Some(sig) = sig_opt {
                    if args.len() != sig.params.len() {
                        self.type_error(
                            format!(
                                "'{}.{}' expects {} argument(s), found {}",
                                mod_name,
                                method,
                                sig.params.len(),
                                args.len()
                            ),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                        for arg in args {
                            self.infer_expr(&arg.value);
                        }
                        return sig.return_type;
                    }
                    for (arg, param_ty) in args.iter().zip(sig.params.iter()) {
                        let at = self.infer_expr(&arg.value);
                        self.check_assignable(param_ty, &at, arg.value.span.clone());
                    }
                    return sig.return_type;
                }
            }
        }

        // Type-receiver associated calls: `T.method(args)` where `T` is a
        // type name (struct, enum, or primitive). The parser produces a
        // MethodCall with `object = Identifier("T")`; the regular receiver
        // pipeline below would treat `T` as a value and fail.
        //
        // From dispatch is special-cased — the source type of the argument
        // disambiguates between multiple `impl From[X] for T` impls.
        if let ExprKind::Identifier(type_name) = &object.kind {
            let is_known_type = self.env.structs.contains_key(type_name)
                || self.env.enums.contains_key(type_name)
                || matches!(
                    type_name.as_str(),
                    "i8" | "i16"
                        | "i32"
                        | "i64"
                        | "u8"
                        | "u16"
                        | "u32"
                        | "u64"
                        | "usize"
                        | "f32"
                        | "f64"
                        | "bool"
                        | "char"
                        | "String"
                );
            if is_known_type {
                // Cancel-narrowing side-table: record `Type.method` for this
                // call site so codegen can elide the par-branch cancel check
                // when the resolved callee is provably non-effectful.
                self.method_callee_types.insert(
                    SpanKey::from_span(span),
                    format!("{}.{}", type_name, method),
                );
                if method == "from" && args.len() == 1 {
                    let arg_ty = self.infer_expr(&args[0].value);
                    if arg_ty == Type::Error {
                        return Type::Error;
                    }
                    if let Some(imp) = self.env.find_from_impl(&arg_ty, type_name, &[]) {
                        return imp
                            .methods
                            .get("from")
                            .map(|sig| sig.return_type.clone())
                            .unwrap_or(Type::Error);
                    }
                    self.type_error(
                        format!(
                            "no `impl From[{}] for {}` is in scope",
                            type_display(&arg_ty),
                            type_name
                        ),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    return Type::Error;
                }
                // General associated call: look up the method on the target
                // type with inherent-beats-trait priority per design.md
                // § Method Resolution Step 3. Multi-inherent / multi-trait
                // ambiguity detection (Step 4) is deferred.
                if let Some(sig) = self.env.find_method(type_name, &[], method).cloned() {
                    if args.len() != sig.params.len() {
                        self.type_error(
                            format!(
                                "method '{}' expects {} argument(s), found {}",
                                method,
                                sig.params.len(),
                                args.len()
                            ),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                        for arg in args {
                            self.infer_expr(&arg.value);
                        }
                        return sig.return_type;
                    }
                    for (arg, param) in args.iter().zip(sig.params.iter()) {
                        let arg_ty = self.infer_expr(&arg.value);
                        self.check_assignable(param, &arg_ty, arg.value.span.clone());
                    }
                    return sig.return_type;
                }
                // Known type but no matching method — fall through so the
                // existing "method not found" diagnostic fires below.
            }
        }

        // Concrete-type UFCS dispatch — `TypeName[T1, T2, ...].method(args)`.
        // The parser disambiguates `TypeName[…].method(` to a single-segment
        // `Path { generic_args: Some(...) }` object; here we route through
        // `find_methods_with_args` so impl-level bounds discharge against
        // the explicit type-args, then substitute each impl-level generic
        // param with its concrete arg in the sig before validating call args.
        // (Sub-item 5B of `phase-4-interpreter.md` § method resolution;
        // canonical entry at `phase-2-parser-ast.md` § "Path expression with
        // generic args — concrete-type UFCS support".)
        if let ExprKind::Path {
            segments,
            generic_args: Some(generic_args),
        } = &object.kind
        {
            if segments.len() == 1 {
                let type_name = segments[0].clone();
                // Concrete-type UFCS at slice 1b widens generic_args to
                // `Vec<GenericArg>`; the dispatch surface still consumes
                // type args only — const-arg binding for UFCS calls
                // lands when slice 3's call-site solver threads the
                // substitution through. Const-args at this position are
                // ignored for dispatch but still parsed so the shape
                // round-trips cleanly.
                let target_args: Vec<Type> = generic_args
                    .iter()
                    .filter_map(|a| match a {
                        GenericArg::Type(t) => Some(self.lower_type_expr(t, &[])),
                        GenericArg::Const(_) => None,
                    })
                    .collect();
                self.method_callee_types.insert(
                    SpanKey::from_span(span),
                    format!("{}.{}", type_name, method),
                );
                let candidates: Vec<(ImplInfo, FunctionSig)> = self
                    .env
                    .find_methods_with_args(&type_name, &target_args, method)
                    .into_iter()
                    .map(|(imp, sig)| (imp.clone(), sig.clone()))
                    .collect();
                // Slice 5C of the method-resolution CR — see
                // `phase-4-interpreter.md` § method-resolution sub-item 5C.
                // `find_methods_with_args` already applies the inherent-
                // beats-trait priority partition + bounds-discharge filter
                // (slices 1 + 3); a length-≥2 result here means multiple
                // candidates of the same priority tier survived. The
                // user must pick a specific UFCS form (`TraitName.method(...)`)
                // to disambiguate. Mirrors slice 3's receiver-form
                // `AmbiguousMethod` (E0239) but uses `AmbiguousAssocFn`
                // (E0233) to match slice 3.5 and slice 5A's framing —
                // type-prefixed dispatch is the natural disambiguation
                // form for UFCS.
                if candidates.len() > 1 {
                    let receiver_display = if target_args.is_empty() {
                        type_name.clone()
                    } else {
                        format!(
                            "{}[{}]",
                            type_name,
                            target_args
                                .iter()
                                .map(type_display)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let candidate_lines: Vec<String> = candidates
                        .iter()
                        .map(|(imp, sig)| {
                            let dispatcher = imp
                                .trait_name
                                .clone()
                                .unwrap_or_else(|| imp.target_type.clone());
                            let subs: HashMap<String, SubstValue> = imp
                                .generic_params
                                .as_ref()
                                .map(|gp| {
                                    gp.params
                                        .iter()
                                        .zip(target_args.iter())
                                        .map(|(p, t)| (p.name.clone(), SubstValue::Type(t.clone())))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let params_display = sig
                                .params
                                .iter()
                                .map(|p| type_display(&substitute_type_params(p, &subs)))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let return_display =
                                type_display(&substitute_type_params(&sig.return_type, &subs));
                            format!(
                                "    `{}.{}({})` -> {}",
                                dispatcher, method, params_display, return_display,
                            )
                        })
                        .collect();
                    self.type_error(
                        format!(
                            "ambiguous method '{}' on `{}`: \
                             multiple candidates apply. Use UFCS to disambiguate:\n{}",
                            method,
                            receiver_display,
                            candidate_lines.join("\n"),
                        ),
                        span.clone(),
                        TypeErrorKind::AmbiguousAssocFn,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Error;
                }
                if let Some((imp, sig)) = candidates.first() {
                    let subs: HashMap<String, SubstValue> = imp
                        .generic_params
                        .as_ref()
                        .map(|gp| {
                            gp.params
                                .iter()
                                .zip(target_args.iter())
                                .map(|(p, t)| (p.name.clone(), SubstValue::Type(t.clone())))
                                .collect()
                        })
                        .unwrap_or_default();
                    let param_types: Vec<Type> = sig
                        .params
                        .iter()
                        .map(|p| substitute_type_params(p, &subs))
                        .collect();
                    let return_ty = substitute_type_params(&sig.return_type, &subs);
                    if args.len() != param_types.len() {
                        self.type_error(
                            format!(
                                "method '{}' expects {} argument(s), found {}",
                                method,
                                param_types.len(),
                                args.len()
                            ),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                        for arg in args {
                            self.infer_expr(&arg.value);
                        }
                        return return_ty;
                    }
                    for (arg, param) in args.iter().zip(param_types.iter()) {
                        let arg_ty = self.infer_expr(&arg.value);
                        self.check_assignable(param, &arg_ty, arg.value.span.clone());
                    }
                    return return_ty;
                }
                // No matching impl-table entry. Built-in types (Vec, Option,
                // etc.) whose methods dispatch through special-case infer
                // paths rather than `env.impls` are out of scope for this
                // slice; falling through to a focused diagnostic.
                self.type_error(
                    format!("no method '{}' on `{}[…]`", method, type_name),
                    span.clone(),
                    TypeErrorKind::NoMethodFound,
                );
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                return Type::Error;
            }
        }

        let obj_ty = self.infer_expr(object);
        if obj_ty == Type::Error {
            for arg in args {
                self.infer_expr(&arg.value);
            }
            return Type::Error;
        }

        // Cancel-narrowing side-table: record `Type.method` for this call
        // site so codegen can elide the par-branch cancel check when the
        // resolved callee is provably non-effectful. Populated here once so
        // it covers every dispatch path below (Slice, String, Map, named
        // types, etc.) — the parser sets `MethodCall.span == receiver.span`,
        // so we use `method_callee_types` rather than `expr_types` (which
        // would race with the return-type insertion at the same key).
        if let Some(type_name) = method_callee_type_name(&obj_ty) {
            self.method_callee_types.insert(
                SpanKey::from_span(span),
                format!("{}.{}", type_name, method),
            );
        }

        // Stdlib slice views on sequence types. `.as_slice()` / `.as_slice_mut()`
        // on a `Vec[T]` or `Array[T, N]` (or their ref borrows) produce a
        // `Slice[T]` / `mut Slice[T]` handle, per design.md § Slices.
        if (method == "as_slice" || method == "as_slice_mut") && args.is_empty() {
            let mutable = method == "as_slice_mut";
            let element = match &obj_ty {
                Type::Array { element, .. } => Some(*element.clone()),
                Type::Slice { element, .. } => Some(*element.clone()),
                Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                    Some(args[0].clone())
                }
                Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                    Type::Array { element, .. } => Some(*element.clone()),
                    Type::Slice { element, .. } => Some(*element.clone()),
                    Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                        Some(args[0].clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(el) = element {
                return Type::Slice {
                    element: Box::new(el),
                    mutable,
                };
            }
        }

        // `Slice[T]` and `mut Slice[T]` method dispatch. These types are not
        // `Type::Named` so they fall through the generic branch below; handle
        // them here before the named-type extraction.
        if let Type::Slice { element, mutable } = &obj_ty.clone() {
            return self.infer_slice_method(element, *mutable, method, args, span);
        }

        // Iterator-source methods: `iter()` / `into_iter()` on any iterable
        // collection produce an `Iterator[Item = T]` value. Handled here in
        // one place so per-collection method handlers don't have to repeat
        // the registration. The borrow-vs-consume distinction between
        // `iter()` and `into_iter()` is a typechecker concern in design.md
        // but immaterial at this layer — both return the same Iterator type.
        // See `wip-list2.md` § Iterator trait — full adaptor surface.
        if method == "iter" || method == "into_iter" {
            if let Some(item_ty) = iterator_item_type_for(&obj_ty) {
                if !args.is_empty() {
                    self.type_error(
                        format!("'{}' takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                return Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item_ty],
                };
            }
        }

        // `clone()` on collection types — `Vec[T]`, `String`, `Map[K, V]`,
        // `Set[T]`, `SortedSet[T]`, `Array[T, N]` all implement Clone per
        // design.md § Iteration line 1692. Returns `Self`. The `T: Clone`
        // bound on element types is enforced via the existing trait-bound
        // checking; primitives and String satisfy it trivially. The
        // canonical bullet lives in `phase-8-stdlib-floor.md` (search
        // `Clone trait surface for collections`).
        if method == "clone" {
            if let Some(self_ty) = clone_self_type_for(&obj_ty) {
                if !args.is_empty() {
                    self.type_error(
                        "clone() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                return self_ty;
            }
        }

        // Iterator method dispatch — `Iterator[Item = T].next()` and the
        // adaptor surface (added in subtask 3+). Keyed on the receiver's
        // outer Type::Named name; the Item type is at args[0].
        // `Range` / `RangeInclusive` are also Iterators (matches Rust),
        // routed through the same dispatch so `(0..10).step_by(2)` works
        // without a redundant `.iter()` call.
        if let Type::Named {
            name,
            args: type_args,
        } = &obj_ty
        {
            if name == "Iterator"
                || name == "Peekable"
                || name == "Range"
                || name == "RangeInclusive"
            {
                let item_ty = type_args.first().cloned().unwrap_or(Type::Error);
                let is_peekable = name == "Peekable";
                return self.infer_iterator_method(&item_ty, method, args, span, is_peekable);
            }
        }

        // `Vec[T].push(item: T)` slot check (round 12.46 / Step 4). Vec is a
        // built-in prelude type with no impl block, so without this dispatch
        // `push` falls through to the silent `Type::Error` arm below and the
        // argument never gets checked against the element type. Routing the
        // single argument through `check_assignable(element, arg_ty, span)`
        // means a once-callable closure value flowing into a `Vec[Fn(...)]`
        // element slot triggers `OnceFnIntoFnSlot` via the same path Step 3
        // wired for parameter slots. Other Vec methods continue through the
        // historical fall-through to preserve existing test behavior — Step 5
        // can promote them when needed.
        if method == "push" && args.len() == 1 {
            let element_ty = match &obj_ty {
                Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                    Some(args[0].clone())
                }
                Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                    Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                        Some(args[0].clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(elem) = element_ty {
                let arg_ty = self.infer_expr(&args[0].value);
                // Unify so an unsolved element typevar bound to the
                // receiver (e.g. `let mut v = Vec.new(); v.push(x);`)
                // gets pinned to the first push's value type. Otherwise
                // the binding's `pattern_binding_inner_types` entry
                // stays unresolved and codegen registers `i64` instead
                // of the right LLVM element type.
                unify_types(
                    &elem,
                    &arg_ty,
                    &mut self.env.substitutions,
                    &mut self.env.const_substitutions,
                );
                // Resolve `elem` so a successful unification doesn't
                // leave the assignability check comparing the stale
                // typevar against the (now-pinned) arg type and emitting
                // a spurious `?T → ArgT` mismatch diagnostic.
                let resolved_elem = resolve_type_var_top(&elem, &self.env.substitutions);
                self.check_assignable(&resolved_elem, &arg_ty, args[0].value.span.clone());
                return Type::Unit;
            }
        }

        // `Vec[T].pop()` / `Vec[T].pop_back()` and `VecDeque[T]`'s
        // `pop_front` / `pop_back` all return `Option[T]` per design.md.
        // The codegen-side pop arm builds an `Option[T]` aggregate via
        // multi-word payload words (commit 76263d1); without the
        // typechecker recording the return type, an unannotated
        // `match q.pop_front() { Some(node) => ... }` infers scrutinee
        // type `Error` and pattern bindings lose their tuple types,
        // breaking the `Some(node) => let (a, b) = node` shape's
        // tuple-binding reconstitution in codegen.
        if matches!(method, "pop" | "pop_back" | "pop_front") && args.is_empty() {
            let element_ty = match &obj_ty {
                Type::Named { name, args }
                    if (name == "Vec" || name == "VecDeque") && args.len() == 1 =>
                {
                    Some(args[0].clone())
                }
                Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                    Type::Named { name, args }
                        if (name == "Vec" || name == "VecDeque") && args.len() == 1 =>
                    {
                        Some(args[0].clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(elem) = element_ty {
                // Resolve typevars so a `let mut q = VecDeque.new(); q.push(x);
                // let _ = q.pop_front();` round-trips the element type — without
                // this, `?T` solved by `push` stays unresolved in the
                // `Option[?T]` return, and downstream `Some(x)` bindings lose
                // the surface type they need for codegen routing.
                let resolved = resolve_type_var_top(&elem, &self.env.substitutions);
                return Type::Named {
                    name: "Option".to_string(),
                    args: vec![resolved],
                };
            }
        }

        // `VecDeque[T].push_back(item)` / `push_front(item)` — slot
        // check sibling to `Vec.push`. Returns `Type::Unit`.
        if matches!(method, "push_back" | "push_front") && args.len() == 1 {
            let element_ty = match &obj_ty {
                Type::Named { name, args }
                    if (name == "Vec" || name == "VecDeque") && args.len() == 1 =>
                {
                    Some(args[0].clone())
                }
                Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                    Type::Named { name, args }
                        if (name == "Vec" || name == "VecDeque") && args.len() == 1 =>
                    {
                        Some(args[0].clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(elem) = element_ty {
                let arg_ty = self.infer_expr(&args[0].value);
                // Mirror the unification in the `Vec.push` arm above so an
                // unsolved receiver-element typevar gets pinned to the first
                // pushed value type.
                unify_types(
                    &elem,
                    &arg_ty,
                    &mut self.env.substitutions,
                    &mut self.env.const_substitutions,
                );
                let resolved_elem = resolve_type_var_top(&elem, &self.env.substitutions);
                self.check_assignable(&resolved_elem, &arg_ty, args[0].value.span.clone());
                return Type::Unit;
            }
        }

        // `String` method dispatch. `Type::Str` is not `Type::Named` so it
        // also falls through the generic branch; handle it here.
        if obj_ty == Type::Str {
            return self.infer_str_method(method, args, span);
        }

        // `Map[K, V]` method dispatch. K and V thread through return types.
        if let Type::Named {
            name,
            args: type_args,
        } = &obj_ty
        {
            if name == "Map" {
                let key = type_args.first().cloned().unwrap_or(Type::Error);
                let val = type_args.get(1).cloned().unwrap_or(Type::Error);
                return self.infer_map_method(&key, &val, method, args, span);
            }
        }

        // `Entry[K, V]` method dispatch — `or_insert`, `or_insert_with`,
        // `and_modify`. Produced by `Map.entry(k)`.
        if let Type::Named {
            name,
            args: type_args,
        } = &obj_ty
        {
            if name == "Entry" {
                let key = type_args.first().cloned().unwrap_or(Type::Error);
                let val = type_args.get(1).cloned().unwrap_or(Type::Error);
                return self.infer_entry_method(&key, &val, method, args, span);
            }
        }

        // `SortedSet[T]` method dispatch. Named type but with dedicated
        // per-method typing (generic T threads through return types).
        if let Type::Named {
            name,
            args: type_args,
        } = &obj_ty
        {
            if name == "SortedSet" {
                let element = type_args.first().cloned().unwrap_or(Type::Error);
                return self.infer_sorted_set_method(&element, method, args, span);
            }
            if name == "Set" {
                let element = type_args.first().cloned().unwrap_or(Type::Error);
                return self.infer_set_method(&element, method, args, span);
            }
        }

        // `Regex` method dispatch.
        if let Type::Named { name, .. } = &obj_ty {
            if name == "Regex" {
                return self.infer_regex_method(method, args, span);
            }
        }

        // `Client` / `Response` / `HttpError` method dispatch.
        if let Type::Named { name, .. } = &obj_ty {
            match name.as_str() {
                "Client" => return self.infer_http_client_method(method, args, span),
                "Response" => return self.infer_http_response_method(method, args, span),
                "HttpError" => return self.infer_http_error_method(method, args, span),
                _ => {}
            }
        }

        // `Sender[T]` / `Receiver[T]` method dispatch.
        if let Type::Named {
            name,
            args: type_args,
        } = &obj_ty
        {
            if name == "Sender" || name == "Receiver" {
                let element = type_args.first().cloned().unwrap_or(Type::Error);
                let is_sender = name == "Sender";
                return self.infer_channel_method(is_sender, &element, method, args, span);
            }
        }

        // `Vec[T].sort_by` / `Vec[T].sorted_by` / `Vec[T].sort_by_key` /
        // `Vec[T].sorted_by_key` — closure-shape validation. Vec has no
        // stdlib impl block; without this intercept the call falls through
        // to the silent-no-method path below, leaving the closure arg
        // synth-typed with fresh metavars (no pushdown into params, no
        // check on the body's return type). A wrong-shape closure would
        // typecheck and runtime-panic in the interpreter's closure-honoring
        // sort paths. `sort_by` / `sort_by_key` mutate in place and return
        // Unit; `sorted_by` / `sorted_by_key` return a new Vec. Receiver
        // mutability is enforced at the binding layer (calling `.sort_by`
        // on a non-`mut` binding errors there), so no explicit mutability
        // gate is duplicated here.
        if matches!(
            method,
            "sort_by" | "sorted_by" | "sort_by_key" | "sorted_by_key"
        ) {
            let elem_for_vec: Option<Type> = match &obj_ty {
                Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                    Some(args[0].clone())
                }
                Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                    Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                        Some(args[0].clone())
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(elem) = elem_for_vec {
                let is_key = method.ends_with("_key");
                let arg_label = if is_key { "key" } else { "comparator" };
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Vec.{}() expects 1 argument ({} closure), found {}",
                            method,
                            arg_label,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else if is_key {
                    self.check_sort_key_closure(&elem, &args[0], method, span);
                } else {
                    self.check_sort_comparator(&elem, &args[0], method, span);
                }
                let mutates_in_place = method == "sort_by" || method == "sort_by_key";
                return if mutates_in_place {
                    Type::Unit
                } else {
                    Type::Named {
                        name: "Vec".to_string(),
                        args: vec![elem],
                    }
                };
            }
        }

        // Strip outer `ref` / `mut ref` to get the named receiver per
        // design.md § Method Resolution Step 1 (autoref candidates `T`,
        // `ref T`, `mut ref T` collapse to the same name lookup; the
        // receiver/self-mode compatibility check happens at the
        // param-binding layer). Shared-struct / Rc / Arc deref handled
        // here (sub-item 3a of the `Type::Shared` / `Type::Rc` /
        // `Type::Arc` representation work) — `Rc[Foo].method()` and
        // `let s: SharedStruct; s.method()` resolve through the inner
        // type's methods. Refinement-base candidate (1C) remains
        // deferred on `Type::Refinement` from phase-9.
        let receiver_for_lookup: Type = receiver_for_method_lookup(&obj_ty);
        let (type_name, type_args) = match &receiver_for_lookup {
            Type::Named { name, args } => (name.clone(), args.clone()),
            Type::TypeParam(name) if name == "Self" => {
                // Self-receiver dispatch (slice 3.5 of the method-resolution
                // CR — `phase-4-interpreter.md` item 8). `self.method()`
                // inside a trait default body resolves through the enclosing
                // trait's own methods + supertrait closure. Outside trait
                // bodies (`enclosing_trait == None`) the dispatcher returns
                // `Type::Error` to preserve the pre-existing silent
                // fallthrough — impl-method bodies bind `Self` via
                // `current_self_type`, a different mechanism.
                return self.dispatch_self_receiver_method(method, args, span);
            }
            Type::TypeParam(name) => {
                // Receiver-form generic call-site dispatch (slice 2 of the
                // method-resolution CR — see `phase-4-interpreter.md` item 8).
                // The complement to type-prefixed `T.method()` dispatch via
                // `try_dispatch_typeparam_assoc_fn` (`infer_call`): for
                // `t.method(args)` where `t: T` and `T: SomeTrait` declares
                // `method`, look up T's bounds in `enclosing_bounds`, find
                // the trait declaring `method`, and lower the trait method's
                // signature with `Self → Type::TypeParam(T)` substitution.
                // Multiple matching bounds → AmbiguousAssocFn (UFCS hint);
                // zero matches → NoMethodFound; exactly one → dispatch.
                //
                // `Self` is handled in the arm above (slice 3.5) — it
                // routes to `dispatch_self_receiver_method` which consults
                // the enclosing trait being defined, not just bounds.
                return self.dispatch_typeparam_receiver_method(name, method, args, span);
            }
            _ => {
                // For non-named types, just type-check args and return Error
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                return Type::Error;
            }
        };

        // Look up method on the receiver type with inherent-beats-trait
        // priority per design.md § Method Resolution Step 3, plus
        // conditional-impl filtering against the receiver's concrete
        // generic args (slice 1 of the method-resolution CR — see
        // `phase-4-interpreter.md`). All-candidates collection lets us
        // detect Step-4 ambiguity (slice 3): >1 surviving candidate at
        // the same priority tier (e.g. two trait impls when no inherent
        // matches) emits AmbiguousMethod and returns Type::Error.
        let candidates = self
            .env
            .find_methods_with_args(&type_name, &type_args, method);
        let method_sig: Option<FunctionSig> = if candidates.len() > 1 {
            // Render each candidate as `Trait.method(receiver)` (or
            // `Type.method(receiver)` for the rare inherent-vs-inherent
            // case). The signature display includes the receiver-then-args
            // tuple plus return type so the programmer can tell the
            // candidates apart at a glance.
            let candidate_lines: Vec<String> = candidates
                .iter()
                .map(|(imp, sig)| {
                    let dispatcher = imp
                        .trait_name
                        .clone()
                        .unwrap_or_else(|| imp.target_type.clone());
                    let params_display = std::iter::once(type_name.clone())
                        .chain(sig.params.iter().map(type_display))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "    `{}.{}({})` -> {}",
                        dispatcher,
                        method,
                        params_display,
                        type_display(&sig.return_type),
                    )
                })
                .collect();
            let receiver_display = if type_args.is_empty() {
                type_name.clone()
            } else {
                format!(
                    "{}[{}]",
                    type_name,
                    type_args
                        .iter()
                        .map(type_display)
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            };
            self.type_error(
                format!(
                    "ambiguous method '{}' on receiver of type '{}': \
                     multiple candidates apply. Use UFCS to disambiguate:\n{}",
                    method,
                    receiver_display,
                    candidate_lines.join("\n"),
                ),
                span.clone(),
                TypeErrorKind::AmbiguousMethod,
            );
            for arg in args {
                self.infer_expr(&arg.value);
            }
            return Type::Error;
        } else {
            candidates.into_iter().next().map(|(_, sig)| sig.clone())
        };

        match method_sig {
            Some(sig) => {
                // Validate labels against method parameter names
                self.validate_labels(args, &sig.param_names, span);
                // Check argument count (excluding self)
                if args.len() != sig.params.len() {
                    self.type_error(
                        format!(
                            "method '{}' expects {} argument(s), found {}",
                            method,
                            sig.params.len(),
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return sig.return_type.clone();
                }
                // Reuse the round-10.1 closure-pushdown helper so generic
                // methods solve `T` from non-closure args before checking
                // closure args. `apply_call_site_marker` is `false`: per
                // design.md, the call-site `mut` marker rule applies only to
                // free-function calls, never to method calls.
                self.check_call_args_with_substitution(
                    args,
                    &sig.params,
                    &sig.return_type,
                    span,
                    /* apply_call_site_marker = */ false,
                )
            }
            None => {
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                // Tightening: error only for user-defined types whose impls
                // are exhaustively known. Built-in prelude types (`Option`,
                // `Result`, `Vec`, `Regex`, etc. — see `prelude::PRELUDE_TYPES`)
                // have a partially-implicit method surface (`.unwrap()`,
                // `.is_ok()`, regex methods that route through Type::Named
                // dispatch above but may not match every name) so they keep
                // the historical silent fall-through.
                let is_user_defined = (self.env.structs.contains_key(&type_name)
                    || self.env.enums.contains_key(&type_name))
                    && !crate::prelude::PRELUDE_TYPES.contains(&type_name.as_str());
                // Args-specialization tightening: even on prelude types, fire
                // NoMethodFound when the method exists on a *different*
                // args-specialization of this type-name (e.g.,
                // `Option[i32].is_lt()` when only `impl Option[Ordering]`
                // declares `is_lt`). Preserves the silent fall-through when
                // the method is genuinely absent (`Vec[i32].some_typo()`
                // stays silent) while surfacing the args-mismatch case that
                // would otherwise silently reach the interpreter and produce
                // a wrong answer through unrelated dispatch.
                let method_on_other_specialization =
                    self.env.impls.iter().any(|imp| {
                        imp.target_type == type_name && imp.methods.contains_key(method)
                    });
                if is_user_defined || method_on_other_specialization {
                    let candidates = self.env.collect_method_names(&type_name, &[]);
                    let candidate_refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
                    let mut msg = format!("no method '{}' on type '{}'", method, type_name);
                    if let Some(suggestion) =
                        crate::edit_distance::suggest_similar(method, &candidate_refs)
                    {
                        msg.push_str(&format!(", did you mean '{}'?", suggestion));
                    }
                    self.type_error(msg, span.clone(), TypeErrorKind::NoMethodFound);
                }
                Type::Error
            }
        }
    }

    /// Validate a `sort_by` / `sorted_by` comparator argument against the
    /// `Fn(elem, elem) -> Ordering` shape. Pushes the expected function
    /// type down into the closure via `check_expr` so closure-parameter
    /// types are inferred from the element type rather than left as fresh
    /// metavars (today's silent-fall-through path) — a wrong-shape
    /// comparator (`xs.sort_by(|a| a)`, `xs.sort_by(|a, b| a)`, or a
    /// `Fn` value of the wrong arity / return type) now produces a
    /// TypeMismatch at the closure expression instead of runtime-panicking
    /// when the interpreter invokes it with two args / consumes the
    /// non-Ordering return.
    fn check_sort_comparator(&mut self, elem: &Type, arg: &CallArg, method: &str, span: &Span) {
        let expected = Type::Function {
            params: vec![elem.clone(), elem.clone()],
            return_type: Box::new(Type::Named {
                name: "Ordering".to_string(),
                args: Vec::new(),
            }),
        };
        let _ = (method, span); // method / span carried for future diagnostic refinement
        self.check_expr(&arg.value, &expected);
    }

    /// Validate a `sort_by_key` / `sorted_by_key` key-function argument
    /// against `Fn(elem) -> K` and verify the inferred `K` satisfies `Ord`.
    /// `K` is a fresh metavar pushed down through `check_expr`; once the
    /// closure body unifies it to a concrete type, an Ord bound check
    /// rejects key types (raw floats, function values, etc.) that lack
    /// total ordering. Generic `K` (still a TypeVar after resolution)
    /// flows through without an Ord assertion — the bound will be
    /// rechecked at monomorphization.
    fn check_sort_key_closure(&mut self, elem: &Type, arg: &CallArg, method: &str, span: &Span) {
        // `Fn(elem) -> K` where K is a placeholder the closure body solves.
        // Use `Type::TypeParam` not `Type::TypeVar`: `types_compatible` treats
        // TypeParam permissively so the `check_assignable` step doesn't fire
        // a spurious "expected K, found <body_ty>" diagnostic. After
        // `check_expr` returns the inferred closure type, read the resolved
        // body type out of the Function shape and check the Ord bound on it.
        // Pattern lifted from `Iterator.map`'s pushdown at infer_iterator_method.
        let placeholder = Type::TypeParam("__sort_by_key_K".to_string());
        let expected = Type::Function {
            params: vec![elem.clone()],
            return_type: Box::new(placeholder),
        };
        let actual_ty = self.check_expr(&arg.value, &expected);
        let resolved_k = match actual_ty {
            Type::Function { return_type, .. } | Type::OnceFunction { return_type, .. } => {
                *return_type
            }
            _ => return,
        };
        if !matches!(
            resolved_k,
            Type::TypeParam(_) | Type::TypeVar(_) | Type::Error
        ) && !self.type_supports_ord(&resolved_k)
        {
            self.type_error(
                format!(
                    "{}: key closure return type '{}' does not implement Ord",
                    method,
                    type_display(&resolved_k)
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
    }

    /// Infer the return type of a method call on `String` (`Type::Str`).
    /// Called from `infer_method_call` when the object type is `Type::Str`.
    fn infer_str_method(&mut self, method: &str, args: &[CallArg], span: &Span) -> Type {
        match method {
            "sorted" => {
                if !args.is_empty() {
                    self.type_error(
                        "'sorted' takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Str
            }
            "sorted_by" => {
                // sorted_by(cmp: Fn(Char, Char) -> Ordering) -> String
                if args.len() != 1 {
                    self.type_error(
                        format!("'sorted_by' expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    self.check_sort_comparator(&Type::Char, &args[0], "sorted_by", span);
                }
                Type::Str
            }
            "chars" => {
                // chars() -> Iterator[char]. Peer of design.md § Character type
                // (line 2299): `for c in s` and `s.chars()` both iterate the
                // string's Unicode scalar values. Tree-walk interpreter
                // implements the same in eval_method_call's "chars" arm; a
                // for-loop on a bare String falls back through the same path.
                if !args.is_empty() {
                    self.type_error(
                        "'chars' takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Char],
                }
            }
            // Unknown string method — typo-suggestion diagnostic if close to
            // a known name, silent otherwise (`len`, `contains`, `is_empty`,
            // … are runtime-only and not yet wired through the typechecker).
            // Flip to always-error once enumeration catches up to the
            // interpreter's String surface — design.md § Method Resolution
            // Step 7.
            _ => self.require_known_method(
                "String",
                method,
                &["chars", "sorted", "sorted_by"],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on a `Slice[T]` or `mut Slice[T]`.
    /// Handles the full read-only surface and the mutation-only surface for
    /// `mut Slice[T]`. Called from `infer_method_call` when the object type is
    /// `Type::Slice`.
    fn infer_slice_method(
        &mut self,
        element: &Type,
        mutable: bool,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let elem = element.clone();
        let option_elem = Type::Named {
            name: "Option".to_string(),
            args: vec![elem.clone()],
        };
        let option_i64 = Type::Named {
            name: "Option".to_string(),
            args: vec![Type::Int(IntSize::I64)],
        };
        let slice_elem = Type::Slice {
            element: Box::new(elem.clone()),
            mutable: false,
        };
        let vec_slice = Type::Named {
            name: "Vec".to_string(),
            args: vec![slice_elem.clone()],
        };

        match method {
            // Read-only methods (available on both Slice[T] and mut Slice[T])
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "Slice.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "Slice.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "first" | "last" => {
                if !args.is_empty() {
                    self.type_error(
                        format!("Slice.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                option_elem
            }
            "get" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                option_elem
            }
            "contains" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "binary_search" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                option_i64
            }
            "split_at" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                Type::Tuple(vec![slice_elem.clone(), slice_elem])
            }
            "chunks" | "windows" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                vec_slice
            }
            // Mutation methods (require mut Slice[T])
            "sort" | "reverse" => {
                if !mutable {
                    self.type_error(
                        format!(
                            "Slice.{}() requires a mutable slice (`mut Slice[T]`)",
                            method
                        ),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if !args.is_empty() {
                    self.type_error(
                        format!("Slice.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Unit
            }
            "sort_by" => {
                if !mutable {
                    self.type_error(
                        "Slice.sort_by() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Slice.sort_by() expects 1 argument (comparator closure), found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    self.check_sort_comparator(&elem, &args[0], "sort_by", span);
                }
                Type::Unit
            }
            "sort_by_key" => {
                if !mutable {
                    self.type_error(
                        "Slice.sort_by_key() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Slice.sort_by_key() expects 1 argument (key closure), found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    self.check_sort_key_closure(&elem, &args[0], "sort_by_key", span);
                }
                Type::Unit
            }
            "fill" => {
                if !mutable {
                    self.type_error(
                        "Slice.fill() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Unit
            }
            "swap" => {
                if !mutable {
                    self.type_error(
                        "Slice.swap() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                Type::Unit
            }
            // `Slice[T]` IS `Iterator[T]` — `.iter()` / `.into_iter()` route
            // through the same Iterator dispatch as `Vec.iter()` so chained
            // adaptors (`s.iter().map(f).filter(p).collect()`) compose. The
            // receiver-type match in `infer_method_call` lands here before
            // the generic `iter` / `into_iter` arm, so the registration
            // duplicates that arm shape (no-args, returns `Iterator[T]`).
            "iter" | "into_iter" => {
                if !args.is_empty() {
                    self.type_error(
                        format!("Slice.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![elem],
                }
            }
            _ => self.require_known_method(
                "Slice",
                method,
                &[
                    "binary_search",
                    "chunks",
                    "contains",
                    "fill",
                    "first",
                    "get",
                    "into_iter",
                    "is_empty",
                    "iter",
                    "last",
                    "len",
                    "reverse",
                    "sort",
                    "sort_by",
                    "sort_by_key",
                    "split_at",
                    "swap",
                    "windows",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Map[K, V]`.
    /// `key` is K, `val` is V from the receiver's type arguments.
    fn infer_map_method(
        &mut self,
        key: &Type,
        val: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        // K: Hash + Eq bound — Map requires the key type to be hashable and equality-comparable.
        if !self.type_supports_hash(key) || !self.type_supports_eq(key) {
            let missing = if !self.type_supports_hash(key) && !self.type_supports_eq(key) {
                "Hash + Eq"
            } else if !self.type_supports_hash(key) {
                "Hash"
            } else {
                "Eq"
            };
            self.type_error(
                format!(
                    "Map[{}, ...]: key type does not implement `{}`; \
                     only hashable equality-comparable types (integers, bool, char, String, \
                     or structs/enums with `#[derive(Hash, Eq)]`) can be Map keys",
                    type_display(key),
                    missing
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
        let k = key.clone();
        let v = val.clone();
        let option_v = Type::Named {
            name: "Option".to_string(),
            args: vec![v.clone()],
        };
        let vec_k = Type::Named {
            name: "Vec".to_string(),
            args: vec![k.clone()],
        };
        let vec_v = Type::Named {
            name: "Vec".to_string(),
            args: vec![v.clone()],
        };
        let tuple_kv = Type::Tuple(vec![k.clone(), v.clone()]);
        let vec_kv = Type::Named {
            name: "Vec".to_string(),
            args: vec![tuple_kv],
        };
        let map_kv = Type::Named {
            name: "Map".to_string(),
            args: vec![k.clone(), v.clone()],
        };

        match method {
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "contains_key" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&k, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "get" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&k, &at, arg.value.span.clone());
                }
                option_v
            }
            "get_or" => {
                if let Some(key_arg) = args.first() {
                    let kt = self.infer_expr(&key_arg.value);
                    self.check_assignable(&k, &kt, key_arg.value.span.clone());
                }
                if let Some(default_arg) = args.get(1) {
                    let dt = self.infer_expr(&default_arg.value);
                    self.check_assignable(&v, &dt, default_arg.value.span.clone());
                }
                v
            }
            "insert" => {
                if let Some(key_arg) = args.first() {
                    let kt = self.infer_expr(&key_arg.value);
                    self.check_assignable(&k, &kt, key_arg.value.span.clone());
                }
                if let Some(val_arg) = args.get(1) {
                    let vt = self.infer_expr(&val_arg.value);
                    self.check_assignable(&v, &vt, val_arg.value.span.clone());
                }
                option_v
            }
            "remove" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&k, &at, arg.value.span.clone());
                }
                option_v
            }
            "keys" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.keys() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                vec_k
            }
            "values" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.values() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                vec_v
            }
            "entries" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.entries() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                vec_kv
            }
            "merge" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&map_kv, &at, arg.value.span.clone());
                }
                map_kv
            }
            "clear" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.clear() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Unit
            }
            "entry" => {
                // `entry(key: K) -> Entry[K, V]` — view returned for the given
                // key, occupied or vacant. Drives the in-place insert-or-modify
                // chain (or_insert / or_insert_with / and_modify) via
                // `infer_entry_method`. See design.md § Entry[K, V].
                if args.len() != 1 {
                    self.type_error(
                        format!("Map.entry() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let kt = self.infer_expr(&args[0].value);
                    self.check_assignable(&k, &kt, args[0].value.span.clone());
                }
                Type::Named {
                    name: "Entry".to_string(),
                    args: vec![k.clone(), v.clone()],
                }
            }
            _ => self.require_known_method(
                "Map",
                method,
                &[
                    "clear",
                    "contains_key",
                    "entries",
                    "entry",
                    "get",
                    "get_or",
                    "insert",
                    "is_empty",
                    "keys",
                    "len",
                    "merge",
                    "remove",
                    "values",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Entry[K, V]`.
    /// Drives the chain produced by `Map.entry(k)` — `or_insert`,
    /// `or_insert_with`, `and_modify`. Effect polymorphism on the closure-
    /// taking forms is handled by the existing closure-effect-propagation
    /// pass in the effect checker; this layer just types the shape.
    fn infer_entry_method(
        &mut self,
        key: &Type,
        val: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let v = val.clone();
        let mut_ref_v = Type::MutRef(Box::new(v.clone()));
        let entry_kv = Type::Named {
            name: "Entry".to_string(),
            args: vec![key.clone(), v.clone()],
        };
        match method {
            "or_insert" => {
                // `or_insert(default: V) -> mut ref V`. Returns a borrow into
                // the map's slot — fresh on Vacant (after writing default),
                // existing on Occupied.
                if args.len() != 1 {
                    self.type_error(
                        format!("Entry.or_insert() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let dt = self.infer_expr(&args[0].value);
                    self.check_assignable(&v, &dt, args[0].value.span.clone());
                }
                mut_ref_v
            }
            "or_insert_with" => {
                // `or_insert_with[with E](f: Fn() -> V with E) -> mut ref V
                // with E`. Closure invoked only on the Vacant arm; effect
                // propagation through `with E` is handled by the effect
                // checker reading the closure's effect set.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Entry.or_insert_with() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let f_ty = Type::Function {
                        params: vec![],
                        return_type: Box::new(v.clone()),
                    };
                    self.check_expr(&args[0].value, &f_ty);
                }
                mut_ref_v
            }
            "and_modify" => {
                // `and_modify[with E](f: Fn(mut ref V) with E) -> Entry[K, V]
                // with E`. Closure invoked only on Occupied; receives a
                // `mut ref V` to the existing slot. Returns self for
                // chaining (e.g. `.and_modify(...).or_insert(default)`).
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Entry.and_modify() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let f_ty = Type::Function {
                        params: vec![mut_ref_v.clone()],
                        return_type: Box::new(Type::Unit),
                    };
                    self.check_expr(&args[0].value, &f_ty);
                }
                entry_kv
            }
            _ => self.require_known_method(
                "Entry",
                method,
                &["and_modify", "or_insert", "or_insert_with"],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `SortedSet[T]`.
    /// `element` is the resolved `T` from the receiver's type arguments.
    /// Called from `infer_method_call` when the object type is
    /// `Type::Named { name: "SortedSet", ... }`.
    fn infer_sorted_set_method(
        &mut self,
        element: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        // T: Ord bound — SortedSet requires a total order on its element type.
        if !self.type_supports_ord(element) {
            self.type_error(
                format!(
                    "SortedSet[{}]: element type does not implement `Ord`; \
                     only types with a total order (integers, bool, char, String, \
                     or structs/enums with `#[derive(Ord)]`) can be SortedSet elements",
                    type_display(element)
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
        let elem = element.clone();
        let option_elem = Type::Named {
            name: "Option".to_string(),
            args: vec![elem.clone()],
        };
        let sorted_set_elem = Type::Named {
            name: "SortedSet".to_string(),
            args: vec![elem.clone()],
        };

        match method {
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "SortedSet.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "SortedSet.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "contains" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "insert" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "remove" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "min" | "max" => {
                if !args.is_empty() {
                    self.type_error(
                        format!("SortedSet.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                option_elem
            }
            "union" | "intersection" | "difference" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&sorted_set_elem, &at, arg.value.span.clone());
                }
                sorted_set_elem
            }
            _ => self.require_known_method(
                "SortedSet",
                method,
                &[
                    "contains",
                    "difference",
                    "insert",
                    "intersection",
                    "is_empty",
                    "len",
                    "max",
                    "min",
                    "remove",
                    "union",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Set[T: Hash + Eq]`.
    /// Hash set with O(1) average insert/remove/contains. Enforces the
    /// `T: Hash + Eq` bound the same way `Map[K, V]` checks `K: Hash + Eq`.
    fn infer_set_method(
        &mut self,
        element: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        // T: Hash + Eq bound
        if !self.type_supports_hash(element) || !self.type_supports_eq(element) {
            self.type_error(
                format!(
                    "Set[{}]: element type does not implement `Hash + Eq`; \
                     only types with a hash (integers, bool, char, String, \
                     or structs/enums with `#[derive(Hash, Eq)]`) can be Set elements",
                    type_display(element)
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
        let elem = element.clone();
        let set_elem = Type::Named {
            name: "Set".to_string(),
            args: vec![elem.clone()],
        };

        match method {
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "Set.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "Set.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "contains" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "insert" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "remove" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "union" | "intersection" | "difference" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&set_elem, &at, arg.value.span.clone());
                }
                set_elem
            }
            _ => self.require_known_method(
                "Set",
                method,
                &[
                    "contains",
                    "difference",
                    "insert",
                    "intersection",
                    "is_empty",
                    "len",
                    "remove",
                    "union",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Iterator[Item = T]`.
    /// `next()` lands in subtask 1; `map(f)` / `filter(pred)` in subtask 3;
    /// the rest of the surface follows in `wip-list2.md` subtasks 4+.
    fn infer_iterator_method(
        &mut self,
        item: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
        is_peekable: bool,
    ) -> Type {
        match method {
            "next" => {
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.next() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![item.clone()],
                }
            }
            "map" => {
                // `map(f: Fn(T) -> U) -> Iterator[U]` — U is solved by
                // pushing `Fn(T) -> TypeParam("__iter_map_U")` into
                // `check_expr`. The closure-pushdown path (lines 5429+) seeds
                // the closure's parameter from T and infers the body type
                // freely; the resulting `actual` is `Fn(T) -> body_ty`. We
                // then read body_ty back out as the new Item type.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.map() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Error],
                    };
                }
                let f_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_map_U".to_string())),
                };
                let actual_ty = self.check_expr(&args[0].value, &f_ty);
                let new_item = match actual_ty {
                    Type::Function { return_type, .. } => *return_type,
                    _ => Type::Error,
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![new_item],
                }
            }
            "filter" => {
                // `filter(pred: Fn(T) -> bool) -> Iterator[T]` — no fresh
                // type variable; the predicate's signature is fully known
                // so check_expr suffices for closure-param pushdown.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.filter() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let pred_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::Bool),
                };
                self.check_expr(&args[0].value, &pred_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "count" => {
                // `count() -> i64` — terminal. Drains the iterator and
                // returns the element count.
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.count() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Int(IntSize::I64)
            }
            "collect" => {
                // `collect() -> Vec[T]` — terminal. v1 is Vec-only; full
                // FromIterator (collect into Set / Map / Array / etc. via
                // type-context inference) is a follow-up CR per
                // wip-list2.md subtask 4.
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.collect() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Vec".to_string(),
                    args: vec![item.clone()],
                }
            }
            "fold" => {
                // `fold(init: A, f: Fn(A, T) -> A) -> A` — terminal. A is
                // inferred from `init` (concrete after infer_expr); both
                // closure params and return are then concrete so
                // check_expr suffices for closure-pushdown.
                if args.len() != 2 {
                    self.type_error(
                        format!("Iterator.fold() expects 2 arguments, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Error;
                }
                let acc_ty = self.infer_expr(&args[0].value);
                let f_ty = Type::Function {
                    params: vec![acc_ty.clone(), item.clone()],
                    return_type: Box::new(acc_ty.clone()),
                };
                self.check_expr(&args[1].value, &f_ty);
                acc_ty
            }
            "any" | "all" => {
                // Short-circuit terminals — `any(pred) -> bool` /
                // `all(pred) -> bool`. Same predicate signature as
                // `filter`, so check_expr against `Fn(T) -> bool`
                // suffices for closure-pushdown.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Bool;
                }
                let pred_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::Bool),
                };
                self.check_expr(&args[0].value, &pred_ty);
                Type::Bool
            }
            "enumerate" => {
                // `enumerate() -> Iterator[(i64, T)]` — wraps each item
                // into a tuple of (index, item).
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.enumerate() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Tuple(vec![Type::Int(IntSize::I64), item.clone()])],
                }
            }
            "take" | "skip" => {
                // `take(n: i64) -> Iterator[T]` and `skip(n: i64) ->
                // Iterator[T]`. Argument is checked against i64; the
                // element type passes through unchanged.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                self.check_expr(&args[0].value, &Type::Int(IntSize::I64));
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "take_while" | "skip_while" => {
                // `take_while(pred: Fn(T) -> bool) -> Iterator[T]` and
                // `skip_while(pred: Fn(T) -> bool) -> Iterator[T]` —
                // same predicate signature as `filter`, so check_expr
                // against `Fn(T) -> bool` suffices for closure-pushdown.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let pred_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::Bool),
                };
                self.check_expr(&args[0].value, &pred_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "flat_map" => {
                // `flat_map(f: Fn(T) -> Iterator[U]) -> Iterator[U]` —
                // the closure body must return an iterator; its element
                // type becomes the new Item. Same pushdown pattern as
                // `map`: `Fn(T) -> TypeParam("__iter_flatmap_U")` lets
                // the body's actual return type flow back, then we
                // pattern-match it for `Iterator[U]`. A non-iterator
                // return raises a TypeMismatch explicitly.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.flat_map() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Error],
                    };
                }
                let f_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_flatmap_U".to_string())),
                };
                let actual_ty = self.check_expr(&args[0].value, &f_ty);
                let new_item = match actual_ty {
                    Type::Function { return_type, .. } => match *return_type {
                        Type::Named {
                            name,
                            args: mut iter_args,
                        } if name == "Iterator" && iter_args.len() == 1 => iter_args.remove(0),
                        other => {
                            self.type_error(
                                format!(
                                    "Iterator.flat_map() closure must return Iterator[U], found {:?}",
                                    other
                                ),
                                span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            Type::Error
                        }
                    },
                    _ => Type::Error,
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![new_item],
                }
            }
            "step_by" => {
                // `step_by(n: i64) -> Iterator[T]` — element type
                // passes through. Argument is checked against i64;
                // negative or zero `n` is a runtime concern (clamped
                // to 1 by the interpreter), so the typechecker
                // accepts any i64.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.step_by() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                self.check_expr(&args[0].value, &Type::Int(IntSize::I64));
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "cycle" => {
                // `cycle() -> Iterator[T]` — element type passes
                // through. The "cloneable source" requirement noted
                // in design.md is implicit here: every Value derives
                // Clone, so any iterator can cycle.
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.cycle() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "inspect" => {
                // `inspect(f: Fn(T) -> R) -> Iterator[T]` — closure's
                // return is discarded so we leave R free via TypeParam
                // pushdown. Element type passes through unchanged.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.inspect() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let f_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_inspect_R".to_string())),
                };
                self.check_expr(&args[0].value, &f_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "scan" => {
                // `scan(init: A, f: Fn(A, T) -> Option<(A, U)>) ->
                // Iterator[U]`. A is inferred from init; the closure's
                // return is constrained via post-hoc unwrap of
                // Option<(A, U)>. U becomes the new Item.
                if args.len() != 2 {
                    self.type_error(
                        format!("Iterator.scan() expects 2 arguments, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Error],
                    };
                }
                let acc_ty = self.infer_expr(&args[0].value);
                let f_ty = Type::Function {
                    params: vec![acc_ty.clone(), item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_scan_R".to_string())),
                };
                let actual_ty = self.check_expr(&args[1].value, &f_ty);
                let new_item = match actual_ty {
                    Type::Function { return_type, .. } => match *return_type {
                        Type::Named {
                            name,
                            args: mut opt_args,
                        } if name == "Option" && opt_args.len() == 1 => match opt_args.remove(0) {
                            Type::Tuple(mut tuple_args) if tuple_args.len() == 2 => {
                                let actual_acc = tuple_args.remove(0);
                                self.check_assignable(&acc_ty, &actual_acc, span.clone());
                                tuple_args.remove(0)
                            }
                            other => {
                                self.type_error(
                                    format!(
                                        "Iterator.scan() closure must return Option<(A, U)>, found Option<{:?}>",
                                        other
                                    ),
                                    span.clone(),
                                    TypeErrorKind::TypeMismatch,
                                );
                                Type::Error
                            }
                        },
                        other => {
                            self.type_error(
                                format!(
                                    "Iterator.scan() closure must return Option<(A, U)>, found {:?}",
                                    other
                                ),
                                span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            Type::Error
                        }
                    },
                    _ => Type::Error,
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![new_item],
                }
            }
            "chunks" | "windows" => {
                // `chunks(n: i64) -> Iterator[Vec[T]]` — non-overlapping
                // groups of up to n consecutive items (final group may
                // be shorter when the source length isn't a multiple
                // of n).
                // `windows(n: i64) -> Iterator[Vec[T]]` — sliding view
                // of size n, advancing by 1 per pull (yields nothing
                // when source has fewer than n items). Both buffer
                // and allocate a fresh `Vec[T]` per yielded group; the
                // effect-checker seeds `allocates(Heap)` on
                // `Iterator.{chunks,windows}`. Argument is checked
                // against i64 (clamped at the runtime layer like
                // `take(n)` / `step_by(n)`).
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Named {
                            name: "Vec".to_string(),
                            args: vec![item.clone()],
                        }],
                    };
                }
                let arg_ty = self.infer_expr(&args[0].value);
                self.check_assignable(
                    &Type::Int(IntSize::I64),
                    &arg_ty,
                    args[0].value.span.clone(),
                );
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Named {
                        name: "Vec".to_string(),
                        args: vec![item.clone()],
                    }],
                }
            }
            "chunk_by" => {
                // `chunk_by(key_fn: Fn(T) -> K) -> Iterator[Vec[T]]` —
                // groups consecutive elements where `key_fn(item)`
                // produces equal keys. Each group is allocated as a
                // fresh `Vec[T]` (the effect-checker seeds
                // `allocates(Heap)` on `Iterator.chunk_by`). K is left
                // free via TypeParam pushdown — equality is enforced
                // at runtime via `Value::PartialEq`, matching the
                // permissive pattern used by scan/inspect.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.chunk_by() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Named {
                            name: "Vec".to_string(),
                            args: vec![item.clone()],
                        }],
                    };
                }
                let key_fn_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_chunk_by_K".to_string())),
                };
                self.check_expr(&args[0].value, &key_fn_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Named {
                        name: "Vec".to_string(),
                        args: vec![item.clone()],
                    }],
                }
            }
            "chain" => {
                // `chain(other: Iterator[T]) -> Iterator[T]` — the
                // element type must agree on both sides. Push down
                // `Iterator[T]` so the argument's element type is
                // checked against ours.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.chain() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let expected = Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                };
                self.check_expr(&args[0].value, &expected);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "zip" => {
                // `zip(other: Iterator[U]) -> Iterator[(T, U)]` — the
                // other iterator's element type can differ; we infer
                // it and use it as the second tuple slot. infer_expr
                // gives us back the actual `Iterator[U]` from which we
                // can extract U.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.zip() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Tuple(vec![item.clone(), Type::Error])],
                    };
                }
                let other_ty = self.infer_expr(&args[0].value);
                let other_item = match &other_ty {
                    Type::Named { name, args } if name == "Iterator" && args.len() == 1 => {
                        args[0].clone()
                    }
                    _ => {
                        self.type_error(
                            format!(
                                "Iterator.zip() expects an Iterator argument, found {:?}",
                                other_ty
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        Type::Error
                    }
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Tuple(vec![item.clone(), other_item])],
                }
            }
            "peekable" => {
                // `peekable() -> Peekable[T]` — wraps the receiver into a
                // distinct named type that exposes `peek()` in addition
                // to the rest of the Iterator surface. Idempotent on
                // a Peekable receiver (still returns Peekable[T]).
                if !args.is_empty() {
                    self.type_error(
                        format!(
                            "Iterator.peekable() takes no arguments, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Peekable".to_string(),
                    args: vec![item.clone()],
                }
            }
            "peek" => {
                // `peek() -> Option<T>` — only valid on `Peekable[T]`. The
                // distinct receiver name is the type-level signal that
                // peekable() has been called; on a plain Iterator we
                // emit UnknownMethod (via Type::Error) so adaptor pipelines
                // that drop the Peekable wrapper (e.g. `peekable().map(f)`
                // returning Iterator[U]) reject downstream `.peek()`.
                if !is_peekable {
                    self.type_error(
                        "peek() is only available on Peekable[T] (call .peekable() first)"
                            .to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Error;
                }
                if !args.is_empty() {
                    self.type_error(
                        "Peekable.peek() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![item.clone()],
                }
            }
            _ => self.require_known_method(
                "Iterator",
                method,
                &[
                    "all",
                    "any",
                    "chain",
                    "chunk_by",
                    "chunks",
                    "collect",
                    "count",
                    "cycle",
                    "enumerate",
                    "filter",
                    "flat_map",
                    "fold",
                    "inspect",
                    "map",
                    "next",
                    "peek",
                    "peekable",
                    "scan",
                    "skip",
                    "skip_while",
                    "step_by",
                    "take",
                    "take_while",
                    "windows",
                    "zip",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Regex`.
    /// Regex is interpreter-only (no codegen). All methods are effect-free.
    fn infer_regex_method(&mut self, method: &str, args: &[CallArg], span: &Span) -> Type {
        let match_ty = Type::Named {
            name: "Match".to_string(),
            args: vec![],
        };
        match method {
            "is_match" => {
                if args.len() != 1 {
                    self.type_error(
                        "Regex.is_match() takes 1 argument".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Bool
            }
            "find" => {
                if args.len() != 1 {
                    self.type_error(
                        "Regex.find() takes 1 argument".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![match_ty],
                }
            }
            "find_all" => {
                if args.len() != 1 {
                    self.type_error(
                        "Regex.find_all() takes 1 argument".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Named {
                    name: "Vec".to_string(),
                    args: vec![match_ty],
                }
            }
            "replace_all" => {
                if args.len() != 2 {
                    self.type_error(
                        "Regex.replace_all() takes 2 arguments (s, replacement)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Str
            }
            _ => self.handle_unknown_method(
                "Regex",
                method,
                &["find", "find_all", "is_match", "replace_all"],
                args,
                span,
            ),
        }
    }

    fn infer_http_client_method(&mut self, method: &str, args: &[CallArg], span: &Span) -> Type {
        let response_ty = Type::Named {
            name: "Response".to_string(),
            args: vec![],
        };
        let http_error_ty = Type::Named {
            name: "HttpError".to_string(),
            args: vec![],
        };
        let result_response = Type::Named {
            name: "Result".to_string(),
            args: vec![response_ty, http_error_ty],
        };
        match method {
            "get" => {
                if args.len() != 1 {
                    self.type_error(
                        "Client.get() takes 1 argument (url: str)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                result_response
            }
            "post" => {
                if args.len() != 2 {
                    self.type_error(
                        "Client.post() takes 2 arguments (url: str, body: str)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                result_response
            }
            _ => self.handle_unknown_method("Client", method, &["get", "post"], args, span),
        }
    }

    fn infer_http_response_method(&mut self, method: &str, args: &[CallArg], span: &Span) -> Type {
        match method {
            "status" => {
                if !args.is_empty() {
                    self.type_error(
                        "Response.status() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "body" => {
                if !args.is_empty() {
                    self.type_error(
                        "Response.body() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Str
            }
            "header" => {
                if args.len() != 1 {
                    self.type_error(
                        "Response.header() takes 1 argument (name: str)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![Type::Str],
                }
            }
            _ => self.handle_unknown_method(
                "Response",
                method,
                &["body", "header", "status"],
                args,
                span,
            ),
        }
    }

    fn infer_http_error_method(&mut self, method: &str, args: &[CallArg], span: &Span) -> Type {
        match method {
            "message" => {
                if !args.is_empty() {
                    self.type_error(
                        "HttpError.message() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Str
            }
            _ => self.handle_unknown_method("HttpError", method, &["message"], args, span),
        }
    }

    /// Infer the return type of a method call on `Sender[T]` or `Receiver[T]`.
    /// `is_sender` distinguishes the two ends; `element` is the channel's `T`.
    fn infer_channel_method(
        &mut self,
        is_sender: bool,
        element: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let elem = element.clone();
        let sender_elem = Type::Named {
            name: "Sender".to_string(),
            args: vec![elem.clone()],
        };
        let option_elem = Type::Named {
            name: "Option".to_string(),
            args: vec![elem.clone()],
        };

        if is_sender {
            match method {
                "send" => {
                    for arg in args {
                        let at = self.infer_expr(&arg.value);
                        self.check_assignable(&elem, &at, arg.value.span.clone());
                    }
                    Type::Unit
                }
                "clone" => {
                    if !args.is_empty() {
                        self.type_error(
                            "Sender.clone() takes no arguments".to_string(),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                    }
                    sender_elem
                }
                _ => self.require_known_method("Sender", method, &["clone", "send"], args, span),
            }
        } else {
            // Receiver
            match method {
                "recv" => {
                    if !args.is_empty() {
                        self.type_error(
                            "Receiver.recv() takes no arguments".to_string(),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                    }
                    elem
                }
                "try_recv" => {
                    if !args.is_empty() {
                        self.type_error(
                            "Receiver.try_recv() takes no arguments".to_string(),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                    }
                    option_elem
                }
                _ => {
                    self.require_known_method("Receiver", method, &["recv", "try_recv"], args, span)
                }
            }
        }
    }

    // ── Label Validation ────────────────────────────────────────

    fn validate_labels(&mut self, args: &[CallArg], param_names: &[Option<String>], _span: &Span) {
        let mut seen_label = false;
        let mut seen_unlabeled_after_label = false;

        for (i, arg) in args.iter().enumerate() {
            if let Some(ref label) = arg.label {
                if seen_unlabeled_after_label {
                    self.type_error(
                        "labeled arguments must be contiguous — cannot have unlabeled arguments between labeled ones".to_string(),
                        arg.span.clone(),
                        TypeErrorKind::NonContiguousLabels,
                    );
                }
                seen_label = true;

                // Check label matches parameter name at this position
                if i < param_names.len() {
                    if let Some(ref pname) = param_names[i] {
                        if label != pname {
                            self.type_error(
                                format!(
                                    "label '{}' does not match parameter '{}' at position {}",
                                    label,
                                    pname,
                                    i + 1
                                ),
                                arg.span.clone(),
                                TypeErrorKind::LabelMismatch,
                            );
                        }
                    } else {
                        self.type_error(
                            format!("parameter at position {} cannot be labeled (destructuring pattern)", i + 1),
                            arg.span.clone(),
                            TypeErrorKind::LabelMismatch,
                        );
                    }
                }
            } else if seen_label {
                seen_unlabeled_after_label = true;
            }
        }
    }

    // ── Field Access ────────────────────────────────────────────

    fn infer_field_access(&mut self, object: &Expr, field: &str, span: &Span) -> Type {
        // Primitive-type associated constants — `i64.MAX`, `f64.INFINITY`,
        // `usize.MAX`, etc. The parser emits these as
        // `FieldAccess { object: Identifier("<primitive>"), field: "<NAME>" }`.
        // Intercept here before `infer_expr(object)` would silently return
        // `Type::Error` for the bare primitive identifier. The lookup
        // returns the const's typed surface so downstream code (`let x =
        // i64.MAX;`) sees the right `Type::Int(I64)` etc.
        if let ExprKind::Identifier(name) = &object.kind {
            if let Some(cv) = crate::prelude::lookup_primitive_const(name, field) {
                return primitive_const_type(cv);
            }
        }
        let obj_ty = self.infer_expr(object);
        if obj_ty == Type::Error {
            return Type::Error;
        }

        // Slice 1b: opaque foreign types (`unsafe extern { type Foo; }`)
        // have no fields visible to Kāra — the C side owns the layout, so
        // even `r.field` through a `ref Foo` / `mut ref Foo` has no
        // meaningful resolution. The bare `Type::Named` arm is a defensive
        // belt for typecheck-error-recovery flows; the by-value binding
        // itself would already have fired `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`
        // upstream.
        let opaque_receiver_name = match &obj_ty {
            Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                Type::Named { name, .. } if self.env.opaque_foreign_types.contains(name) => {
                    Some(name.clone())
                }
                _ => None,
            },
            Type::Named { name, .. } if self.env.opaque_foreign_types.contains(name) => {
                Some(name.clone())
            }
            _ => None,
        };
        if let Some(name) = opaque_receiver_name {
            self.type_error(
                format!(
                    "error[E_OPAQUE_TYPE_NO_FIELDS]: opaque foreign type '{name}' \
                     has no fields visible to Kāra; the C side owns the layout. \
                     Field access through `ref {name}` / `mut ref {name}` is not \
                     supported — pass the reference to a foreign function declared \
                     in the same `unsafe extern {{ }}` block instead"
                ),
                span.clone(),
                TypeErrorKind::TypeMismatch,
            );
            return Type::Error;
        }

        let type_name = match &obj_ty {
            Type::Named { name, .. } => name.clone(),
            // Shared-struct receivers (`Type::Shared(name)` — a `shared
            // struct N { ... }`'s value type) carry the same struct
            // definition lookup as a bare `Type::Named { name, args: [] }`.
            // Without this arm, `node.field` on a pattern-bound shared
            // handle falls through to `Type::Error` and silently breaks
            // every downstream consumer (match scrutinee inference,
            // method dispatch, pattern-binding type recording).
            Type::Shared(name) => name.clone(),
            _ => return Type::Error,
        };

        if let Some(struct_info) = self.env.structs.get(&type_name) {
            let struct_info = struct_info.clone();
            for (fname, ftype, is_pub) in &struct_info.fields {
                if fname == field {
                    // CR-18 field-access half: reject non-`pub` field access
                    // on an imported struct from outside the defining module.
                    if !is_pub {
                        self.check_cross_module_field_access(&type_name, field, span);
                    }
                    return ftype.clone();
                }
            }
            let available: Vec<&str> = struct_info
                .fields
                .iter()
                .map(|(n, _, _)| n.as_str())
                .collect();
            self.type_error(
                format!(
                    "no field '{}' on struct '{}', available fields: {}",
                    field,
                    type_name,
                    available.join(", ")
                ),
                span.clone(),
                TypeErrorKind::UndefinedField,
            );
            Type::Error
        } else {
            // Not in the local env, but may be an imported struct — probe the
            // origin module directly so cross-module field access can still
            // be validated for CR-18.
            self.infer_imported_field_access(&type_name, field, span)
        }
    }

    /// Emit `E0221 PrivateTypeInPublicSignature` when a non-`pub` field is
    /// accessed on an imported struct from outside its defining module. For
    /// local structs (and when no `ProgramTree` is attached), silently
    /// accepts the access — slice 6b treats same-module field access as
    /// always allowed.
    fn check_cross_module_field_access(&mut self, struct_name: &str, field: &str, span: &Span) {
        let Some(tree) = self.tree else { return };
        let Some(current_id) = self.current_module else {
            return;
        };
        let current_path = tree.module(current_id).path.clone();

        // Find the defining module. For a local struct, origin == current.
        let origin_path: Vec<String> = match self.type_origins.get(struct_name) {
            Some((path, _, _)) => path.clone(),
            None => current_path.clone(),
        };
        if origin_path == current_path {
            // Same-module access — non-pub fields are always reachable to
            // sibling code.
            return;
        }
        self.type_error(
            format!(
                "private field '{}' of struct '{}' is not visible outside its defining module",
                field, struct_name,
            ),
            span.clone(),
            TypeErrorKind::PrivateTypeInPublicSignature,
        );
    }

    /// Access a field on a struct that isn't registered in the local env —
    /// typically an imported struct from another module. Consults the
    /// `ProgramTree` so we can (a) return the field type and (b) enforce
    /// the cross-module field-visibility rule.
    fn infer_imported_field_access(&mut self, struct_name: &str, field: &str, span: &Span) -> Type {
        let Some(tree) = self.tree else {
            return Type::Error;
        };
        let Some((origin_path, canonical_name, _vis)) = self.type_origins.get(struct_name).cloned()
        else {
            return Type::Error;
        };
        let Some(&origin_id) = tree.graph.by_path.get::<[String]>(&origin_path) else {
            return Type::Error;
        };
        let origin_module = tree.module(origin_id);
        // Look up by the canonical name — `struct_name` here may be an
        // import alias (`import db.Connection as Conn` binds `Conn` but the
        // struct is defined as `Connection`). The canonical name survives
        // the chain walked in `collect_import_origins`.
        let Some(struct_def) = find_struct_def(origin_module, &canonical_name) else {
            return Type::Error;
        };
        let field_def = match struct_def.fields.iter().find(|f| f.name == field) {
            Some(f) => f,
            None => {
                let available: Vec<&str> =
                    struct_def.fields.iter().map(|f| f.name.as_str()).collect();
                self.type_error(
                    format!(
                        "no field '{}' on struct '{}', available fields: {}",
                        field,
                        struct_name,
                        available.join(", ")
                    ),
                    span.clone(),
                    TypeErrorKind::UndefinedField,
                );
                return Type::Error;
            }
        };

        if !field_def.is_pub {
            // `origin_path` is guaranteed to differ from `current_module`'s
            // path because `type_origins` only holds cross-module entries.
            self.type_error(
                format!(
                    "private field '{}' of struct '{}' is not visible outside its defining module",
                    field, struct_name,
                ),
                span.clone(),
                TypeErrorKind::PrivateTypeInPublicSignature,
            );
        }

        // Return the field's declared type. We lower the TypeExpr with an
        // empty generic scope — the origin module's generics are not in
        // scope here, and that's OK for slice-6b's coarse cross-module type
        // surface.
        self.lower_type_expr(&field_def.ty, &[])
    }

    // ── Struct Literals ─────────────────────────────────────────

    fn infer_struct_literal(&mut self, path: &[String], fields: &[FieldInit], span: &Span) -> Type {
        let struct_name = path.last().cloned().unwrap_or_default();

        let struct_info = match self.env.structs.get(&struct_name) {
            Some(info) => info.clone(),
            None => {
                // Type-check field values anyway
                for f in fields {
                    self.infer_expr(&f.value);
                }
                self.type_error(
                    format!("'{}' is not a struct", struct_name),
                    span.clone(),
                    TypeErrorKind::NotAStruct,
                );
                return Type::Error;
            }
        };

        let expected_fields: HashSet<&str> = struct_info
            .fields
            .iter()
            .map(|(n, _, _)| n.as_str())
            .collect();
        let provided_fields: HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();

        // Check for missing fields
        for (fname, _, _) in &struct_info.fields {
            if !provided_fields.contains(fname.as_str()) {
                self.type_error(
                    format!("missing field '{}' in struct '{}'", fname, struct_name),
                    span.clone(),
                    TypeErrorKind::MissingField,
                );
            }
        }

        // Check for extra fields
        for f in fields {
            if !expected_fields.contains(f.name.as_str()) {
                self.type_error(
                    format!("unknown field '{}' in struct '{}'", f.name, struct_name),
                    f.span.clone(),
                    TypeErrorKind::ExtraField,
                );
            }
        }

        // Type-check field values. Use `check_expr` against the field's
        // declared type when known so check-mode coercions (empty
        // `Vec[]` / `Set[]` / `Array[]`, `Into` / `TryInto`, closure
        // pushdown, etc.) fire at struct-field initializer positions.
        // Fall back to synthesis when the field is not declared on the
        // struct (already diagnosed above as an extra field).
        for f in fields {
            if let Some((_, expected_ty, _)) =
                struct_info.fields.iter().find(|(n, _, _)| n == &f.name)
            {
                self.check_expr(&f.value, &expected_ty.clone());
            } else {
                self.infer_expr(&f.value);
            }
        }

        // Shared-struct literals lower to Type::Shared so the literal's
        // type matches an annotated `let s: S = S { ... }` shape and the
        // method-resolution deref step (sub-item 3a) sees a consistent
        // receiver type. Sub-item 2's `lower_path_type` intercept handles
        // the annotation side; this is its construction-site twin.
        if struct_info.is_shared {
            Type::Shared(struct_name)
        } else {
            Type::Named {
                name: struct_name,
                args: Vec::new(),
            }
        }
    }

    // ── Match ───────────────────────────────────────────────────

    /// Check-mode form of `if`. Threads `expected` into both branches so
    /// closures and other check-sensitive shapes inside arms see the
    /// target type. The condition is still synthesized + asserted Bool.
    /// When the `else` branch is missing, the expected type must accept
    /// `Unit` (the synth path's behavior); we delegate to
    /// `check_assignable` for that diagnostic so the message is uniform
    /// with non-branching cases.
    fn check_if_against(
        &mut self,
        condition: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
        expected: &Type,
        span: &Span,
    ) -> Type {
        let cond_ty = self.infer_expr(condition);
        if cond_ty != Type::Bool && cond_ty != Type::Error {
            self.type_error(
                format!(
                    "condition must be 'bool', found '{}'",
                    type_display(&cond_ty)
                ),
                condition.span.clone(),
                TypeErrorKind::ConditionNotBool,
            );
        }
        let then_ty = self.check_block_against(then_block, expected);
        if let Some(else_expr) = else_branch {
            let else_ty = self.check_expr(else_expr, expected);
            // Each branch's check_expr already reported a TypeMismatch
            // against `expected` if it didn't comply; no need to re-check
            // cross-branch compatibility (it's transitive through expected).
            // Pick a non-Never type as the recorded result.
            let result_ty = if then_ty != Type::Never {
                then_ty
            } else {
                else_ty
            };
            self.record_expr_type(span, &result_ty);
            result_ty
        } else {
            // No else: the if returns Unit. Surface the standard
            // assignability diagnostic if the caller expected non-Unit.
            self.check_assignable(expected, &Type::Unit, span.clone());
            self.record_expr_type(span, &Type::Unit);
            Type::Unit
        }
    }

    /// Check-mode form of `if let`. Same shape as `check_if_against`
    /// but binds the pattern's variables in the then-block scope before
    /// checking it. Pattern type-checking against the value's type is
    /// deferred to the synthesis-mode `infer_expr` arm — we mirror its
    /// current behavior (synth value, no pattern type check) so this
    /// slice doesn't change diagnostics around irrefutable-let.
    fn check_if_let_against(
        &mut self,
        pattern: &Pattern,
        value: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
        expected: &Type,
        span: &Span,
    ) -> Type {
        let scrut_ty = self.infer_expr(value);
        // Mirror `infer_if_let`'s pattern binding so the then-block sees
        // the pattern's bindings with their scrutinee-derived types.
        let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
        let dispatch_ty = dispatch_ty.clone();
        self.local_scope.push();
        self.check_pattern_against(pattern, &dispatch_ty, mode);
        let then_ty = self.check_block_against(then_block, expected);
        self.local_scope.pop();
        if let Some(else_expr) = else_branch {
            let else_ty = self.check_expr(else_expr, expected);
            let result_ty = if then_ty != Type::Never {
                then_ty
            } else {
                else_ty
            };
            self.record_expr_type(span, &result_ty);
            result_ty
        } else {
            self.check_assignable(expected, &Type::Unit, span.clone());
            self.record_expr_type(span, &Type::Unit);
            Type::Unit
        }
    }

    /// Check-mode form of `match`. Each arm body is checked against
    /// `expected`. Mirrors `infer_match` for scrutinee/guard/pattern
    /// machinery and exhaustiveness; only the arm-body inference is
    /// replaced with check-mode dispatch. Per-arm assignability
    /// diagnostics from `check_expr` replace the synth path's aggregate
    /// `BranchTypeMismatch` (more specific — points at the offending
    /// arm rather than the whole match).
    fn check_match_against(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expected: &Type,
        span: &Span,
    ) -> Type {
        let scrut_ty = self.infer_expr(scrutinee);
        let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
        let dispatch_ty = dispatch_ty.clone();
        let mut arm_types: Vec<Type> = Vec::new();
        for arm in arms {
            self.local_scope.push();
            self.check_pattern_against(&arm.pattern, &dispatch_ty, mode);
            if let Some(guard) = &arm.guard {
                let guard_ty = self.infer_expr(guard);
                if guard_ty != Type::Bool && guard_ty != Type::Error {
                    self.type_error(
                        format!(
                            "match guard must be 'bool', found '{}'",
                            type_display(&guard_ty)
                        ),
                        guard.span.clone(),
                        TypeErrorKind::ConditionNotBool,
                    );
                }
            }
            let arm_ty = self.check_expr(&arm.body, expected);
            arm_types.push(arm_ty);
            self.local_scope.pop();
        }
        self.check_exhaustiveness(&scrut_ty, arms, span.clone());
        let result_ty = arm_types
            .iter()
            .find(|t| **t != Type::Never)
            .cloned()
            .unwrap_or(Type::Never);
        self.record_expr_type(span, &result_ty);
        result_ty
    }

    fn infer_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], span: &Span) -> Type {
        let scrut_ty = self.infer_expr(scrutinee);
        let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
        let dispatch_ty = dispatch_ty.clone();
        let mut arm_types: Vec<Type> = Vec::new();

        for arm in arms {
            self.local_scope.push();
            self.check_pattern_against(&arm.pattern, &dispatch_ty, mode);
            if let Some(guard) = &arm.guard {
                let guard_ty = self.infer_expr(guard);
                if guard_ty != Type::Bool && guard_ty != Type::Error {
                    self.type_error(
                        format!(
                            "match guard must be 'bool', found '{}'",
                            type_display(&guard_ty)
                        ),
                        guard.span.clone(),
                        TypeErrorKind::ConditionNotBool,
                    );
                }
            }
            let arm_ty = self.infer_expr(&arm.body);
            arm_types.push(arm_ty);
            self.local_scope.pop();
        }

        // Check exhaustiveness for enum types
        self.check_exhaustiveness(&scrut_ty, arms, span.clone());

        // Check all arm types are compatible
        let result_ty = arm_types
            .iter()
            .find(|t| **t != Type::Never)
            .cloned()
            .unwrap_or(Type::Never);

        for arm_ty in &arm_types {
            if *arm_ty != Type::Never
                && *arm_ty != Type::Error
                && result_ty != Type::Error
                && !types_compatible(&result_ty, arm_ty)
            {
                self.type_error(
                    format!(
                        "match arms have incompatible types: '{}' and '{}'",
                        type_display(&result_ty),
                        type_display(arm_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::BranchTypeMismatch,
                );
                break;
            }
        }

        result_ty
    }

    /// Validate a slice/array pattern against the scrutinee type and
    /// return `(element_type, rest_binding_type)` — the per-element type
    /// used to recurse into prefix/suffix sub-patterns, and the type the
    /// `..name` rest binding should receive. Emits per-case diagnostics
    /// (arity overflow, under-coverage without `..`, non-literal `Array`
    /// size, `String` redirect, unsupported scrutinee shape). Returns
    /// `(Type::Error, Type::Error)` on rejection so callers can still
    /// recurse cleanly without crashing. Sub-item 2 of the slice/array-
    /// patterns entry (phase 5.2).
    fn slice_pattern_types(
        &mut self,
        prefix: &[Pattern],
        rest: &Option<RestPattern>,
        suffix: &[Pattern],
        scrutinee_type: &Type,
        span: &Span,
    ) -> (Type, Type) {
        let head = prefix.len();
        let tail = suffix.len();
        let used = head + tail;
        match scrutinee_type {
            Type::Error => (Type::Error, Type::Error),
            Type::Array { element, size } => match size.as_usize() {
                Some(n) => {
                    if used > n {
                        self.type_error(
                            format!(
                                "slice pattern has {used} element{} but \
                                 `Array[_, {n}]` has length {n}; remove \
                                 element patterns or add a `..` marker",
                                if used == 1 { "" } else { "s" },
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        return (Type::Error, Type::Error);
                    }
                    if rest.is_none() && used != n {
                        self.type_error(
                            format!(
                                "slice pattern covers {used} of {n} \
                                 positions on `Array[_, {n}]`; add a `..` \
                                 marker if the remaining positions should \
                                 match anything"
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        return ((**element).clone(), Type::Error);
                    }
                    let remainder = (n - used) as i64;
                    let rest_ty = Type::Array {
                        element: element.clone(),
                        size: ConstArg::Literal(remainder),
                    };
                    ((**element).clone(), rest_ty)
                }
                None => {
                    self.type_error(
                        "slice patterns on `Array[T, N]` require `N` to be \
                         a compile-time literal; const-parameter array \
                         sizes are not yet supported in pattern position"
                            .to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    ((**element).clone(), Type::Error)
                }
            },
            Type::Slice { element, mutable } => {
                let rest_ty = Type::Slice {
                    element: element.clone(),
                    mutable: *mutable,
                };
                ((**element).clone(), rest_ty)
            }
            Type::Named { name, args } if name == "Vec" => {
                let element = args.first().cloned().unwrap_or(Type::Error);
                let rest_ty = Type::Slice {
                    element: Box::new(element.clone()),
                    mutable: false,
                };
                (element, rest_ty)
            }
            Type::Str => {
                self.type_error(
                    "slice patterns do not apply to `String` — UTF-8 is \
                     variable-width per code point, so byte-level positional \
                     patterns would produce invalid boundaries. To match on \
                     string content, convert to `Slice[u8]` with `.bytes()` \
                     or `Iterator[char]` with `.chars()` first"
                        .to_string(),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                (Type::Error, Type::Error)
            }
            _ => {
                self.type_error(
                    format!(
                        "slice patterns apply to `Array[T, N]`, `Vec[T]`, \
                         and `Slice[T]`; cannot match a value of type `{}`",
                        type_display(scrutinee_type)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                (Type::Error, Type::Error)
            }
        }
    }

    /// Walk `pattern` against the scrutinee's structural type `expected`
    /// under `mode` (Owned / Ref / MutRef), binding any introduced names
    /// into `self.local_scope`. The mode is the **match scrutinee's**
    /// borrow form, captured at the match entry by
    /// `ScrutineeMode::classify` after stripping one outer `ref` /
    /// `mut ref` (so the variant / struct / tuple dispatch below keeps
    /// matching the unwrapped shape). `mode` propagates transitively
    /// into every sub-pattern; each leaf binding's type is wrapped via
    /// `ScrutineeMode::wrap_binding_ty` so a `ref T` scrutinee yields
    /// `ref FieldType` bindings — design.md § Match Arm Binding Modes.
    /// Owned scrutinees keep the prior behaviour exactly (no wrap).
    fn check_pattern_against(&mut self, pattern: &Pattern, expected: &Type, mode: ScrutineeMode) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => {
                // Check if this binding name is actually an enum variant
                // (unit variants are parsed as Binding since the parser can't distinguish)
                if let Type::Named {
                    name: enum_name, ..
                } = expected
                {
                    if let Some(enum_info) = self.env.enums.get(enum_name).cloned() {
                        if enum_info.variants.iter().any(|(vn, _)| vn == name) {
                            // It's a unit variant match, not a variable binding
                            return;
                        }
                    }
                }
                let binding_ty = mode.wrap_binding_ty(expected.clone());
                self.local_scope.insert(name.clone(), binding_ty);
                self.record_pattern_binding_borrow_mode(&pattern.span, mode, expected);
                // Mirror bind_pattern_types's side-table write so codegen
                // can reconstitute struct payloads for match-arm bindings.
                // `Type::Str` registers `"String"` parallel to how
                // `Type::Named { name: "Vec" }` registers `"Vec"` —
                // required by the tuple-payload destructure path
                // (`pattern_payload_word_count`) for variant-payload
                // tuples containing String elements (Theme 5, 2026-05-10).
                // `Type::Shared(name)` registers under its bare struct
                // name so codegen's `shared_type_for_expr` lookup finds
                // the heap layout for `node.field` access after a
                // `Some(node)` pattern binding.
                if let Type::Named {
                    name: type_name, ..
                } = expected
                {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                } else if matches!(expected, Type::Str) {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), "String".to_string());
                } else if let Type::Shared(type_name) = expected {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                }
                // PB sibling slice (2026-05-09): mirror
                // `bind_pattern_types`'s sibling-table write so direct
                // method dispatch on a pattern-bound `Vec[T]` / `Slice[T]`
                // payload (the canonical match-arm shape) routes through
                // the right element-typed path.
                self.record_pattern_inner_type(pattern, expected);
            }
            PatternKind::Literal(_) => {
                // Type checking of literal patterns deferred
            }
            PatternKind::TupleVariant { path, patterns } => {
                let variant_name = path.last().cloned().unwrap_or_default();
                if let Type::Named { name, args } = expected {
                    if let Some(enum_info) = self.env.enums.get(name).cloned() {
                        if let Some((_, VariantTypeInfo::Tuple(field_types))) =
                            enum_info.variants.iter().find(|(n, _)| n == &variant_name)
                        {
                            // Substitute the enum's generic params with the
                            // concrete args from the scrutinee's type so
                            // sub-patterns see the resolved payload type
                            // (e.g. `Err(e)` against `Result[i64, MyError]`
                            // sees `e: MyError`, not `e: TypeParam("E")`).
                            let subs: HashMap<String, SubstValue> = enum_info
                                .generic_params
                                .iter()
                                .cloned()
                                .zip(args.iter().cloned().map(SubstValue::Type))
                                .collect();
                            for (pat, ty) in patterns.iter().zip(field_types.iter()) {
                                let resolved = if subs.is_empty() {
                                    ty.clone()
                                } else {
                                    substitute_type_params(ty, &subs)
                                };
                                self.check_pattern_against(pat, &resolved, mode);
                            }
                            return;
                        }
                    }
                }
                // Fallback: bind sub-patterns to Error
                for pat in patterns {
                    self.check_pattern_against(pat, &Type::Error, mode);
                }
            }
            PatternKind::Struct { path, fields } => {
                let struct_name = path.last().cloned().unwrap_or_default();
                // Look up struct or enum variant
                let field_types: Option<Vec<(String, Type)>> =
                    if let Some(info) = self.env.structs.get(&struct_name) {
                        Some(
                            info.fields
                                .iter()
                                .map(|(n, t, _)| (n.clone(), t.clone()))
                                .collect(),
                        )
                    } else if let Type::Named { name, .. } = expected {
                        self.env.enums.get(name).and_then(|e| {
                            e.variants
                                .iter()
                                .find(|(n, _)| n == &struct_name)
                                .and_then(|(_, v)| {
                                    if let VariantTypeInfo::Struct(fields) = v {
                                        Some(fields.clone())
                                    } else {
                                        None
                                    }
                                })
                        })
                    } else {
                        None
                    };

                if let Some(ft) = field_types {
                    for field in fields {
                        let field_ty = ft
                            .iter()
                            .find(|(n, _)| n == &field.name)
                            .map(|(_, t)| t.clone())
                            .unwrap_or(Type::Error);
                        if let Some(ref sub_pattern) = field.pattern {
                            self.check_pattern_against(sub_pattern, &field_ty, mode);
                        } else {
                            // Shorthand: field name becomes binding. Under a
                            // `ref`/`mut ref` scrutinee, the binding type is
                            // wrapped per the match-arm binding-mode rule
                            // (design.md § Match Arm Binding Modes).
                            let binding_ty = mode.wrap_binding_ty(field_ty.clone());
                            self.local_scope.insert(field.name.clone(), binding_ty);
                            // Record borrow mode keyed by the field's span
                            // so codegen can apply the ref-binding shim at
                            // the synthesized leaf binding site (codegen
                            // synthesizes `Pattern { Binding(field.name),
                            // span: field.span }` for shorthand).
                            self.record_pattern_binding_borrow_mode(&field.span, mode, &field_ty);
                        }
                    }
                }
            }
            PatternKind::Tuple(patterns) => {
                if let Type::Tuple(types) = expected {
                    for (pat, ty) in patterns.iter().zip(types.iter()) {
                        self.check_pattern_against(pat, ty, mode);
                    }
                }
            }
            PatternKind::RangePattern { .. } => {
                // Nothing to bind for range patterns
            }
            PatternKind::AtBinding {
                name,
                pattern: inner,
            } => {
                let binding_ty = mode.wrap_binding_ty(expected.clone());
                self.local_scope.insert(name.clone(), binding_ty);
                // The `name @` outer alias is recorded against the outer
                // pattern's span; the inner sub-pattern records its own
                // bindings via the recursive call.
                self.record_pattern_binding_borrow_mode(&pattern.span, mode, expected);
                self.check_pattern_against(inner, expected, mode);
            }
            PatternKind::Or(alternatives) => {
                for alt in alternatives {
                    self.check_pattern_against(alt, expected, mode);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let (element_ty, rest_ty) =
                    self.slice_pattern_types(prefix, rest, suffix, expected, &pattern.span);
                for pat in prefix.iter().chain(suffix.iter()) {
                    self.check_pattern_against(pat, &element_ty, mode);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    // Slice-rest mutability propagation
                    // (design.md § Slice and array patterns > Mutability
                    // propagation, lines 7657–7661): a `..rest` over a
                    // `Vec[T]` / `Slice[T]` produces `Slice[T]`
                    // (immutable) under `Owned` / `Ref` and
                    // `mut Slice[T]` under `MutRef`; over an
                    // `Array[T, N]` the rest binds `Array[T, K]` under
                    // `Owned`, `ref Array[T, K]` under `Ref`, and
                    // `mut ref Array[T, K]` under `MutRef`. Generic
                    // `wrap_binding_ty` skips `Type::Slice` (so struct
                    // fields typed `Slice[T]` are never elevated) —
                    // the rest binding's source is the freshly
                    // synthesised window, so the elevation rule is
                    // applied locally here.
                    let binding_ty = match (mode, rest_ty) {
                        (ScrutineeMode::MutRef, Type::Slice { element, .. }) => Type::Slice {
                            element,
                            mutable: true,
                        },
                        (m, ty) => m.wrap_binding_ty(ty),
                    };
                    self.local_scope.insert(name.clone(), binding_ty);
                }
            }
        }
    }

    fn check_exhaustiveness(&mut self, scrutinee_type: &Type, arms: &[MatchArm], span: Span) {
        use crate::exhaustive::{check_match_exhaustive, unreachable_arms, ExhaustiveResult};
        for idx in unreachable_arms(scrutinee_type, arms, &self.env) {
            self.type_warning(
                "unreachable match arm: pattern is fully covered by an earlier arm".to_string(),
                arms[idx].pattern.span.clone(),
                TypeErrorKind::UnreachableArm,
            );
        }
        match check_match_exhaustive(scrutinee_type, arms, &self.env) {
            ExhaustiveResult::Exhaustive | ExhaustiveResult::Skipped => {}
            ExhaustiveResult::NonExhaustive { witness } => {
                // Preserve the prior diagnostic wording for bool and enum
                // scrutinees when the witness names a single top-level
                // constructor (no nested compound payload). Compound
                // witnesses and non-enum scrutinees use the pattern form.
                let is_simple_witness = !witness.contains('(') && !witness.contains('{');
                let message = match scrutinee_type {
                    Type::Bool if is_simple_witness => {
                        format!("non-exhaustive match on bool: missing {witness}")
                    }
                    Type::Named { name, .. }
                        if is_simple_witness && self.env.enums.contains_key(name) =>
                    {
                        format!("non-exhaustive match: missing variants: {witness}")
                    }
                    _ => format!("non-exhaustive match: pattern `{witness}` not covered"),
                };
                self.type_error(message, span, TypeErrorKind::NonExhaustiveMatch);
            }
        }
    }

    // ── Pattern Binding for Let ─────────────────────────────────

    /// Reverse direction of `lower_type_expr` for the subset needed by the
    /// pattern-binding sibling table (PB sibling slice 2026-05-09): convert
    /// a `Type` back to a synthetic `TypeExpr` so it can be forwarded
    /// through the lowering pass for codegen consumption (it lowers each
    /// surface element type back to an LLVM type via
    /// `llvm_type_for_type_expr`). Coverage: primitive integer / float /
    /// bool / char / str / unit, `Type::Named` (Vec, Slice, Map, struct,
    /// enum names), `Type::Tuple`, `Type::Array`, `Type::Slice`, `Type::Ref`,
    /// `Type::MutRef`, `Type::Shared`, `Type::Rc`, `Type::Arc`,
    /// `Type::TypeParam`. Pieces outside this set (function types, type
    /// vars, assoc projections, errors) fall back to `TypeKind::Error`,
    /// which `llvm_type_for_type_expr` lowers to i64 — adequate for the
    /// element-type registration use case (those payloads are not
    /// supported in pattern-bound Vec/Slice element positions today).
    fn type_to_type_expr(ty: &Type) -> TypeExpr {
        let span = Span::default();
        let path = |name: &str, args: Vec<TypeExpr>| TypeExpr {
            kind: TypeKind::Path(PathExpr {
                segments: vec![name.to_string()],
                generic_args: if args.is_empty() {
                    None
                } else {
                    Some(args.into_iter().map(GenericArg::Type).collect())
                },
                span: span.clone(),
            }),
            span: span.clone(),
        };
        match ty {
            Type::Int(IntSize::I8) => path("i8", vec![]),
            Type::Int(IntSize::I16) => path("i16", vec![]),
            Type::Int(IntSize::I32) => path("i32", vec![]),
            Type::Int(IntSize::I64) => path("i64", vec![]),
            Type::UInt(UIntSize::U8) => path("u8", vec![]),
            Type::UInt(UIntSize::U16) => path("u16", vec![]),
            Type::UInt(UIntSize::U32) => path("u32", vec![]),
            Type::UInt(UIntSize::U64) => path("u64", vec![]),
            Type::UInt(UIntSize::Usize) => path("usize", vec![]),
            Type::Float(FloatSize::F32) => path("f32", vec![]),
            Type::Float(FloatSize::F64) => path("f64", vec![]),
            Type::Bool => path("bool", vec![]),
            Type::Char => path("char", vec![]),
            Type::Str => path("str", vec![]),
            Type::Unit => TypeExpr {
                kind: TypeKind::Unit,
                span,
            },
            Type::Named { name, args } => {
                let arg_exprs = args.iter().map(Self::type_to_type_expr).collect();
                path(name, arg_exprs)
            }
            Type::Shared(name) => path(name, vec![]),
            Type::Rc(inner) => path("Rc", vec![Self::type_to_type_expr(inner)]),
            Type::Arc(inner) => path("Arc", vec![Self::type_to_type_expr(inner)]),
            Type::Tuple(elems) => TypeExpr {
                kind: TypeKind::Tuple(elems.iter().map(Self::type_to_type_expr).collect()),
                span,
            },
            Type::Array { element, size } => TypeExpr {
                kind: TypeKind::Array {
                    element: Box::new(Self::type_to_type_expr(element)),
                    size: Box::new(Expr {
                        kind: ExprKind::Integer(size.as_literal().unwrap_or(0), None),
                        span: span.clone(),
                    }),
                },
                span,
            },
            Type::Slice { element, mutable } => {
                let inner = Box::new(Self::type_to_type_expr(element));
                if *mutable {
                    TypeExpr {
                        kind: TypeKind::MutSlice(inner),
                        span,
                    }
                } else {
                    path("Slice", vec![*inner])
                }
            }
            Type::Ref(inner) => TypeExpr {
                kind: TypeKind::Ref(Box::new(Self::type_to_type_expr(inner))),
                span,
            },
            Type::MutRef(inner) => TypeExpr {
                kind: TypeKind::MutRef(Box::new(Self::type_to_type_expr(inner))),
                span,
            },
            Type::Weak(inner) => TypeExpr {
                kind: TypeKind::Weak(Box::new(Self::type_to_type_expr(inner))),
                span,
            },
            Type::TypeParam(name) => path(name, vec![]),
            // Fallback for shapes that don't have a clean TypeExpr round-trip
            // (TypeVar, Function, OnceFunction, Pointer, AssocProjection,
            // Error). The element-type registration use case never sees
            // these as Vec[T] / Slice[T] inner types in a well-typed
            // program, so falling back to Error → i64 lowering is safe.
            _ => TypeExpr {
                kind: TypeKind::Error,
                span,
            },
        }
    }

    /// If `ty` is `Vec[T]` / `Slice[T]` / `mut Slice[T]`, record the inner
    /// element type at `pattern.span` in the sibling table so codegen can
    /// register it under the binding's variable name (`vec_elem_types` /
    /// `slice_elem_types`). For `Type::Slice` (which isn't a `Type::Named`
    /// shape), also write the canonical `"Slice"` surface name into the
    /// String-name table so codegen's `bind_pattern_values` knows which
    /// element-type registry to populate. `Vec` already gets a String-name
    /// entry from the existing `Type::Named` write. PB sibling slice
    /// (2026-05-09).
    /// Stamp the pattern-binding span with the borrow form derived from
    /// the enclosing `ScrutineeMode`. Owned mode produces no entry. The
    /// `dispatch_ty` is the unwrapped binding type (i.e., the type
    /// _before_ `wrap_binding_ty` re-wraps it) — used to skip recording
    /// when the field's own type is already a borrow shape, mirroring
    /// `wrap_binding_ty`'s pass-through rule (a `ref FieldT` field
    /// through a `ref Container` scrutinee stays `ref FieldT`, not
    /// `ref ref FieldT`, so the codegen shim must not wrap it again).
    fn record_pattern_binding_borrow_mode(
        &mut self,
        span: &Span,
        mode: ScrutineeMode,
        dispatch_ty: &Type,
    ) {
        let borrow = match mode {
            ScrutineeMode::Owned => return,
            ScrutineeMode::Ref => crate::ast::PatternBindingBorrow::Ref,
            ScrutineeMode::MutRef => crate::ast::PatternBindingBorrow::MutRef,
        };
        // Skip already-borrow leaf shapes (parity with
        // `ScrutineeMode::wrap_binding_ty`'s identity arm). A struct
        // field declared `ref T` / `mut ref T` / `Slice[T]` keeps its
        // own borrow form — codegen would already lower it as a
        // pointer / slice header, so the ref-shim would double-wrap.
        if matches!(
            dispatch_ty,
            Type::Ref(_) | Type::MutRef(_) | Type::Slice { .. }
        ) {
            return;
        }
        self.pattern_binding_borrow_modes
            .insert(SpanKey::from_span(span), borrow);
    }

    fn record_pattern_inner_type(&mut self, pattern: &Pattern, ty: &Type) {
        // Tuple bindings (e.g. `Some(node)` where `node: (i64, i64)`):
        // record the whole tuple `TypeExpr` so codegen can reconstruct
        // a tuple struct from the multi-word payload. Without this,
        // `pattern_binding_types` skips anonymous tuple shapes and
        // the codegen's `reconstruct_payload_value` Binding arm falls
        // through to single-word — the downstream `let (a, b) = node`
        // then fails because `node` isn't a struct value.
        if let Type::Tuple(_) = ty {
            let tup_te = Self::type_to_type_expr(ty);
            self.pattern_binding_inner_types
                .insert(SpanKey::from_span(&pattern.span), tup_te);
            self.pattern_binding_types
                .insert(SpanKey::from_span(&pattern.span), "Tuple".to_string());
            return;
        }
        let (elem, name): (Option<&Type>, Option<&'static str>) = match ty {
            // `VecDeque[T]` shares `Vec[T]`'s `{ptr, len, cap}` codegen
            // layout (see `extract_vec_elem_type`), so record it under
            // the same sibling-table path. Without this arm, an untyped
            // `let mut q = VecDeque.new();` leaves `vec_elem_types`
            // empty and `q.push_back(x)` falls through method dispatch.
            Type::Named { name, args }
                if (name == "Vec" || name == "VecDeque") && args.len() == 1 =>
            {
                (Some(&args[0]), None)
            }
            Type::Slice { element, .. } => (Some(element.as_ref()), Some("Slice")),
            _ => (None, None),
        };
        if let Some(elem_ty) = elem {
            let elem_te = Self::type_to_type_expr(elem_ty);
            let key = SpanKey::from_span(&pattern.span);
            self.pattern_binding_inner_types.insert(key, elem_te);
            // Stash the raw `Type` so `finalize_pattern_binding_inner_types`
            // can re-resolve typevars after the function body completes.
            // Bindings whose element type contains a typevar (e.g. `let mut
            // q = VecDeque.new();` — `?T` solved later by `q.push_back(x)`)
            // depend on this re-resolution to surface the right TypeExpr.
            self.pattern_binding_inner_unresolved
                .insert(key, elem_ty.clone());
            if let Some(canon_name) = name {
                self.pattern_binding_types
                    .insert(SpanKey::from_span(&pattern.span), canon_name.to_string());
            }
        }
    }

    /// Resolve any `Type::TypeVar` entries captured by
    /// `record_pattern_inner_type` against `env.substitutions` and
    /// overwrite the public `pattern_binding_inner_types` table. Runs
    /// once at the end of `check`, after every function body has been
    /// inferred and all typevars have either been solved or surfaced as
    /// "cannot infer" diagnostics.
    fn finalize_pattern_binding_inner_types(&mut self) {
        let id_to_name: HashMap<TypeVarId, String> = HashMap::new();
        let const_id_to_name: HashMap<ConstVarId, String> = HashMap::new();
        let updates: Vec<(SpanKey, TypeExpr)> = self
            .pattern_binding_inner_unresolved
            .iter()
            .map(|(key, ty)| {
                let resolved = resolve_type_vars(
                    ty,
                    &self.env.substitutions,
                    &id_to_name,
                    &self.env.const_substitutions,
                    &const_id_to_name,
                );
                (*key, Self::type_to_type_expr(&resolved))
            })
            .collect();
        for (key, te) in updates {
            self.pattern_binding_inner_types.insert(key, te);
        }
    }

    fn bind_pattern_types(&mut self, pattern: &Pattern, ty: &Type) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                self.local_scope.insert(name.clone(), ty.clone());
                // Record the surface type for codegen so it can reconstitute
                // struct payloads from the i64 word at match-arm bind sites
                // (see TypeCheckResult.pattern_binding_types). Named types
                // record under their canonical name; `Type::Str` registers
                // its 3-word `String` surface name parallel to how
                // `Type::Named { name: "Vec" }` registers `"Vec"` —
                // required by the tuple-payload destructure path
                // (`pattern_payload_word_count`) which needs to slice a
                // flat tuple payload into per-element word ranges.
                // Other primitives and references stay unrecorded — their
                // 1-word default matches their actual layout.
                if let Type::Named {
                    name: type_name, ..
                } = ty
                {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                } else if matches!(ty, Type::Str) {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), "String".to_string());
                } else if let Type::Shared(type_name) = ty {
                    // Parallel to `check_pattern_against`: register the
                    // bare struct name so codegen's `shared_type_for_expr`
                    // lookup resolves `node.field` access on a pattern-
                    // bound shared-struct handle.
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                }
                // PB sibling slice (2026-05-09): record the inner element
                // type for `Vec[T]` / `Slice[T]` bindings so codegen can
                // register the LLVM elem type under the binding's variable
                // name (vec_elem_types / slice_elem_types). Lights up
                // direct method dispatch (`xs.len()`, `xs[0]`, `xs.push(...)`)
                // on a pattern-bound collection payload without needing
                // function-arg routing as a work-around.
                self.record_pattern_inner_type(pattern, ty);
            }
            PatternKind::Tuple(patterns) => {
                if let Type::Tuple(types) = ty {
                    if patterns.len() != types.len() {
                        self.type_error(
                            format!(
                                "tuple pattern has {} element(s) but type has {}",
                                patterns.len(),
                                types.len()
                            ),
                            pattern.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        for pat in patterns {
                            self.bind_pattern_types(pat, &Type::Error);
                        }
                    } else {
                        for (pat, t) in patterns.iter().zip(types.iter()) {
                            self.bind_pattern_types(pat, t);
                        }
                    }
                } else if *ty != Type::Error {
                    self.type_error(
                        format!("tuple pattern used but type is `{}`", type_display(ty)),
                        pattern.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    for pat in patterns {
                        self.bind_pattern_types(pat, &Type::Error);
                    }
                } else {
                    for pat in patterns {
                        self.bind_pattern_types(pat, &Type::Error);
                    }
                }
            }
            PatternKind::Struct { path, fields } => {
                let struct_name = path.last().map(String::as_str).unwrap_or("");
                let field_source_ty = if let Type::Named { name, .. } = ty {
                    if name == struct_name || ty == &Type::Error {
                        Some(name.clone())
                    } else if *ty != Type::Error {
                        self.type_error(
                            format!(
                                "struct pattern `{}` used but type is `{}`",
                                struct_name,
                                type_display(ty)
                            ),
                            pattern.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        None
                    } else {
                        None
                    }
                } else if *ty != Type::Error {
                    self.type_error(
                        format!(
                            "struct pattern `{}` used but type is `{}`",
                            struct_name,
                            type_display(ty)
                        ),
                        pattern.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    None
                } else {
                    None
                };
                for field in fields {
                    let field_ty = if let Some(ref sname) = field_source_ty {
                        if let Some(info) = self.env.structs.get(sname) {
                            if let Some((_, t, _)) =
                                info.fields.iter().find(|(n, _, _)| n == &field.name)
                            {
                                t.clone()
                            } else {
                                self.type_error(
                                    format!(
                                        "no field `{}` found on struct `{}`",
                                        field.name, sname
                                    ),
                                    field.span.clone(),
                                    TypeErrorKind::UndefinedField,
                                );
                                Type::Error
                            }
                        } else {
                            Type::Error
                        }
                    } else {
                        Type::Error
                    };
                    if let Some(ref sub) = field.pattern {
                        self.bind_pattern_types(sub, &field_ty);
                    } else {
                        self.local_scope.insert(field.name.clone(), field_ty);
                    }
                }
            }
            PatternKind::TupleVariant { patterns, .. } => {
                for pat in patterns {
                    self.bind_pattern_types(pat, &Type::Error);
                }
            }
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
            PatternKind::AtBinding { name, pattern } => {
                self.local_scope.insert(name.clone(), ty.clone());
                self.bind_pattern_types(pattern, ty);
            }
            PatternKind::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    self.bind_pattern_types(first, ty);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let (element_ty, rest_ty) =
                    self.slice_pattern_types(prefix, rest, suffix, ty, &pattern.span);
                for pat in prefix.iter().chain(suffix.iter()) {
                    self.bind_pattern_types(pat, &element_ty);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    self.local_scope.insert(name.clone(), rest_ty);
                }
            }
        }
    }

    // ── Irrefutability Check for Parameter Patterns ──────────────

    /// Returns true if `pat` is irrefutable for a value of type `ty`. Prefers
    /// the Maranget machinery (`U([PAT], _) == false`) when the type is in
    /// the handled set; falls back to the legacy syntactic check on
    /// `ref`/`function`/`typeparam`/etc. types that Maranget skips. Slice 6
    /// of the exhaustiveness upgrade.
    fn is_irrefutable_pattern(&self, pat: &Pattern, ty: &Type) -> bool {
        match crate::exhaustive::is_pattern_irrefutable(pat, ty, &self.env) {
            Some(b) => b,
            None => self.is_irrefutable_param_pattern(pat),
        }
    }

    /// Legacy syntactic refutability check. Retained as the fallback for
    /// types Maranget doesn't reason about (refs, function values, generic
    /// parameters, etc.). Prefer `is_irrefutable_pattern` when a type is
    /// available.
    fn is_irrefutable_param_pattern(&self, pat: &Pattern) -> bool {
        match &pat.kind {
            PatternKind::Binding(_) | PatternKind::Wildcard => true,
            PatternKind::Tuple(patterns) => patterns
                .iter()
                .all(|p| self.is_irrefutable_param_pattern(p)),
            PatternKind::Struct { path, fields } => {
                // A struct pattern is irrefutable only if the name refers to a
                // struct type (not an enum variant). Enum variant names are
                // refutable — they only match one branch.
                if path.len() == 1 {
                    let name = &path[0];
                    if self.env.structs.contains_key(name) {
                        // Known struct — irrefutable iff all field sub-patterns are
                        fields.iter().all(|f| {
                            f.pattern
                                .as_ref()
                                .is_none_or(|p| self.is_irrefutable_param_pattern(p))
                        })
                    } else if self
                        .env
                        .enums
                        .values()
                        .any(|e| e.variants.iter().any(|(v, _)| v == name))
                    {
                        // Known enum variant name — refutable
                        false
                    } else {
                        // Unknown name — let type errors surface; treat as irrefutable
                        // to avoid double-diagnosing the same source mistake.
                        true
                    }
                } else {
                    false
                }
            }
            PatternKind::AtBinding { pattern, .. } => self.is_irrefutable_param_pattern(pattern),
            PatternKind::Literal(_)
            | PatternKind::RangePattern { .. }
            | PatternKind::TupleVariant { .. }
            | PatternKind::Or(_) => false,
            // Slice patterns are refutable in general — `[]` does not cover
            // `Vec[T]`, and any non-`..` prefix/suffix narrows the matched
            // length class. Sub-item 2 will refine this to `irrefutable iff
            // every nested pattern is irrefutable` for `Array[T, N]` only.
            PatternKind::Slice { .. } => false,
        }
    }

    /// Emit a `RefutablePattern` error if `param`'s pattern is refutable
    /// for its declared type.
    fn check_param_irrefutable(&mut self, param: &Param, ty: &Type) {
        if !self.is_irrefutable_pattern(&param.pattern, ty) {
            self.type_error(
                "refutable pattern in function parameter; use `if let` or `match` for patterns that may not match".to_string(),
                param.pattern.span.clone(),
                TypeErrorKind::RefutablePattern,
            );
        }
    }
}

#[cfg(test)]
mod once_function_carrier_tests {
    use super::*;

    fn fn_i32_to_i32() -> Type {
        Type::Function {
            params: vec![Type::Int(IntSize::I32)],
            return_type: Box::new(Type::Int(IntSize::I32)),
        }
    }

    fn once_fn_i32_to_i32() -> Type {
        Type::OnceFunction {
            params: vec![Type::Int(IntSize::I32)],
            return_type: Box::new(Type::Int(IntSize::I32)),
        }
    }

    #[test]
    fn type_display_prints_oncefn() {
        assert_eq!(type_display(&once_fn_i32_to_i32()), "OnceFn(i32) -> i32");
        assert_eq!(type_display(&fn_i32_to_i32()), "Fn(i32) -> i32");
    }

    #[test]
    fn type_display_oncefn_unit_return_omits_arrow() {
        let no_arg_unit = Type::OnceFunction {
            params: vec![],
            return_type: Box::new(Type::Unit),
        };
        assert_eq!(type_display(&no_arg_unit), "OnceFn()");
    }

    #[test]
    fn type_display_oncefn_multi_param() {
        let multi = Type::OnceFunction {
            params: vec![Type::Int(IntSize::I32), Type::Bool],
            return_type: Box::new(Type::Float(FloatSize::F64)),
        };
        assert_eq!(type_display(&multi), "OnceFn(i32, bool) -> f64");
    }

    #[test]
    fn types_compatible_oncefn_identity() {
        assert!(types_compatible(
            &once_fn_i32_to_i32(),
            &once_fn_i32_to_i32()
        ));
    }

    #[test]
    fn types_compatible_oncefn_rejects_fn_in_either_direction() {
        assert!(!types_compatible(&once_fn_i32_to_i32(), &fn_i32_to_i32()));
        assert!(!types_compatible(&fn_i32_to_i32(), &once_fn_i32_to_i32()));
    }

    #[test]
    fn types_compatible_oncefn_param_arity_mismatch() {
        let one = once_fn_i32_to_i32();
        let two = Type::OnceFunction {
            params: vec![Type::Int(IntSize::I32), Type::Int(IntSize::I32)],
            return_type: Box::new(Type::Int(IntSize::I32)),
        };
        assert!(!types_compatible(&one, &two));
    }

    #[test]
    fn numeric_trait_arms_reject_oncefn() {
        // The trait-bound queries (`type_supports_*`) live on `TypeChecker`, so
        // we build a minimal one against an empty parsed program. With no impls
        // registered, the function-shape arms (now extended with `OnceFunction`)
        // are the ones exercised — verifying we widened the catch-all "false"
        // patterns rather than silently letting `OnceFunction` fall through to
        // permissive arms.
        let parsed = crate::parse("");
        let resolved = crate::resolve(&parsed.program);
        let tc = TypeChecker::new(&parsed.program, &resolved);
        let oncefn = once_fn_i32_to_i32();
        assert!(!tc.type_supports_partial_eq(&oncefn));
        assert!(!tc.type_supports_eq(&oncefn));
        assert!(!tc.type_supports_hash(&oncefn));
        assert!(!tc.type_supports_ord(&oncefn));
        assert!(!tc.type_supports_display(&oncefn));
        assert!(!tc.type_supports_partial_ord(&oncefn));
    }

    #[test]
    fn substitute_type_params_preserves_once() {
        let t_to_t = Type::OnceFunction {
            params: vec![Type::TypeParam("T".to_string())],
            return_type: Box::new(Type::TypeParam("T".to_string())),
        };
        let mut subs = HashMap::new();
        subs.insert("T".to_string(), SubstValue::Type(Type::Bool));
        let resolved = substitute_type_params(&t_to_t, &subs);
        assert_eq!(
            resolved,
            Type::OnceFunction {
                params: vec![Type::Bool],
                return_type: Box::new(Type::Bool),
            }
        );
    }

    #[test]
    fn contains_type_param_handles_oncefn() {
        let with_param = Type::OnceFunction {
            params: vec![Type::TypeParam("T".to_string())],
            return_type: Box::new(Type::Int(IntSize::I32)),
        };
        assert!(contains_type_param(&with_param));

        let no_param = once_fn_i32_to_i32();
        assert!(!contains_type_param(&no_param));
    }

    // ── Type::Shared / Type::Rc / Type::Arc variants ──

    #[test]
    fn test_type_display_shared_rc_arc_variants() {
        let shared = Type::Shared("S".to_string());
        assert_eq!(type_display(&shared), "S");

        let rc_i64 = Type::Rc(Box::new(Type::Int(IntSize::I64)));
        assert_eq!(type_display(&rc_i64), "Rc[i64]");

        let arc_str = Type::Arc(Box::new(Type::Str));
        assert_eq!(type_display(&arc_str), "Arc[String]");
    }

    #[test]
    fn test_types_compatible_rc_not_assignable_to_arc() {
        let rc_i64 = Type::Rc(Box::new(Type::Int(IntSize::I64)));
        let arc_i64 = Type::Arc(Box::new(Type::Int(IntSize::I64)));
        assert!(!types_compatible(&rc_i64, &arc_i64));
        assert!(!types_compatible(&arc_i64, &rc_i64));

        // The legacy structural form `Type::Named { name: "Rc", … }` is
        // a different type now — variants are distinct, even though
        // sub-item 2 hasn't yet migrated callers to construct them.
        let legacy_rc = Type::Named {
            name: "Rc".to_string(),
            args: vec![Type::Int(IntSize::I64)],
        };
        assert!(!types_compatible(&rc_i64, &legacy_rc));
        assert!(!types_compatible(&legacy_rc, &rc_i64));
    }

    #[test]
    fn test_types_compatible_shared_struct_name_match() {
        let shared_s = Type::Shared("S".to_string());
        let shared_s2 = Type::Shared("S".to_string());
        assert!(types_compatible(&shared_s, &shared_s2));

        let shared_t = Type::Shared("T".to_string());
        assert!(!types_compatible(&shared_s, &shared_t));

        // Distinct from the legacy `Type::Named { name: "S", args: [] }`.
        let legacy_s = Type::Named {
            name: "S".to_string(),
            args: vec![],
        };
        assert!(!types_compatible(&shared_s, &legacy_s));
        assert!(!types_compatible(&legacy_s, &shared_s));
    }

    // ── lower_path_type produces Rc / Arc / Shared variants (sub-item 2) ──

    fn build_typechecker(src: &str) -> TypeChecker<'static> {
        // Leak the parsed/resolved data so the TypeChecker borrow is 'static
        // for the duration of the test — fine; the lifetime ends with the
        // test process.
        let parsed: &'static _ = Box::leak(Box::new(crate::parse(src)));
        let resolved: &'static _ = Box::leak(Box::new(crate::resolve(&parsed.program)));
        let mut tc = TypeChecker::new(&parsed.program, resolved);
        tc.build_type_env();
        tc
    }

    fn path_with_args(name: &str, args: Vec<crate::ast::TypeExpr>) -> crate::ast::PathExpr {
        use crate::ast::GenericArg;
        crate::ast::PathExpr {
            segments: vec![name.to_string()],
            generic_args: if args.is_empty() {
                None
            } else {
                Some(args.into_iter().map(GenericArg::Type).collect())
            },
            span: Span::default(),
        }
    }

    fn type_path(name: &str) -> crate::ast::TypeExpr {
        crate::ast::TypeExpr {
            kind: crate::ast::TypeKind::Path(path_with_args(name, vec![])),
            span: Span::default(),
        }
    }

    #[test]
    fn test_lower_rc_path_type_produces_rc_variant() {
        let mut tc = build_typechecker("");
        let path = path_with_args("Rc", vec![type_path("i64")]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(lowered, Type::Rc(Box::new(Type::Int(IntSize::I64))));
    }

    #[test]
    fn test_lower_arc_path_type_produces_arc_variant() {
        let mut tc = build_typechecker("");
        let path = path_with_args("Arc", vec![type_path("String")]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(lowered, Type::Arc(Box::new(Type::Str)));
    }

    #[test]
    fn test_lower_shared_struct_path_type_produces_shared_variant() {
        let mut tc = build_typechecker("shared struct S { val: i64 }");
        let path = path_with_args("S", vec![]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(lowered, Type::Shared("S".to_string()));
    }

    #[test]
    fn test_lower_nonshared_struct_path_type_stays_named() {
        // Cross-check: the shared-struct intercept must not fire for plain
        // structs — sub-item 2's behavior-preserving promise hinges on this.
        let mut tc = build_typechecker("struct P { val: i64 }");
        let path = path_with_args("P", vec![]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(
            lowered,
            Type::Named {
                name: "P".to_string(),
                args: vec![],
            }
        );
    }

    // ── Method resolution: receiver_for_method_lookup deref step (sub-item 3a) ──

    #[test]
    fn test_receiver_for_lookup_strips_ref_wrappers() {
        let foo = Type::Named {
            name: "Foo".to_string(),
            args: vec![],
        };
        // `ref Foo` and `mut ref Foo` deref to `Foo` per design.md
        // § Method Resolution Step 1 — same as before sub-item 3a.
        assert_eq!(
            receiver_for_method_lookup(&Type::Ref(Box::new(foo.clone()))),
            foo
        );
        assert_eq!(
            receiver_for_method_lookup(&Type::MutRef(Box::new(foo.clone()))),
            foo
        );
    }

    #[test]
    fn test_receiver_for_lookup_shared_lowers_to_named() {
        // `Type::Shared(S)` lowers to `Type::Named { name: "S", args: [] }`
        // so the candidate-list lookup feeds into the existing
        // user-defined-struct method-resolution path verbatim.
        let shared = Type::Shared("S".to_string());
        assert_eq!(
            receiver_for_method_lookup(&shared),
            Type::Named {
                name: "S".to_string(),
                args: vec![],
            }
        );
    }

    #[test]
    fn test_receiver_for_lookup_rc_arc_deref_to_inner() {
        // `Rc[Foo]` and `Arc[Foo]` strip the wrapper so the inner type's
        // methods become reachable. Args carry through (e.g.
        // `Rc[Vec[i64]]` → `Vec[i64]`).
        let foo = Type::Named {
            name: "Foo".to_string(),
            args: vec![],
        };
        assert_eq!(
            receiver_for_method_lookup(&Type::Rc(Box::new(foo.clone()))),
            foo
        );
        assert_eq!(
            receiver_for_method_lookup(&Type::Arc(Box::new(foo.clone()))),
            foo
        );

        let vec_i64 = Type::Named {
            name: "Vec".to_string(),
            args: vec![Type::Int(IntSize::I64)],
        };
        assert_eq!(
            receiver_for_method_lookup(&Type::Rc(Box::new(vec_i64.clone()))),
            vec_i64
        );
    }

    #[test]
    fn test_receiver_for_lookup_passthrough_for_other_types() {
        // No-op for types without an outer wrapper — TypeParam, primitive,
        // etc. — so the existing arms in `infer_method_call` (TypeParam
        // dispatch, fallthrough) still receive the original shape.
        let tp = Type::TypeParam("T".to_string());
        assert_eq!(receiver_for_method_lookup(&tp), tp);

        let prim = Type::Int(IntSize::I64);
        assert_eq!(receiver_for_method_lookup(&prim), prim);
    }
}

#[cfg(test)]
mod closure_once_callability_inference_tests {
    //! Round 12.44 (Step 2) — closure-expression once-callability
    //! inference at construction. Verifies the typechecker assigns
    //! `Type::OnceFunction` to closures whose body consumes a captured
    //! outer non-Copy binding, `Type::Function` otherwise (capture-free
    //! / read-only-capture / explicit `ref ||` / `mut ref ||` prefix).
    use super::*;

    /// Type-check `src`, then return the inferred type of the first
    /// `Function` or `OnceFunction` value in `expr_types` — i.e., the
    /// closure expression's recorded type. Closure expressions are the
    /// only places these variants appear in user programs (no surface
    /// `Fn(...)` / `OnceFn(...)` annotation lower path yet).
    fn first_closure_type(src: &str) -> Type {
        let parsed = crate::parse(src);
        let resolved = crate::resolve(&parsed.program);
        let tc = crate::typecheck(&parsed.program, &resolved);
        for ty in tc.expr_types.values() {
            if matches!(ty, Type::Function { .. } | Type::OnceFunction { .. }) {
                return ty.clone();
            }
        }
        panic!(
            "expected a Function/OnceFunction-typed closure expression in `expr_types`; \
             expr_types: {:?}",
            tc.expr_types
        );
    }

    #[test]
    fn closure_captures_and_consumes_infers_oncefn() {
        // `apply(cfg)`: `apply` takes owned non-Copy `Cfg`, so the
        // capture-position `cfg` is in Consuming mode → outer non-Copy
        // → closure is once-callable → `Type::OnceFunction`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = || apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::OnceFunction { .. }),
            "expected OnceFunction; got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_only_reads_capture_infers_fn() {
        // `cfg.name` is a FieldAccess walked in Reading mode at the
        // closure body's top level → the `cfg` identifier-leaf inside
        // is Reading → no consume → repeatable closure → `Function`.
        let src = "struct Cfg { name: i64 }\n\
                   fn make(cfg: Cfg) -> i64 {\n\
                       let h = || cfg.name;\n\
                       cfg.name\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function; got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn capture_free_closure_infers_fn() {
        // No outer references → no captures → trivially repeatable.
        let src = "fn main() {\n\
                       let h = || 42;\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function; got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn explicit_ref_prefix_forces_fn_even_when_body_would_consume() {
        // `ref ||` declares the captures as borrows; the round-12.6
        // repeatable-closure rule says these are NOT once-callable
        // regardless of body shape. The body here would otherwise look
        // consume-y to the walker (call with own param slot), but the
        // explicit prefix short-circuits the walk to `Function`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = ref || apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (ref prefix forces repeatable); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn explicit_mut_ref_prefix_forces_fn_even_when_body_would_consume() {
        // `mut ref ||` declares the captures as mutable borrows; same
        // round-12.6 rule. Body shape that would otherwise infer
        // OnceFn must produce `Function` here.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = mut ref || apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (mut ref prefix forces repeatable); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_consuming_copy_capture_infers_fn() {
        // `apply(n)` where `n` is `i64` (Copy). Even though `n` is in
        // Consuming mode, Copy types never trigger once-callability —
        // a Copy capture is duplicated, not moved, on every invocation.
        let src = "fn apply(x: i64) { }\n\
                   fn make() {\n\
                       let n: i64 = 42;\n\
                       let h = || apply(n);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (Copy capture, not once-callable); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_param_shadowing_outer_non_copy_does_not_capture() {
        // The closure's `cfg` parameter shadows the outer `cfg`, so
        // the body's `apply(cfg)` consumes the PARAM, not a capture.
        // No outer non-Copy is consumed → repeatable.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = |cfg: Cfg| apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (param shadows outer); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_body_local_let_shadows_outer_non_copy_capture() {
        // A `let cfg = ...` inside the closure body shadows the outer
        // `cfg`. Subsequent `apply(cfg)` inside the body consumes the
        // body-local, not the capture → repeatable.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = || {\n\
                           let cfg = Cfg { name: 7 };\n\
                           apply(cfg)\n\
                       };\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (body let shadows capture); got {}",
            type_display(&ty)
        );
    }
}

#[cfg(test)]
mod once_fn_slot_rejection_tests {
    //! Round 12.45 (Step 3) — caller-side rejection of `OnceFn` arguments at
    //! `Fn(...)` and `ref Fn(...)` parameter slots. The slot promises
    //! repeatable invocation; an `OnceFn` value violates that promise. The
    //! diagnostic kind is `OnceFnIntoFnSlot` (E0235); when the argument is
    //! a closure literal that the typechecker has already classified as
    //! once-callable (Step 2), the message also names the consumed capture.
    use super::*;

    fn typecheck_src(src: &str) -> TypeCheckResult {
        let parsed = crate::parse(src);
        let resolved = crate::resolve(&parsed.program);
        crate::typecheck(&parsed.program, &resolved)
    }

    fn errors_of_kind(result: &TypeCheckResult, kind: &TypeErrorKind) -> Vec<TypeError> {
        result
            .errors
            .iter()
            .filter(|e| std::mem::discriminant(&e.kind) == std::mem::discriminant(kind))
            .cloned()
            .collect()
    }

    #[test]
    fn own_fn_slot_rejects_oncefn_closure_literal() {
        // `take(f: Fn())`: owned `Fn()` slot — promises the callee can call
        // `f` any number of times. The closure `|| apply(cfg)` is once-
        // callable (consumes captured non-Copy `cfg`). Step 3 must reject.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one OnceFnIntoFnSlot error; all errors: {:?}",
            result.errors
        );
        assert!(
            hits[0].message.contains("once-callable"),
            "expected message to mention 'once-callable'; got '{}'",
            hits[0].message
        );
        assert!(
            hits[0].message.contains("'cfg'") || hits[0].message.contains("captured binding"),
            "expected message to name the consumed capture 'cfg'; got '{}'",
            hits[0].message
        );
    }

    #[test]
    fn own_fn_slot_accepts_repeatable_closure() {
        // Capture-free closure → `Type::Function` → fits an own `Fn()` slot.
        let src = "fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       take(|| { });\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot error for repeatable closure; got: {:?}",
            hits
        );
    }

    #[test]
    fn own_fn_slot_accepts_explicit_ref_prefix_closure_via_binding() {
        // `ref ||` forces repeatable per round 12.6 even when the body
        // would otherwise look consume-y. `ref` is not legal at call
        // sites (parser rejects), so the closure must be let-bound first;
        // the binding gets `Type::Function`, which the own-Fn slot accepts.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let h = ref || apply(cfg);\n\
                       take(h);\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot error for ref-prefix-bound closure; got: {:?}",
            hits
        );
    }

    #[test]
    fn ref_fn_slot_rejects_oncefn_closure_literal() {
        // `ref Fn()` slot — same once-callability constraint; the callee
        // can dispatch through the ref repeatedly, so a once-callable
        // closure value must be rejected. The closure literal types as
        // bare `OnceFn()`, the slot is `ref Fn()`; the unwrapped shape
        // (Fn vs OnceFn) flags the once-callability violation rather than
        // the ref-vs-bare regular mismatch.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: ref Fn()) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected OnceFnIntoFnSlot error for ref-Fn slot rejection; all errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn cross_call_oncefn_through_fn_slot_rejects_at_inner_site() {
        // Inner `inner(cb: Fn())` — a Fn slot. Outer `forward(cb: Fn())`
        // forwards `cb` to inner. Caller passes a once-callable closure to
        // forward — already a Step-3 violation at the OUTER call site. The
        // test pins that the diagnostic kind fires at the user-visible
        // call site (forward(...)), regardless of how many forwarding
        // hops the typechecker would chase.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn inner(cb: Fn()) { cb() }\n\
                   fn forward(cb: Fn()) { inner(cb) }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       forward(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected at least one OnceFnIntoFnSlot error in cross-call forwarding; \
             all errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn method_call_fn_slot_rejects_oncefn_closure_literal() {
        // Method-call slot rejection — the same `Fn()` rule applies to
        // method parameter slots, since the dispatch site routes through
        // `check_call_args_with_substitution` and ultimately
        // `check_assignable`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   struct Runner { }\n\
                   impl Runner {\n\
                       fn drive(self, f: Fn()) { f() }\n\
                   }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let r = Runner { };\n\
                       r.drive(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected OnceFnIntoFnSlot error for method-call Fn-slot rejection; \
             all errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn no_typemismatch_double_report_when_oncefn_slot_violation_fires() {
        // The OnceFnIntoFnSlot kind replaces the generic TypeMismatch for
        // this specific shape — emitting both would double-report. The
        // single-error invariant is what makes the new diagnostic useful;
        // this test pins it.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let once_hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        let mismatch_hits = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert_eq!(once_hits.len(), 1);
        // The TypeMismatch kind may still appear for unrelated reasons,
        // but not for the same span as the OnceFn slot violation.
        let once_span = once_hits[0].span.clone();
        for tm in &mismatch_hits {
            assert!(
                tm.span != once_span,
                "TypeMismatch double-reported at OnceFn slot violation span: {:?}",
                tm
            );
        }
    }

    #[test]
    fn diagnostic_includes_three_concrete_fix_hints() {
        // Round 12.47 (Step 5a) — diagnostic polish. The OnceFnIntoFnSlot
        // message must offer the three concrete fixes documented in the
        // implementation checklist: clone the consumed capture, restructure
        // to keep the closure local, or change the slot type to `OnceFn`.
        // Pin each phrase so future edits to the message body don't silently
        // drop a fix hint.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert_eq!(hits.len(), 1, "all errors: {:?}", result.errors);
        let msg = &hits[0].message;
        assert!(
            msg.contains("clone the captured value"),
            "missing clone hint; got '{}'",
            msg
        );
        assert!(
            msg.contains("invoke the closure locally") || msg.contains("restructure"),
            "missing restructure-locally hint; got '{}'",
            msg
        );
        assert!(
            msg.contains("`OnceFn(...)`") || msg.contains("OnceFn(...)"),
            "missing OnceFn-slot-change hint; got '{}'",
            msg
        );
    }
}

#[cfg(test)]
mod once_fn_container_slot_tests {
    //! Round 12.46 (Step 4) — once-callability rejection at container element
    //! slots, plus surface `OnceFn(...)` annotation acceptance and for-loop
    //! iteration parity over `Vec[Fn]` and `Vec[OnceFn]`. The active rejection
    //! is at the *insert* (`.push`); iteration falls out for free because
    //! `for f in vec` types `f` as the element type, and Step 1's `Call`
    //! dispatch already accepts both `Function` and `OnceFunction` callees.
    use super::*;

    fn typecheck_src(src: &str) -> TypeCheckResult {
        let parsed = crate::parse(src);
        let resolved = crate::resolve(&parsed.program);
        crate::typecheck(&parsed.program, &resolved)
    }

    fn errors_of_kind(result: &TypeCheckResult, kind: &TypeErrorKind) -> Vec<TypeError> {
        result
            .errors
            .iter()
            .filter(|e| std::mem::discriminant(&e.kind) == std::mem::discriminant(kind))
            .cloned()
            .collect()
    }

    #[test]
    fn vec_fn_push_rejects_oncefn_closure_literal() {
        // `Vec[Fn()]` element slot — pushing a once-callable closure must
        // reject at the call site of `.push` because the slot promises
        // repeatable invocation. Routes through the new Vec.push slot
        // dispatch into `check_assignable`, which fires `OnceFnIntoFnSlot`
        // (E0235) via Step 3's logic.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one OnceFnIntoFnSlot error at Vec[Fn].push site; \
             all errors: {:?}",
            result.errors
        );
        assert!(
            hits[0].message.contains("once-callable"),
            "expected 'once-callable' in message; got '{}'",
            hits[0].message
        );
        assert!(
            hits[0].message.contains("'cfg'") || hits[0].message.contains("captured binding"),
            "expected consumed-capture name 'cfg' in message; got '{}'",
            hits[0].message
        );
    }

    #[test]
    fn vec_fn_push_accepts_repeatable_closure() {
        // Capture-free closure → `Type::Function` → fits `Vec[Fn()]` element.
        let src = "fn main() {\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(|| { });\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot for repeatable closure push; got: {:?}",
            hits
        );
        // Also confirm no TypeMismatch crept in for the push arg.
        let mismatch = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert!(
            mismatch.is_empty(),
            "expected no TypeMismatch errors; got: {:?}",
            mismatch
        );
    }

    #[test]
    fn vec_oncefn_push_accepts_once_callable_closure() {
        // Surface `OnceFn(...)` annotation (round 12.46 Step 4) lets the
        // user opt into a Vec whose element slot accepts once-callable
        // closures. Pushing a closure that consumes a captured non-Copy
        // binding now fits the slot — `OnceFunction` ⇄ `OnceFunction`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let mut v: Vec[OnceFn()] = Vec.new();\n\
                       v.push(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot for OnceFn-into-OnceFn slot; got: {:?}",
            hits
        );
        let mismatch = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert!(
            mismatch.is_empty(),
            "expected no TypeMismatch for OnceFn-into-OnceFn slot; got: {:?}",
            mismatch
        );
    }

    #[test]
    fn vec_oncefn_slot_accepts_function_closure_via_subsumption() {
        // Item 131 sub-step 3 (bidirectional subsumption): a Function-typed
        // closure (repeatable) flows into a Vec[OnceFn] slot. Fn is a subtype
        // of OnceFn — a repeatable callable trivially satisfies the
        // callable-once contract. `is_subtype(OnceFunction, Function)` returns
        // true at check_assignable, so neither TypeMismatch nor
        // OnceFnIntoFnSlot fires.
        //
        // Pre-sub-step-3 this fired TypeMismatch (the old test name was
        // `vec_oncefn_annotation_lowers_to_once_function_type` — which
        // observed the rejection as a side effect of the symmetric
        // types_compatible cross-pair rejection). The annotation is still
        // correctly lowered to OnceFunction; what changed is that the upward
        // direction is now admitted at the slot.
        let src = "fn main() {\n\
                       let mut v: Vec[OnceFn() -> i64] = Vec.new();\n\
                       v.push(|| 7);\n\
                   }";
        let result = typecheck_src(src);
        let mismatch = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert!(
            mismatch.is_empty(),
            "Function → OnceFn slot is admitted by sub-step 3 subsumption; \
             expected no TypeMismatch but got: {:?}",
            mismatch
        );
        let once_hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            once_hits.is_empty(),
            "OnceFnIntoFnSlot must not fire for Function → OnceFn (only the \
             reverse direction is the round-12.45 case); got: {:?}",
            once_hits
        );
    }

    #[test]
    fn for_loop_over_vec_fn_invokes_repeatedly() {
        // Iteration over `Vec[Fn()]` yields `f: Fn()` per iteration. The
        // body's `f()` call dispatches against `Type::Function`, which
        // Step 1 made first-class for callee dispatch. No OnceFn ever
        // appears in this path because the slot at insert time was Fn.
        let src = "fn main() {\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(|| { });\n\
                       v.push(|| { });\n\
                       for f in v {\n\
                           f();\n\
                       }\n\
                   }";
        let result = typecheck_src(src);
        assert!(
            result.errors.is_empty(),
            "expected clean typecheck; got errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn for_loop_over_vec_oncefn_invokes_each_element() {
        // Iteration over `Vec[OnceFn()]` yields `f: OnceFn()` per
        // iteration. The typechecker's Call dispatch matches
        // `Function | OnceFunction`, so the body's `f()` succeeds. Each
        // iteration owns its element (move semantics) so calling once is
        // fine; the body invokes f exactly once.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg1 = Cfg { name: 1 };\n\
                       let cfg2 = Cfg { name: 2 };\n\
                       let mut v: Vec[OnceFn()] = Vec.new();\n\
                       v.push(|| apply(cfg1));\n\
                       v.push(|| apply(cfg2));\n\
                       for f in v {\n\
                           f();\n\
                       }\n\
                   }";
        let result = typecheck_src(src);
        assert!(
            result.errors.is_empty(),
            "expected clean typecheck for Vec[OnceFn] iter+invoke; got: {:?}",
            result.errors
        );
    }

    #[test]
    fn vec_fn_push_oncefn_through_intermediate_binding_still_rejects() {
        // The closure is bound to a let first, then pushed. The let's
        // binding type infers to OnceFunction (Step 2) and the push slot
        // check sees OnceFunction → Function and fires E0235. This pins
        // that the Vec.push slot check does not depend on the argument
        // being a closure literal — any once-callable value flowing into
        // the slot rejects.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let h = || apply(cfg);\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(h);\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected OnceFnIntoFnSlot when pushing a let-bound once-callable \
             closure into Vec[Fn]; all errors: {:?}",
            result.errors
        );
    }
}
