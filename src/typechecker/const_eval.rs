//! Const-expression evaluator and helpers.
//!
//! Houses the `ConstEvalError` enum and the pure functions that drive
//! constant evaluation: arithmetic / comparison / logical operator
//! application, integer packing into typed `ConstValue` shapes, glyph
//! lookup for diagnostic rendering, and the `ConstValue ↔ Type` /
//! `ConstArg` mapping helpers consumed by the typechecker's
//! `eval_const_expr*` methods.
//!
//! The `TypeChecker::eval_const_expr*` driver methods remain in
//! `super` for the first extraction pass — they thread `self` state
//! (errors, type_env, resolve_result) that hasn't yet been audited for
//! `pub(super)` field visibility. A later commit will lift them here.

use crate::ast::{BinOp, Expr, ExprKind, UnaryOp};
use crate::prelude::ConstValue;
use crate::token::{IntSuffix, Span};
use std::collections::HashMap;

use super::types::{ConstArg, FloatSize, IntSize, SubstValue, Type, UIntSize};

/// Const generics slice 3 sub-step (g): substitute `ConstArg::ConstParam(name)`
/// against the typechecker's `SubstValue` map (slice 1 fork F1). When the
/// map binds `name → Const(cv)`, the result becomes a `Literal` carrying
/// the resolved value coerced through `i64`. `Literal` and `ConstVar`
/// pass through unchanged.
pub(super) fn substitute_const_arg(arg: &ConstArg, subs: &HashMap<String, SubstValue>) -> ConstArg {
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
fn const_value_to_i64(cv: &ConstValue) -> Option<i64> {
    use ConstValue::*;
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
        lhs: ConstValue,
        rhs: ConstValue,
        span: Span,
    },
    /// `checked_neg` returned `None` for a unary operation.
    UnaryOverflow {
        op: UnaryOp,
        operand: ConstValue,
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
pub(super) fn const_value_to_array_size(cv: &ConstValue) -> Option<usize> {
    use ConstValue::*;
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
pub(super) fn format_const_value(cv: &ConstValue) -> String {
    use ConstValue::*;
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
pub(super) fn binop_glyph(op: &BinOp) -> &'static str {
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
pub(super) fn unaryop_glyph(op: &UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "!",
        UnaryOp::BitNot => "~",
        UnaryOp::Deref => "*",
    }
}

/// Type-tag for a `ConstValue` payload. Mirrors `primitive_const_type` for
/// the const-eval path so error diagnostics carry the right surface type.
pub(super) fn const_value_type(cv: &ConstValue) -> Type {
    use ConstValue::*;
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
pub(super) fn infer_operand_target_ty(left: &Expr, right: &Expr) -> Option<Type> {
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
pub(super) fn integer_to_const_value(
    n: i64,
    target_ty: &Type,
    span: &Span,
) -> Result<ConstValue, ConstEvalError> {
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
pub(super) fn apply_unary(
    op: UnaryOp,
    val: ConstValue,
    span: &Span,
) -> Result<ConstValue, ConstEvalError> {
    use ConstValue::*;
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
pub(super) fn apply_binary(
    op: BinOp,
    lhs: ConstValue,
    rhs: ConstValue,
    span: &Span,
) -> Result<ConstValue, ConstEvalError> {
    use ConstValue::*;
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
    lhs: ConstValue,
    rhs: ConstValue,
    span: &Span,
) -> Result<ConstValue, ConstEvalError> {
    use ConstValue::*;
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
    let incomparable = |l: &ConstValue, r: &ConstValue| ConstEvalError::CompareIncomparable {
        lhs_ty: const_value_type(l),
        rhs_ty: const_value_type(r),
        span: span.clone(),
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
    lhs: ConstValue,
    rhs: ConstValue,
    span: &Span,
) -> Result<ConstValue, ConstEvalError> {
    use ConstValue::*;
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

/// Map a primitive-type associated constant value to its surface `Type`.
/// Used by `infer_field_access` to resolve `i64.MAX` / `f64.INFINITY` /
/// `usize.MAX` etc. to the correct numeric type. The interpreter and
/// codegen consume the same `ConstValue` for the runtime / LLVM value.
pub(super) fn primitive_const_type(cv: &ConstValue) -> Type {
    use ConstValue::*;
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
