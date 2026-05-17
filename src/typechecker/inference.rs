//! Type-inference primitives: structural substitution, call-site
//! metavariable plumbing, unification, type-variable resolution, and
//! unbound-parameter detection.
//!
//! These are pure tree-walking utilities — no `TypeChecker` state is
//! threaded. The `TypeChecker::unify` / `resolve_type` / `fresh_type_var`
//! methods in `super` are thin wrappers that consult its
//! `substitutions` / `const_substitutions` maps; the algorithms here
//! operate on borrowed `HashMap` substrate so they're callable from
//! anywhere without a `&mut TypeChecker`.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

use super::const_eval::substitute_const_arg;
use super::types::{
    type_display, types_compatible, ConstArg, ConstVarId, SubstValue, Type, TypeVarId,
};

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
pub(super) fn substitute_type_params(ty: &Type, subs: &HashMap<String, SubstValue>) -> Type {
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
        // caller can post-resolve via `resolve_assoc_projections`. The
        // projection's own `args` (the `i64` in `F.Mapped[i64]` — GAT
        // slice 4) are walked too so a `F.Mapped[T]` with `T` solved
        // becomes `F.Mapped[<concrete>]` before resolution. The
        // `receiver_args` field (slice 5) is also walked recursively
        // so an already-resolved generic receiver like
        // `Wrapper<T>.Mapped[U]` propagates further substitutions
        // into `T`.
        Type::AssocProjection {
            param,
            assoc,
            args,
            receiver_args,
        } => {
            let new_args: Vec<Type> = args
                .iter()
                .map(|a| substitute_type_params(a, subs))
                .collect();
            let walked_receiver_args: Vec<Type> = receiver_args
                .iter()
                .map(|a| substitute_type_params(a, subs))
                .collect();
            if let Some(concrete) = subs.get(param).and_then(SubstValue::as_type) {
                // GAT slice 5: when the receiver resolves to a generic
                // concrete type (`Type::Named`/`Shared`/`Rc`/`Arc`),
                // capture the base name in `param` and the receiver's
                // args in `receiver_args` so the lookup at
                // `resolve_assoc_projections` can key by bare name and
                // build an impl-side substitution. The pre-slice-5 path
                // used `type_display(concrete)` directly, which produced
                // `"Wrapper<String>"` and failed the bare-name lookup.
                let (param_name, captured_recv_args) = match concrete {
                    Type::Named { name, args } => (name.clone(), args.clone()),
                    Type::Shared(name) => (name.clone(), vec![]),
                    Type::Rc(inner) => ("Rc".to_string(), vec![(**inner).clone()]),
                    Type::Arc(inner) => ("Arc".to_string(), vec![(**inner).clone()]),
                    other => (type_display(other), vec![]),
                };
                Type::AssocProjection {
                    param: param_name,
                    assoc: assoc.clone(),
                    args: new_args,
                    receiver_args: captured_recv_args,
                }
            } else {
                Type::AssocProjection {
                    param: param.clone(),
                    assoc: assoc.clone(),
                    args: new_args,
                    receiver_args: walked_receiver_args,
                }
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
pub(super) fn instantiate_signature_with_fresh_vars(
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
            // GAT slice 4: the projection's own `args` (the `T` in
            // `F.Mapped[T]`) IS an introduction site — walk it so the
            // outer signature gets fresh vars for nested params.
            // GAT slice 5: `receiver_args` is empty at signature-source
            // lowering time (the parser produces a bare-receiver path),
            // but walking it preserves correctness if a synthesised
            // signature ever carries pre-resolved receiver args.
            Type::AssocProjection {
                args,
                receiver_args,
                ..
            } => {
                for a in args {
                    collect(a, names, seen, const_names, const_seen);
                }
                for a in receiver_args {
                    collect(a, names, seen, const_names, const_seen);
                }
            }
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
            // GAT slice 4: walk the projection's own type-args so a
            // `F.Mapped[T]` in the signature gets `T` swapped for its
            // fresh `TypeVar`. The outer `param` is a TypeParam name
            // pre-resolution and stays as a String here; the call-site
            // solver re-maps it once `F` itself is bound (the
            // `substitute_type_params` arm in this same file handles
            // that direction post-call-site solve). GAT slice 5:
            // `receiver_args` is empty for parsed signatures; walking
            // it here keeps the structure stable for any synthesised
            // projection that already carries pre-resolved receiver
            // args.
            Type::AssocProjection {
                param,
                assoc,
                args,
                receiver_args,
            } => Type::AssocProjection {
                param: param.clone(),
                assoc: assoc.clone(),
                args: args
                    .iter()
                    .map(|a| substitute(a, name_to_id, name_to_const_id))
                    .collect(),
                receiver_args: receiver_args
                    .iter()
                    .map(|a| substitute(a, name_to_id, name_to_const_id))
                    .collect(),
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
pub(super) struct InstantiatedSignature {
    pub(super) params: Vec<Type>,
    pub(super) return_type: Type,
    pub(super) name_to_id: HashMap<String, TypeVarId>,
    pub(super) id_to_name: HashMap<TypeVarId, String>,
    #[allow(dead_code)]
    pub(super) name_to_const_id: HashMap<String, ConstVarId>,
    pub(super) const_id_to_name: HashMap<ConstVarId, String>,
}

/// Const generics slice 3c: walk `expr` and substitute any
/// `ExprKind::Identifier(name)` whose `name` is in `subst` with an
/// `Integer(value)` literal. Used by the where-clause discharge
/// engine to inline resolved const-args into the predicate Expr
/// before evaluation. Composite shapes (Tuple / ArrayLiteral) recurse
/// element-wise; non-substitutable shapes (calls, closures, etc.)
/// pass through unchanged — slice 2's `eval_const_expr` rejects them
/// downstream as `NonConstShape`.
pub(super) fn substitute_const_idents_in_expr(expr: &Expr, subst: &HashMap<String, i64>) -> Expr {
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
pub(super) fn expr_as_type_expr(expr: &Expr) -> Option<TypeExpr> {
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
pub(super) fn is_literal_const_arg_expr(expr: &Expr) -> bool {
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
pub(super) fn const_value_from_literal(expr: &Expr) -> Option<i64> {
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
pub(super) fn resolve_type_vars(
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
pub(super) fn resolve_const_arg(
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
pub(super) fn resolve_type_var_top(ty: &Type, substitutions: &HashMap<TypeVarId, Type>) -> Type {
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
pub(super) fn unify_types(
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
pub(super) fn unify_const_args(
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
pub(super) fn find_unbound_const_param<'a>(
    ty: &'a Type,
    in_scope: &HashSet<&str>,
) -> Option<&'a str> {
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

pub(super) fn find_unbound_type_param<'a>(
    ty: &'a Type,
    in_scope: &HashSet<&str>,
) -> Option<&'a str> {
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
        Type::AssocProjection {
            param,
            args,
            receiver_args,
            ..
        } => {
            if !in_scope.contains(param.as_str()) {
                return Some(param.as_str());
            }
            // GAT slice 4: walk the projection's own type-args
            // (`F.Mapped[T]` → `T` may be unbound) so the call-site
            // generic-instantiation check sees nested params.
            // GAT slice 5: also walk receiver_args for the
            // post-substitution shape where the projection carries
            // the receiver's args (e.g., `Wrapper<T>.Mapped[i64]`
            // post-`substitute_type_params`).
            args.iter()
                .find_map(|a| find_unbound_type_param(a, in_scope))
                .or_else(|| {
                    receiver_args
                        .iter()
                        .find_map(|a| find_unbound_type_param(a, in_scope))
                })
        }
        _ => None,
    }
}
