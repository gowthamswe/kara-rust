//! AST lowering pass: rewrites operator expressions into trait-method calls.
//!
//! Runs between typecheck and effectcheck. Consumes the typechecker's
//! `expr_types` side-table to resolve operand types, then rewrites
//! `BinOp` / `UnaryOp` nodes into `Call(Path([target_type, method]), args)`
//! shape. Downstream phases (effect, ownership, interpreter, codegen) see
//! a uniform Call shape — no special-case handling for "is this a primitive
//! op" is needed at semantic-analysis layers.
//!
//! v2 scope: arithmetic (`+ - * / %`), unary `Neg`, equality (`== !=`),
//! comparison (`< <= > >=`), bitwise (`& | ^ << >> ~`), and unary `Not` are
//! lowered. Logical `&&` / `||` stay as `BinOp` — short-circuit semantics
//! can't be faithfully expressed as a trait-method call (evaluating both
//! arms eagerly would change program meaning). Range operators stay as
//! `BinOp` too (they construct Range types, not trait calls).

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;
use crate::typechecker::{FloatSize, IntSize, Type, TypeCheckResult, UIntSize};

/// Rewrite operator expressions across the entire program in place.
pub fn lower_program(program: &mut Program, tc: &TypeCheckResult) {
    let mut lowerer = Lowerer { tc };
    for item in &mut program.items {
        lowerer.lower_item(item);
    }
    // Forward the typechecker's `?` cross-error-type conversion table onto
    // the program so codegen can read it without taking a `TypeCheckResult`.
    // Keys are `(span.offset, span.length)`; values are the target type's
    // canonical name (e.g. `"AppError"`). Codegen emits `Target.from(e)`
    // before the propagation early-return when an entry exists.
    program.question_conversions = tc
        .question_conversions
        .iter()
        .map(|(k, v)| ((k.0, k.1), v.clone()))
        .collect();
    // Forward the method-call → `Type.method` callee key table so codegen
    // can narrow the par-branch cancel check at instance method sites.
    // The typechecker populates this directly because the parser sets
    // `MethodCall.span == receiver.span`, so a generic post-hoc walk
    // against `expr_types` would race with the return-type insertion at
    // the same key. See typechecker `infer_method_call`.
    program.method_callee_types = tc
        .method_callee_types
        .iter()
        .map(|(k, v)| ((k.0, k.1), v.clone()))
        .collect();
    // Forward the pattern-binding type table so codegen can reconstitute
    // struct payloads (single-field error wrappers, etc.) from the i64
    // word at match-arm bind sites. Without this, `Err(e) => e.field`
    // can't dispatch through the struct shape because `e` was bound as
    // i64. See `bind_pattern_values` in src/codegen.rs.
    program.pattern_binding_types = tc
        .pattern_binding_types
        .iter()
        .map(|(k, v)| ((k.0, k.1), v.clone()))
        .collect();
}

struct Lowerer<'a> {
    tc: &'a TypeCheckResult,
}

impl<'a> Lowerer<'a> {
    fn lower_item(&mut self, item: &mut Item) {
        match item {
            Item::Function(f) => self.lower_function(f),
            Item::ImplBlock(imp) => {
                for it in &mut imp.items {
                    if let ImplItem::Method(m) = it {
                        self.lower_function(m);
                    }
                }
            }
            Item::ConstDecl(c) => self.lower_expr(&mut c.value),
            _ => {}
        }
    }

    fn lower_function(&mut self, f: &mut Function) {
        self.lower_block(&mut f.body);
    }

    fn lower_block(&mut self, block: &mut Block) {
        for stmt in &mut block.stmts {
            self.lower_stmt(stmt);
        }
        if let Some(ref mut e) = block.final_expr {
            self.lower_expr(e);
        }
    }

    fn lower_stmt(&mut self, stmt: &mut Stmt) {
        match &mut stmt.kind {
            StmtKind::Let { value, .. } | StmtKind::LetElse { value, .. } => {
                self.lower_expr(value);
                if let StmtKind::LetElse { else_block, .. } = &mut stmt.kind {
                    self.lower_block(else_block);
                }
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.lower_block(body);
            }
            StmtKind::Assign { target, value } => {
                self.lower_expr(target);
                self.lower_expr(value);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                self.lower_expr(target);
                self.lower_expr(value);
            }
            StmtKind::Expr(e) => self.lower_expr(e),
        }
    }

    /// Lower an expression: recurse into children first (bottom-up), then
    /// rewrite the current node if it's a lowerable operator.
    fn lower_expr(&mut self, expr: &mut Expr) {
        // Recurse into sub-expressions first.
        match &mut expr.kind {
            ExprKind::Binary { left, right, .. } => {
                self.lower_expr(left);
                self.lower_expr(right);
            }
            ExprKind::Unary { operand, .. } => self.lower_expr(operand),
            ExprKind::Question(inner) => self.lower_expr(inner),
            ExprKind::OptionalChain { object, args, .. } => {
                self.lower_expr(object);
                if let Some(args) = args {
                    for a in args {
                        self.lower_expr(&mut a.value);
                    }
                }
            }
            ExprKind::NilCoalesce { left, right } => {
                self.lower_expr(left);
                self.lower_expr(right);
            }
            ExprKind::Call { callee, args } => {
                self.lower_expr(callee);
                for a in args {
                    self.lower_expr(&mut a.value);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.lower_expr(object);
                for a in args {
                    self.lower_expr(&mut a.value);
                }
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.lower_expr(object);
            }
            ExprKind::Index { object, index } => {
                self.lower_expr(object);
                self.lower_expr(index);
            }
            ExprKind::Block(b) => self.lower_block(b),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.lower_expr(condition);
                self.lower_block(then_block);
                if let Some(eb) = else_branch {
                    self.lower_expr(eb);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.lower_expr(value);
                self.lower_block(then_block);
                if let Some(eb) = else_branch {
                    self.lower_expr(eb);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.lower_expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &mut arm.guard {
                        self.lower_expr(g);
                    }
                    self.lower_expr(&mut arm.body);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.lower_expr(condition);
                self.lower_block(body);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.lower_expr(value);
                self.lower_block(body);
            }
            ExprKind::For { iterable, body, .. } => {
                self.lower_expr(iterable);
                self.lower_block(body);
            }
            ExprKind::Loop { body, .. } => self.lower_block(body),
            ExprKind::LabeledBlock { body, .. } => self.lower_block(body),
            ExprKind::Closure { body, .. } => self.lower_expr(body),
            ExprKind::Return(opt) => {
                if let Some(e) = opt {
                    self.lower_expr(e);
                }
            }
            ExprKind::Break { value, .. } => {
                if let Some(e) = value {
                    self.lower_expr(e);
                }
            }
            ExprKind::Continue { .. } => {}
            ExprKind::Tuple(es) | ExprKind::ArrayLiteral(es) => {
                for e in es {
                    self.lower_expr(e);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.lower_expr(value);
                self.lower_expr(count);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.lower_expr(e);
                }
                // `Array[e1, e2, …]` is just the prefix-named form of the
                // bare `[e1, e2, …]` array literal — canonicalize it here so
                // codegen has a single ArrayLiteral arm to handle. Vec / Set
                // / Map prefix forms stay as PrefixCollectionLiteral; their
                // codegen paths consume the type-name marker.
                if let ExprKind::PrefixCollectionLiteral { type_name, items } = &mut expr.kind {
                    if type_name == "Array" {
                        let lowered_items = std::mem::take(items);
                        expr.kind = ExprKind::ArrayLiteral(lowered_items);
                    }
                }
            }
            ExprKind::MapLiteral(pairs) => {
                for (k, v) in pairs {
                    self.lower_expr(k);
                    self.lower_expr(v);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.lower_expr(&mut f.value);
                }
                if let Some(s) = spread {
                    self.lower_expr(s);
                }
            }
            ExprKind::Pipe { left, right } => {
                self.lower_expr(left);
                self.lower_expr(right);
            }
            ExprKind::Cast { expr: inner, .. } => self.lower_expr(inner),
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.lower_expr(s);
                }
                if let Some(e) = end {
                    self.lower_expr(e);
                }
            }
            ExprKind::Unsafe(b) | ExprKind::Try(b) | ExprKind::Seq(b) | ExprKind::Par(b) => {
                self.lower_block(b)
            }
            ExprKind::Lock { body, .. } => self.lower_block(body),
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.lower_expr(&mut b.value);
                }
                self.lower_block(body);
            }
            // Leaf nodes — nothing to recurse into.
            ExprKind::Integer(_, _)
            | ExprKind::Float(_, _)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_)
            | ExprKind::InterpolatedStringLit(_)
            | ExprKind::Bool(_)
            | ExprKind::Identifier(_)
            | ExprKind::Path { .. }
            | ExprKind::SelfValue
            | ExprKind::SelfType
            | ExprKind::PipePlaceholder
            | ExprKind::Error => {}
        }

        // Then rewrite this node if it's an operator we lower.
        if let Some(rewritten) = self.try_rewrite(expr) {
            expr.kind = rewritten;
        }
    }

    /// Inspect an expression and return a rewritten `ExprKind` if it's an
    /// operator that should be lowered to a trait-method call. v1: arithmetic
    /// binary ops + unary `Neg` only.
    fn try_rewrite(&self, expr: &Expr) -> Option<ExprKind> {
        match &expr.kind {
            ExprKind::Binary { op, left, right } => {
                self.rewrite_binary(op, left, right, &expr.span)
            }
            ExprKind::Unary { op, operand } => self.rewrite_unary(op, operand, &expr.span),
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => self
                .rewrite_into_call(object, method, args, &expr.span)
                .or_else(|| self.rewrite_try_into_call(object, method, args, &expr.span)),
            ExprKind::Call { callee, args } => {
                self.rewrite_bare_assoc_fn_call(callee, args, &expr.span)
            }
            _ => None,
        }
    }

    /// Rewrite a bare-identifier associated-function call (`name(args)`) to
    /// `Target.name(args)` when the typechecker resolved the target via
    /// expected-type inference. Backed by `bare_assoc_fn_targets` populated in
    /// `try_apply_expected_assoc_fn_inference`. The rewritten call dispatches
    /// through the existing impl table at the interpreter / codegen layer.
    fn rewrite_bare_assoc_fn_call(
        &self,
        callee: &Expr,
        args: &[CallArg],
        span: &Span,
    ) -> Option<ExprKind> {
        let name = match &callee.kind {
            ExprKind::Identifier(n) => n.clone(),
            _ => return None,
        };
        let target = self
            .tc
            .bare_assoc_fn_targets
            .get(&SpanKey::from_span(span))?
            .clone();
        let new_callee = Box::new(Expr {
            span: callee.span.clone(),
            kind: ExprKind::Path {
                segments: vec![target, name],
                generic_args: None,
            },
        });
        Some(ExprKind::Call {
            callee: new_callee,
            args: args.to_vec(),
        })
    }

    /// Rewrite `x.into()` to `Target.from(x)` when the typechecker recorded a
    /// target type in `into_conversions`. The `Into` blanket impl is purely a
    /// lowering rewrite — no impl entries are materialized at type-check time.
    fn rewrite_into_call(
        &self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Option<ExprKind> {
        if method != "into" || !args.is_empty() {
            return None;
        }
        let target = self.tc.into_conversions.get(&SpanKey::from_span(span))?;
        Some(call_path(
            vec![target.clone(), "from".to_string()],
            vec![object.clone()],
        ))
    }

    /// Rewrite `x.try_into()` to `Target.try_from(x)` when the typechecker
    /// recorded a target type in `try_into_conversions`. Same desugar
    /// architecture as `rewrite_into_call` — `TryInto` is not materialized
    /// in the impl table; user impls `TryFrom` and the desugar routes
    /// through it.
    fn rewrite_try_into_call(
        &self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Option<ExprKind> {
        if method != "try_into" || !args.is_empty() {
            return None;
        }
        let target = self
            .tc
            .try_into_conversions
            .get(&SpanKey::from_span(span))?;
        Some(call_path(
            vec![target.clone(), "try_from".to_string()],
            vec![object.clone()],
        ))
    }

    fn rewrite_binary(
        &self,
        op: &BinOp,
        left: &Expr,
        right: &Expr,
        _span: &Span,
    ) -> Option<ExprKind> {
        let method = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "div",
            BinOp::Mod => "rem",
            BinOp::Eq => "eq",
            BinOp::NotEq => "ne",
            BinOp::Lt => "lt",
            BinOp::LtEq => "le",
            BinOp::Gt => "gt",
            BinOp::GtEq => "ge",
            BinOp::BitAnd => "bitand",
            BinOp::BitOr => "bitor",
            BinOp::BitXor => "bitxor",
            BinOp::Shl => "shl",
            BinOp::Shr => "shr",
            // Defer: logical (short-circuit), range (constructs Range type).
            BinOp::And | BinOp::Or | BinOp::Range | BinOp::RangeInclusive => return None,
        };

        let lhs_ty = self.type_at(&left.span)?;
        let rhs_ty = self.type_at(&right.span)?;
        // Homogeneity: only lower when both operands are the same type.
        // Mixed-numeric arithmetic (`i32 + i64`) currently fails typecheck
        // — once widening is wired, this constraint can relax.
        if lhs_ty != rhs_ty {
            return None;
        }
        let target = target_type_name(lhs_ty, op, self.tc)?;

        // User impls of `Eq` conventionally provide only `eq`; mirror the
        // Rust/std convention and desugar `!=` to `!eq(a, b)` on Named
        // operand types. Primitive types always have `ne` registered in the
        // stdlib impl table, so they dispatch directly.
        if matches!(op, BinOp::NotEq) && matches!(lhs_ty, Type::Named { .. }) {
            let eq_call = Expr {
                span: left.span.clone(),
                kind: call_path(
                    vec![target, "eq".to_string()],
                    vec![left.clone(), right.clone()],
                ),
            };
            return Some(ExprKind::Unary {
                op: UnaryOp::Not,
                operand: Box::new(eq_call),
            });
        }

        Some(call_path(
            vec![target, method.to_string()],
            vec![left.clone(), right.clone()],
        ))
    }

    fn rewrite_unary(&self, op: &UnaryOp, operand: &Expr, _span: &Span) -> Option<ExprKind> {
        // `*expr` is a built-in dereference — no trait-method lowering needed.
        if matches!(op, UnaryOp::Deref) {
            return None;
        }
        let method = match op {
            UnaryOp::Neg => "neg",
            // Both `!` (logical not) and `~` (bitwise not) dispatch through
            // the `Not` trait's `not` method. The target type disambiguates:
            // `!bool` → `bool.not`, `~i32` → `i32.not`.
            UnaryOp::Not | UnaryOp::BitNot => "not",
            UnaryOp::Deref => unreachable!(),
        };
        let ty = self.type_at(&operand.span)?;
        // Gate each op to its valid operand types (mirrors typechecker rules).
        let ok = match op {
            UnaryOp::Neg => matches!(ty, Type::Int(_) | Type::Float(_)),
            UnaryOp::Not => matches!(ty, Type::Bool),
            UnaryOp::BitNot => matches!(ty, Type::Int(_) | Type::UInt(_)),
            UnaryOp::Deref => unreachable!(),
        };
        if !ok {
            return None;
        }
        let target = primitive_type_name(ty)?;
        Some(call_path(
            vec![target, method.to_string()],
            vec![operand.clone()],
        ))
    }

    fn type_at(&self, span: &Span) -> Option<&Type> {
        self.tc.expr_types.get(&SpanKey::from_span(span))
    }
}

/// Build a `Call(Path(segments), [args])` ExprKind with no labels.
fn call_path(segments: Vec<String>, args: Vec<Expr>) -> ExprKind {
    let span = args
        .first()
        .map(|a| a.span.clone())
        .unwrap_or_else(|| Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        });
    let callee = Box::new(Expr {
        span: span.clone(),
        kind: ExprKind::Path {
            segments,
            generic_args: None,
        },
    });
    let call_args = args
        .into_iter()
        .map(|e| CallArg {
            label: None,
            mut_marker: false,
            span: e.span.clone(),
            value: e,
        })
        .collect();
    ExprKind::Call {
        callee,
        args: call_args,
    }
}

/// Resolve the impl-target type name for a binary operator on operand type `ty`.
/// For primitive types, falls back to the stdlib target (`i32`, `bool`, etc.).
/// For user-defined `Type::Named`, returns the name only when a matching user
/// impl is registered — the operator-lowering pass supports user-type dispatch
/// only for the relational traits (`Eq`, `Ord`) at present; arithmetic/bitwise
/// stay primitive-only until the heterogeneous `Rhs`/`Output` design lands.
fn target_type_name(ty: &Type, op: &BinOp, tc: &TypeCheckResult) -> Option<String> {
    if let Some(name) = primitive_type_name(ty) {
        return Some(name);
    }
    if let Type::Named { name, .. } = ty {
        let trait_name = match op {
            BinOp::Eq | BinOp::NotEq => "Eq",
            BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => "Ord",
            _ => return None,
        };
        if tc
            .trait_impls
            .contains(&(trait_name.to_string(), name.clone()))
        {
            return Some(name.clone());
        }
    }
    None
}

/// Returns the canonical name of a primitive type for impl-target lookup,
/// or None for non-primitive types we don't yet lower operators on.
fn primitive_type_name(ty: &Type) -> Option<String> {
    let name = match ty {
        Type::Int(IntSize::I8) => "i8",
        Type::Int(IntSize::I16) => "i16",
        Type::Int(IntSize::I32) => "i32",
        Type::Int(IntSize::I64) => "i64",
        Type::UInt(UIntSize::U8) => "u8",
        Type::UInt(UIntSize::U16) => "u16",
        Type::UInt(UIntSize::U32) => "u32",
        Type::UInt(UIntSize::U64) => "u64",
        Type::UInt(UIntSize::Usize) => "usize",
        Type::Float(FloatSize::F32) => "f32",
        Type::Float(FloatSize::F64) => "f64",
        Type::Bool => "bool",
        Type::Char => "char",
        Type::Str => "String",
        _ => return None,
    };
    Some(name.to_string())
}
