//! Type-expression lowering: AST `TypeExpr` → internal `Type`.
//!
//! Houses the recursive `lower_type_expr*` walker, the path-type
//! resolver (`lower_path_type`), array / slice lowering with
//! const-arg threading, primitive-type name lookup, generic-param
//! name collection, parameter-bound gathering, associated-type
//! projection resolution, alias-chain resolution, and element-type
//! extraction. Lives in a sibling `impl<'a> super::TypeChecker<'a>`
//! block.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

use super::const_eval::{const_value_to_array_size, const_value_type};
use super::inference::substitute_type_params;
use super::types::{type_display, ConstArg, FloatSize, IntSize, SubstValue, Type, UIntSize};
use super::TypeErrorKind;

impl<'a> super::TypeChecker<'a> {
    // ── lower_type_expr ─────────────────────────────────────────

    pub(super) fn lower_type_expr(&mut self, ty: &TypeExpr, generic_scope: &[String]) -> Type {
        // Top-level entry: by default the type is in a sized-by-value
        // position. Slice 1b's `E_OPAQUE_TYPE_REQUIRES_INDIRECTION` check
        // flips `parent_is_ref` to `true` only when descending through
        // `TypeKind::Ref` / `TypeKind::MutRef`, so opaque-foreign-type names
        // are accepted at `ref Foo` / `mut ref Foo` and rejected everywhere
        // else (fn params/return, struct fields, enum payloads, let
        // bindings, generic args, tuples, arrays, etc.).
        self.lower_type_expr_inner(ty, generic_scope, false)
    }

    pub(super) fn lower_type_expr_inner(
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
            // `impl Trait` slice 3: return-position / RPITIT / TAIT-RHS
            // lower to `Type::Existential` carrying the named trait bound
            // and a `SpanKey` origin that identifies the declaration site
            // (so two `impl Iterator` declarations on different fns stay
            // distinct even with structurally identical bounds).
            // Argument-position was already eliminated by the slice-2
            // resolver desugar, so any `TypeKind::ImplTrait` reaching this
            // site is in a return / TAIT / non-arg position. Caller-side
            // opacity, body-return checking, and trait-surface method
            // dispatch are handled at `check_assignable` /
            // `is_subtype_with_projections` / `type_satisfies_bound` and
            // the receiver-dispatch path. See phase-5-diagnostics.md line
            // 397 for the slice 3 entry.
            TypeKind::ImplTrait {
                trait_path, args, ..
            } => {
                let trait_args: Vec<Type> = args
                    .iter()
                    .filter_map(|a| match a {
                        GenericArg::Type(t) => {
                            Some(self.lower_type_expr_inner(t, generic_scope, false))
                        }
                        // Const-args on trait paths aren't part of slice 3
                        // — the parser accepts them but `Type::Existential`
                        // has no representation for const arguments today.
                        GenericArg::Const(_) => None,
                    })
                    .collect();
                Type::Existential {
                    trait_name: trait_path.segments.join("."),
                    trait_args,
                    origin: crate::resolver::SpanKey::from_span(&ty.span),
                    // Slice 6: the TAIT marker is set later by
                    // `env_add_type_alias` when this lowering happens
                    // inside a `type X = impl Trait;` declaration.
                    // Return-position / RPITIT existentials from slice
                    // 3 stay `None` — their alias-less origin is the
                    // distinguishing signal for slice 6's TAIT-aware
                    // diagnostics.
                    tait_alias: None,
                }
            }
            // `dyn Trait` slice 5 — the dual of `impl Trait`. Slice 5
            // ships the RPITIT-incompatibility check: when the named
            // trait has any method declared with `-> impl Trait`
            // (return-position impl trait in trait), `dyn Trait`
            // cannot synthesize a fixed vtable slot for that method.
            // Lower to `Type::Error` with `E_RPITIT_INCOMPATIBLE_WITH_DYN`
            // naming the first offending method. When the trait has no
            // RPITIT methods, emit the generic `E_DYN_TRAIT_NOT_IMPLEMENTED_YET`
            // P1-deferred stub (general `dyn Trait` value/type
            // semantics, vtable layout, and effect checking are P1 per
            // design.md § Polymorphism). Walking generic args before
            // emitting surfaces any nested type errors.
            TypeKind::Dyn {
                trait_path, args, ..
            } => {
                for a in args {
                    if let GenericArg::Type(t) = a {
                        let _ = self.lower_type_expr_inner(t, generic_scope, false);
                    }
                }
                let trait_name = trait_path.segments.join(".");
                if let Some(offending) = Self::trait_first_rpitit_method(self.program, &trait_name)
                {
                    self.type_error(
                        format!(
                            "error[E_RPITIT_INCOMPATIBLE_WITH_DYN]: cannot use \
                             `dyn {trait_name}` because method `{offending}` returns \
                             `impl Trait` — return-position `impl Trait` in trait \
                             methods (RPITIT) has no fixed vtable slot, so `dyn`-dispatched \
                             callers cannot synthesize a thunk; route through a generic \
                             parameter `[T: {trait_name}]` instead"
                        ),
                        ty.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                } else {
                    self.type_error(
                        format!(
                            "error[E_DYN_TRAIT_NOT_IMPLEMENTED_YET]: `dyn {trait_name}` is \
                             parsed but the trait-object machinery (vtable construction, \
                             dynamic dispatch, effect-opacity story) is P1-deferred per \
                             design.md § Polymorphism; route through a generic parameter \
                             `[T: {trait_name}]` for now"
                        ),
                        ty.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Type::Error
            }
            TypeKind::Unit => Type::Unit,
            TypeKind::Error => Type::Error,
        }
    }

    /// `impl Trait` slice 5 — return `Some(method_name)` for the first
    /// method of `trait_name` whose return type is `TypeKind::ImplTrait`,
    /// i.e. the trait declares an RPITIT method. Walks the AST trait
    /// declaration directly (the typed env doesn't carry per-method
    /// return-shape information in a queryable form). Returns `None`
    /// when the trait is not defined in the current program (e.g.,
    /// baked stdlib trait), when no methods return `impl Trait`, or
    /// when the name resolves to a non-trait item — slice 5's check
    /// fires only on positively-identified RPITIT traits; the generic
    /// `E_DYN_TRAIT_NOT_IMPLEMENTED_YET` stub covers the rest.
    fn trait_first_rpitit_method(
        program: &crate::ast::Program,
        trait_name: &str,
    ) -> Option<String> {
        for item in &program.items {
            if let Item::TraitDef(t) = item {
                if t.name != trait_name {
                    continue;
                }
                for trait_item in &t.items {
                    if let TraitItem::Method(method) = trait_item {
                        if let Some(ref ret) = method.return_type {
                            if matches!(ret.kind, TypeKind::ImplTrait { .. }) {
                                return Some(method.name.clone());
                            }
                        }
                    }
                }
                return None;
            }
        }
        None
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

    pub(super) fn lower_path_type(&mut self, path: &PathExpr, generic_scope: &[String]) -> Type {
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
            // `#[deprecated]` slice 4 — emit deprecation warning when
            // a type-position reference resolves to a deprecated
            // struct / enum / trait. Lowering happens AFTER the
            // enclosing item's `lint_override_stack` frame is pushed
            // (the typechecker walks signatures inside `check_function`
            // / `check_impl_block` after pushing), so the cascade
            // honours `#[allow(deprecated)]` on the enclosing scope.
            if self.env.structs.contains_key(name)
                || self.env.enums.contains_key(name)
                || self.env.traits.contains_key(name)
            {
                self.check_deprecated_use_at(&path.span, name);
            }
            Type::Named {
                name: name.clone(),
                args,
            }
        } else {
            // Two-segment path where the first segment is a type parameter:
            // `I.Item` — an associated type projection. Exactly two segments
            // required; deeper paths (`A.B.C`) are module paths, not projections.
            // Generic-associated-type form `F.Mapped[i64]` (GAT slice 4): the
            // path's `generic_args` attach to the GAT name (the parser writes
            // them on the path as a whole; for a 2-segment path that's the
            // assoc-type segment by convention). Lower them as the
            // projection's `args` so the type system retains the
            // instantiation through substitution and resolution.
            if path.segments.len() == 2 && generic_scope.contains(&path.segments[0]) {
                let assoc = path.segments[1].clone();
                let args =
                    self.lower_generic_args_named(&path.generic_args, generic_scope, Some(&assoc));
                // GAT slice 5: `receiver_args` is empty at lowering time —
                // the source-level `F.Mapped[i64]` shape has the receiver
                // as a bare type param. `substitute_type_params` populates
                // `receiver_args` once `F` is solved to a concrete generic
                // receiver like `Wrapper[String]`.
                return Type::AssocProjection {
                    param: path.segments[0].clone(),
                    assoc,
                    args,
                    receiver_args: vec![],
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

    pub(super) fn generic_param_names(generics: &Option<GenericParams>) -> Vec<String> {
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
    pub(super) fn collect_param_bounds(
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

    /// Walk `ty` and resolve any `AssocProjection { param, assoc, args,
    /// receiver_args }` nodes whose `param` (after substitution it holds
    /// the concrete receiver's bare type name) has an entry in
    /// `impl_assoc_types`. This is called after `substitute_type_params`
    /// so that `T.Item` first gets its `T` replaced by the concrete
    /// receiver (base name in `param`, args in `receiver_args`), then
    /// gets resolved to the actual associated type.
    ///
    /// GAT slice 5 — two-sided substitution: when the lookup hits, the
    /// resolver builds a substitution map from BOTH
    /// (a) the struct's `generic_params` zipped with the projection's
    ///     `receiver_args` — substituting impl-side TypeParams in the
    ///     template (e.g., the `T` in `Pair[T, U]` for an impl
    ///     `impl[T] Functor for Wrapper[T] { type Mapped[U] = Pair[T, U] }`),
    /// AND
    /// (b) the entry's `gat_params` zipped with the projection's own `args` —
    ///     substituting GAT-side TypeParams (the `U` in the same example).
    /// The substitution runs once on the template, yielding the concrete
    /// resolved type. If no entry is found, the projection is reconstructed
    /// with the args/receiver_args recursively resolved so the consumer
    /// sees a well-formed projection node.
    pub(super) fn resolve_assoc_projections(&self, ty: &Type) -> Type {
        match ty {
            Type::AssocProjection {
                param,
                assoc,
                args,
                receiver_args,
            } => {
                let resolved_args: Vec<Type> = args
                    .iter()
                    .map(|a| self.resolve_assoc_projections(a))
                    .collect();
                let resolved_recv_args: Vec<Type> = receiver_args
                    .iter()
                    .map(|a| self.resolve_assoc_projections(a))
                    .collect();
                if let Some(entry) = self
                    .env
                    .impl_assoc_types
                    .get(&(param.clone(), assoc.clone()))
                {
                    // Build the two-sided substitution map. Impl-side
                    // params come from the struct's `generic_params`
                    // paired with the projection's `receiver_args` —
                    // mirroring the `element_type_of` substitution
                    // pattern. GAT-side params come from the entry's
                    // own `gat_params` list paired with the
                    // projection's `args`. If the lengths don't match
                    // (arity mismatch — should be caught upstream as
                    // an error), zip truncates silently and the rest
                    // of the template is left with unsolved TypeParams,
                    // which downstream typechecking will surface.
                    let mut subs: HashMap<String, SubstValue> = HashMap::new();
                    if let Some(info) = self.env.structs.get(param) {
                        for (p, a) in info.generic_params.iter().zip(resolved_recv_args.iter()) {
                            subs.insert(p.clone(), SubstValue::Type(a.clone()));
                        }
                    }
                    for (p, a) in entry.gat_params.iter().zip(resolved_args.iter()) {
                        subs.insert(p.clone(), SubstValue::Type(a.clone()));
                    }
                    let substituted = substitute_type_params(&entry.ty, &subs);
                    // Recursively resolve in case the substituted type
                    // contains further projections (e.g., when the
                    // template's RHS itself references an assoc type).
                    self.resolve_assoc_projections(&substituted)
                } else {
                    Type::AssocProjection {
                        param: param.clone(),
                        assoc: assoc.clone(),
                        args: resolved_args,
                        receiver_args: resolved_recv_args,
                    }
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
    pub(super) fn element_type_of(&self, ty: &Type) -> Type {
        match ty {
            // Primitive borrowed views — element type is the inner type.
            Type::Array { element, .. } | Type::Slice { element, .. } => *element.clone(),
            Type::Named { name, args } => {
                // Look up the "Item" associated type for this collection.
                let Some(entry) = self
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
                substitute_type_params(&entry.ty, &subs)
            }
            _ => ty.clone(),
        }
    }
}
