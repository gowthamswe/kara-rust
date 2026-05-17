//! Generic-bound and where-clause validation.
//!
//! Houses trait-name recognition (`is_known_trait`, `is_trait_alias`,
//! `report_trait_alias_use`), inline-bound validation, where-clause
//! validation, const-param type permittedness checks, and
//! default-parameter-value validation. Lives in a sibling
//! `impl<'a> super::TypeChecker<'a>` block.

use crate::ast::*;
use crate::token::Span;

use super::types::{type_display, IntSize, Type, VariantTypeInfo};
use super::{ConstEvalError, TypeErrorKind};

impl<'a> super::TypeChecker<'a> {
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
    pub(super) fn is_trait_alias(&self, trait_name: &str) -> bool {
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
    pub(super) fn trait_alias_bound_list(&self, trait_name: &str) -> Option<String> {
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
    pub(super) fn validate_all_bounds(
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

    /// GAT slice 7 — structural proof that an impl's GAT binding RHS
    /// satisfies a bound declared on the GAT (e.g., `type Mapped[U]: Clone`).
    /// The check runs once at impl-site registration and must hold for
    /// arbitrary instantiations of the GAT's own parameters.
    ///
    /// Three proof paths:
    ///
    /// 1. **`TypeParam(name)` RHS** — the binding is bare (`type Mapped = T`).
    ///    Proof discharges via the impl's `enclosing_bounds[T]` carrying the
    ///    bound trait (or one whose supertrait closure reaches it).
    ///
    /// 2. **Concrete-head RHS** (`Vec[U]`, `i64`, `Doubler`, etc.) — proof
    ///    routes through `type_satisfies_bound`, which consults built-in
    ///    `type_supports_*` for derive-recognised traits and the impl table
    ///    (with `impl_args_match`'s "stored-empty matches any" rule) for
    ///    everything else. A generic-on-name impl like
    ///    `impl[T] Clone for Vec[T]` discharges `Vec[U]: Clone` for any U.
    ///
    /// 3. **Anything else** (function types, raw pointers, type variables)
    ///    cannot satisfy a nominal trait bound today and conservatively
    ///    returns `false` — the diagnostic at the call site surfaces the
    ///    mismatch.
    ///
    /// Conservative-by-design: types we can't prove satisfy the bound get
    /// rejected at impl-site. The user can address by providing an explicit
    /// impl, an explicit bound on the impl param, or a different RHS choice.
    pub(super) fn gat_rhs_satisfies_bound(&self, rhs: &Type, bound_trait: &str) -> bool {
        if let Type::TypeParam(name) = rhs {
            if let Some(bounds) = self.enclosing_bounds.get(name) {
                return bounds.iter().any(|tb| {
                    let trait_name = tb.path.last().cloned().unwrap_or_default();
                    if trait_name == bound_trait {
                        return true;
                    }
                    self.env
                        .supertrait_closure_traits(&trait_name)
                        .iter()
                        .any(|st| st == bound_trait)
                });
            }
            return false;
        }
        self.type_satisfies_bound(rhs, bound_trait)
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
    pub(super) fn validate_default_params(&mut self, params: &[Param], generic_scope: &[String]) {
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
}
