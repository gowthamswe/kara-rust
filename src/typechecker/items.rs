//! Pass-2 item checking: walk the program with the populated TypeEnv
//! and check function bodies, trait declarations, impl blocks, const
//! declarations, plus the visibility audit on public signatures.
//!
//! Houses `check_items` (the driver), `check_trait_def`, the
//! visibility-audit triad (`collect_type_visibility`,
//! `check_type_expr_visibility`, `check_signature_visibility`),
//! `check_function`, `check_impl_block`, `check_const_decl`, and the
//! statement/block-level inference primitives (`check_block_against`,
//! `infer_block`, `check_stmt`, `check_unsolved_type_param`).

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::{IntSuffix, Span};
use std::collections::{HashMap, HashSet};

use super::const_eval::{
    apply_binary, apply_unary, const_value_type, infer_operand_target_ty, integer_to_const_value,
};
use super::inference::{find_unbound_const_param, find_unbound_type_param};
use super::types::{type_display, IntSize, Type, UIntSize, VariantTypeInfo};
use super::{ConstEvalError, LocalTypeScope, TypeErrorKind};

impl<'a> super::TypeChecker<'a> {
    pub(super) fn check_items(&mut self) {
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
                        deprecation: None,
                        is_track_caller: false,
                        lint_overrides: Vec::new(),
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
    pub(super) fn check_signature_visibility(&mut self) {
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
    pub(super) fn expr_references_name(expr: &Expr, name: &str) -> bool {
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

    /// Per-position "is this param a borrow slot?" lookup for the
    /// `Call` arm of the once-callability walker. Returns
    /// `Some(Vec<bool>)` where each `true` means "borrow position
    /// (`ref T` / `mut ref T` / `mut Slice[T]`), so the arg is read,
    /// not consumed". `None` when the callee's signature is unknown
    /// (function-pointer call, type-param method, builtin without an
    /// `env.functions` entry) — the caller falls back to per-arg
    /// defaults (Consuming). Mirrors `ownership::param_modes_from_signature`
    /// without depending on it, by reading directly from `self.env`.
    pub(super) fn callee_borrow_positions(&self, callee: &Expr) -> Option<Vec<bool>> {
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
    pub(super) fn check_block_against(&mut self, block: &Block, expected: &Type) -> Type {
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

        // GAT slice 7: cache the trait's AssocTypeDecl bounds keyed by
        // assoc-type name so the binding loop can enforce them at impl
        // site without re-walking program.items per binding. Empty when
        // the impl is inherent (no trait) or the trait isn't found in
        // the current program's items (e.g., baked stdlib traits, where
        // the bound enforcement is a no-op — slice 7 v1 scope is the
        // user-program surface where program.items carries the decl).
        let trait_assoc_bounds: HashMap<String, Vec<TraitBound>> = imp
            .trait_name
            .as_ref()
            .and_then(|tp| tp.segments.last())
            .and_then(|tn| {
                self.program.items.iter().find_map(|it| match it {
                    Item::TraitDef(t) if t.name == *tn => Some(t),
                    _ => None,
                })
            })
            .map(|trait_def| {
                trait_def
                    .items
                    .iter()
                    .filter_map(|it| match it {
                        TraitItem::AssocType(decl) => {
                            Some((decl.name.clone(), decl.bounds.clone()))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        for item in &imp.items {
            match item {
                ImplItem::Method(method) => self.check_function(method, Some(&self_type), &[]),
                ImplItem::AssocType(binding) => {
                    // GAT slice 5: extend the generic scope with the GAT's
                    // own params so the binding RHS like `Wrapper[U]` lowers
                    // `U` as `Type::TypeParam("U")` instead of falling
                    // through as `Named { name: "U", args: [] }`. The
                    // template now references both impl-side params
                    // (from `gp`) and GAT-side params (from
                    // `binding.generic_params`) uniformly; the resolver
                    // distinguishes the two via the `gat_params` list
                    // stored alongside the template.
                    let gat_params = Self::generic_param_names(&binding.generic_params);
                    let mut combined_scope = gp.clone();
                    combined_scope.extend(gat_params.iter().cloned());
                    let bound_ty = self.lower_type_expr(&binding.ty, &combined_scope);

                    // GAT slice 7: impl-site bound enforcement.
                    // The trait's GAT declaration may carry bounds
                    // (`type Mapped[U]: Trait`). At every impl site,
                    // the binding's lowered RHS must satisfy each
                    // declared bound. Per design.md the proof is
                    // structural: the RHS is provable to satisfy
                    // `Trait` for arbitrary GAT-param instantiation
                    // when the RHS's head type carries a generic-on-
                    // name impl of the bound trait (e.g.,
                    // `Vec[U]: Clone` via `impl[T] Clone for Vec[T]`).
                    // The TypeParam-RHS shape (`type Mapped = T`)
                    // proves via the impl's own `enclosing_bounds`
                    // on T.
                    if let Some(bounds) = trait_assoc_bounds.get(&binding.name) {
                        for bound in bounds {
                            let bound_trait = bound.path.last().cloned().unwrap_or_default();
                            if !self.gat_rhs_satisfies_bound(&bound_ty, &bound_trait) {
                                self.type_error(
                                    format!(
                                        "error[E_GAT_BOUND_NOT_SATISFIED]: \
                                         binding `type {} = {}` does not satisfy \
                                         declared GAT bound `{}: {}`",
                                        binding.name,
                                        type_display(&bound_ty),
                                        binding.name,
                                        bound_trait,
                                    ),
                                    binding.span.clone(),
                                    TypeErrorKind::TypeMismatch,
                                );
                            }
                        }
                    }

                    self.env.impl_assoc_types.insert(
                        (type_name.clone(), binding.name.clone()),
                        crate::typechecker::env::ImplAssocTypeEntry {
                            ty: bound_ty,
                            gat_params,
                        },
                    );
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

    pub(super) fn infer_block(&mut self, block: &Block) -> Type {
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

    pub(super) fn check_stmt(&mut self, stmt: &Stmt) {
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
}
