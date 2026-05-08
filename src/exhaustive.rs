//! Maranget-style pattern exhaustiveness for `match` expressions.
//!
//! Tracked under `docs/implementation_checklist/phase-4-interpreter.md` §
//! "TypeChecker: upgrade exhaustiveness to Maranget's algorithm".
//!
//! Slice 1 (landed): pattern matrix data structure + `is_useful` recursion
//! for finite-domain scrutinees only (`bool` and named enums) with opaque
//! payloads.
//!
//! Slice 2 (landed): field-level recursion. Variant payloads, tuple
//! elements, and struct fields are lowered into `Pat::Ctor.args` so
//! `Some(0)` and `Some(1)` are distinct rows. Per-column types are threaded
//! through `is_useful` to drive constructor enumeration at every depth.
//!
//! Slice 3 (landed): top-level scrutinee gate widened from "bool + named
//! enum only" to "everything except function/typeparam/ref/pointer/error/
//! unit". Open-domain types (`i64`, `f64`, `Char`, `Str`, `String`, `Vec`,
//! `Map`, `Slice`, `Array`, etc.) flow through `enumerate_ctors → None`,
//! which lands on the default-matrix path and demands a wildcard arm.
//! `Never` enumerates to an empty constructor list (vacuously exhaustive).
//!
//! Slice 4 (landed): witness construction. The core recursion now
//! returns `Option<Vec<Pat>>` (`None` = covered; `Some(w)` = uncovered
//! witness vector matching the query column count). The witness is
//! repackaged on each recursion level so the caller sees a single root
//! pattern, then rendered as a string for the diagnostic.
//!
//! Slice 5 (landed): reachability pass. `unreachable_arms` walks arms
//! left-to-right, building a covering matrix from prior *unguarded* arms,
//! and reports any arm whose pattern adds no new coverage. Guarded arms
//! don't contribute to the covering matrix.
//!
//! Slice 6 (this file): irrefutability via `U`. `is_pattern_irrefutable`
//! exposes the same recursion to callers that need to know whether a
//! single pattern matches every value of a given type (`let PAT = expr`
//! and function/closure parameters require this; `if let` / `while let`
//! require the inverse). Returns `None` for skipped types so the caller
//! can fall back to the older syntactic check on ref/function/typeparam
//! scrutinees that Maranget doesn't reason about. Range and float literal
//! patterns still lower to `Pat::Wildcard` (slice 6 gap analysis; floats
//! wait on Eq/Hash modeling).

use crate::ast::{LiteralPattern, MatchArm, Pattern, PatternKind};
use crate::typechecker::{Type, TypeEnv, VariantTypeInfo};

#[derive(Debug, Clone)]
enum Pat {
    Wildcard,
    Ctor { ctor: PatCtor, args: Vec<Pat> },
    Or(Vec<Pat>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PatCtor {
    Bool(bool),
    Variant(String),
    Lit(PatLit),
    Tuple,
    Struct(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PatLit {
    Integer(i64),
    Char(char),
    String(String),
}

#[derive(Debug, Clone)]
struct Row {
    pats: Vec<Pat>,
}

#[derive(Debug, Clone, Default)]
struct Matrix {
    rows: Vec<Row>,
}

pub enum ExhaustiveResult {
    Exhaustive,
    NonExhaustive {
        witness: String,
    },
    /// Scrutinee type is not yet handled by Maranget; the caller must skip
    /// the check (matches the pre-Maranget early-return behaviour).
    Skipped,
}

/// Returns `Some(true)` if `pat` matches every value of `ty` (irrefutable),
/// `Some(false)` if some value is not matched (refutable), or `None` if
/// the type is outside Maranget's handled set (caller should fall back to
/// the legacy syntactic check). Implements `U([PAT], _) == false` —
/// exactly the rule called out in design.md § Pattern Exhaustiveness for
/// slice 6 of the upgrade.
pub fn is_pattern_irrefutable(pat: &Pattern, ty: &Type, env: &TypeEnv) -> Option<bool> {
    if !is_handled_scrutinee(ty) {
        return None;
    }
    let lowered = lower_pattern(pat, ty, env);
    let matrix = Matrix {
        rows: vec![Row {
            pats: vec![lowered],
        }],
    };
    let head_types = vec![ty.clone()];
    Some(usefulness(&matrix, &[Pat::Wildcard], &head_types, env).is_none())
}

/// Walk the arms in source order and return the indices of any arm whose
/// pattern is fully covered by some earlier *unguarded* arm. Guarded arms
/// are themselves checked for reachability (a guarded arm with a duplicate
/// pattern of an earlier unguarded arm is still unreachable) but never
/// contribute to coverage themselves — the guard might fail at runtime,
/// so a following arm with the same pattern can still be reached.
pub fn unreachable_arms(scrutinee_type: &Type, arms: &[MatchArm], env: &TypeEnv) -> Vec<usize> {
    if !is_handled_scrutinee(scrutinee_type) {
        return Vec::new();
    }
    let head_types = vec![scrutinee_type.clone()];
    let mut unreachable = Vec::new();
    let mut covering = Matrix::default();
    for (i, arm) in arms.iter().enumerate() {
        let pat = lower_pattern(&arm.pattern, scrutinee_type, env);
        let q = vec![pat.clone()];
        if usefulness(&covering, &q, &head_types, env).is_none() {
            unreachable.push(i);
        }
        if arm.guard.is_none() {
            covering.rows.push(Row { pats: vec![pat] });
        }
    }
    unreachable
}

pub fn check_match_exhaustive(
    scrutinee_type: &Type,
    arms: &[MatchArm],
    env: &TypeEnv,
) -> ExhaustiveResult {
    if !is_handled_scrutinee(scrutinee_type) {
        return ExhaustiveResult::Skipped;
    }

    let matrix = build_matrix(arms, scrutinee_type, env);
    let head_types = vec![scrutinee_type.clone()];

    match usefulness(&matrix, &[Pat::Wildcard], &head_types, env) {
        None => ExhaustiveResult::Exhaustive,
        Some(witness_vec) => {
            let pat = witness_vec.into_iter().next().unwrap_or(Pat::Wildcard);
            ExhaustiveResult::NonExhaustive {
                witness: render_witness(&pat, scrutinee_type, env),
            }
        }
    }
}

/// Gate for top-level exhaustiveness. Denylist: skip computed/erroneous/
/// internal types and ref/pointer wrappers (the latter to preserve the
/// pre-Maranget skip behaviour on `match r { ... }` over `ref T`). Every
/// other type — bool, integers, floats, char, str, tuple, array, slice,
/// named, never — flows through. For open-domain types the wildcard
/// recursion in `is_useful` falls to the default-matrix path, demanding a
/// wildcard arm.
fn is_handled_scrutinee(ty: &Type) -> bool {
    !matches!(
        ty,
        Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::TypeParam(_)
            | Type::TypeVar(_)
            | Type::AssocProjection { .. }
            | Type::Error
            | Type::Unit
            | Type::Ref(_)
            | Type::MutRef(_)
            | Type::Weak(_)
            | Type::Pointer { .. }
    )
}

fn build_matrix(arms: &[MatchArm], scrut_type: &Type, env: &TypeEnv) -> Matrix {
    let mut rows = Vec::new();
    for arm in arms {
        if arm.guard.is_some() {
            continue;
        }
        let pat = lower_pattern(&arm.pattern, scrut_type, env);
        rows.push(Row { pats: vec![pat] });
    }
    Matrix { rows }
}

fn lower_pattern(p: &Pattern, scrut_type: &Type, env: &TypeEnv) -> Pat {
    match &p.kind {
        PatternKind::Wildcard => Pat::Wildcard,
        PatternKind::Binding(name) => {
            if let Type::Named {
                name: type_name, ..
            } = scrut_type
            {
                if let Some(info) = env.enums.get(type_name) {
                    if info.variants.iter().any(|(v, _)| v == name) {
                        return Pat::Ctor {
                            ctor: PatCtor::Variant(name.clone()),
                            args: vec![],
                        };
                    }
                }
            }
            Pat::Wildcard
        }
        PatternKind::Literal(lit) => match lit {
            LiteralPattern::Bool(b) => Pat::Ctor {
                ctor: PatCtor::Bool(*b),
                args: vec![],
            },
            LiteralPattern::Integer(n, _) => Pat::Ctor {
                ctor: PatCtor::Lit(PatLit::Integer(*n)),
                args: vec![],
            },
            LiteralPattern::Char(c) => Pat::Ctor {
                ctor: PatCtor::Lit(PatLit::Char(*c)),
                args: vec![],
            },
            LiteralPattern::String(s) => Pat::Ctor {
                ctor: PatCtor::Lit(PatLit::String(s.clone())),
                args: vec![],
            },
            // Float patterns lack an Eq/Hash story under f64 — slice 3's
            // type-specific handling models them explicitly. Until then,
            // collapse to wildcard. Float scrutinees are wildcard-required
            // anyway, so this only affects nested float literals (which the
            // is_top_level gate already keeps rare).
            LiteralPattern::Float(_, _) => Pat::Wildcard,
        },
        // Range patterns lower to wildcard for now. Slice 6 will model
        // integer/char ranges as constructor ranges for proper gap analysis.
        PatternKind::RangePattern { .. } => Pat::Wildcard,
        PatternKind::TupleVariant { path, patterns } => {
            let name = path.last().cloned().unwrap_or_default();
            let payload_types = variant_payload_types(scrut_type, &name, env);
            let args = patterns
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let ty = payload_types.get(i).cloned().unwrap_or(Type::Unit);
                    lower_pattern(p, &ty, env)
                })
                .collect();
            Pat::Ctor {
                ctor: PatCtor::Variant(name),
                args,
            }
        }
        PatternKind::Struct { path, fields } => {
            let name = path.last().cloned().unwrap_or_default();
            // Two cases: enum struct-variant under an enum scrutinee, or a
            // plain struct.
            if let Type::Named {
                name: type_name, ..
            } = scrut_type
            {
                if let Some(info) = env.enums.get(type_name) {
                    if let Some((_, vinfo)) = info.variants.iter().find(|(v, _)| v == &name) {
                        let field_decls: Vec<(String, Type)> = match vinfo {
                            VariantTypeInfo::Struct(decls) => decls.clone(),
                            _ => vec![],
                        };
                        let args = lower_struct_fields(&field_decls, fields, env);
                        return Pat::Ctor {
                            ctor: PatCtor::Variant(name),
                            args,
                        };
                    }
                }
            }
            let field_decls: Vec<(String, Type)> = env
                .structs
                .get(&name)
                .map(|info| {
                    info.fields
                        .iter()
                        .map(|(n, t, _)| (n.clone(), t.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let args = lower_struct_fields(&field_decls, fields, env);
            Pat::Ctor {
                ctor: PatCtor::Struct(name),
                args,
            }
        }
        PatternKind::Tuple(patterns) => {
            let elem_types: Vec<Type> = match scrut_type {
                Type::Tuple(elems) => elems.clone(),
                _ => vec![],
            };
            let args = patterns
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let ty = elem_types.get(i).cloned().unwrap_or(Type::Unit);
                    lower_pattern(p, &ty, env)
                })
                .collect();
            Pat::Ctor {
                ctor: PatCtor::Tuple,
                args,
            }
        }
        PatternKind::AtBinding { pattern, .. } => lower_pattern(pattern, scrut_type, env),
        PatternKind::Or(alts) => Pat::Or(
            alts.iter()
                .map(|a| lower_pattern(a, scrut_type, env))
                .collect(),
        ),
    }
}

fn lower_struct_fields(
    field_decls: &[(String, Type)],
    pattern_fields: &[crate::ast::FieldPattern],
    env: &TypeEnv,
) -> Vec<Pat> {
    field_decls
        .iter()
        .map(
            |(fname, fty)| match pattern_fields.iter().find(|f| &f.name == fname) {
                Some(fp) => match &fp.pattern {
                    Some(p) => lower_pattern(p, fty, env),
                    None => Pat::Wildcard,
                },
                None => Pat::Wildcard,
            },
        )
        .collect()
}

fn variant_payload_types(parent_ty: &Type, variant_name: &str, env: &TypeEnv) -> Vec<Type> {
    if let Type::Named {
        name: enum_name, ..
    } = parent_ty
    {
        if let Some(info) = env.enums.get(enum_name) {
            if let Some((_, vinfo)) = info.variants.iter().find(|(v, _)| v == variant_name) {
                return match vinfo {
                    VariantTypeInfo::Unit => vec![],
                    VariantTypeInfo::Tuple(types) => types.clone(),
                    VariantTypeInfo::Struct(fields) => {
                        fields.iter().map(|(_, t)| t.clone()).collect()
                    }
                };
            }
        }
    }
    vec![]
}

/// Maranget's `U(P, q)`, witness-producing variant. Returns `None` if `q`
/// is fully covered by `matrix`; returns `Some(witness)` where `witness`
/// is a vector of patterns (one per column of `q`) describing a value
/// matching `q` but missed by every row of `matrix`.
///
/// `head_types[i]` is the type of column `i` so that wildcard recursion at
/// column 0 enumerates the right constructor set.
fn usefulness(matrix: &Matrix, q: &[Pat], head_types: &[Type], env: &TypeEnv) -> Option<Vec<Pat>> {
    if q.is_empty() {
        return matrix.rows.is_empty().then(Vec::new);
    }

    let q_head = &q[0];
    let q_rest = &q[1..];
    let head_ty = &head_types[0];
    let rest_tys = &head_types[1..];

    match q_head {
        Pat::Or(alts) => {
            for alt in alts {
                let mut new_q = Vec::with_capacity(q.len());
                new_q.push(alt.clone());
                new_q.extend_from_slice(q_rest);
                if let Some(w) = usefulness(matrix, &new_q, head_types, env) {
                    return Some(w);
                }
            }
            None
        }
        Pat::Ctor { ctor, args } => {
            let arity = args.len();
            let specialized = specialize(matrix, ctor, arity);
            let mut new_q = args.clone();
            new_q.extend_from_slice(q_rest);
            // When the pattern's arg count and the ctor's field-type list
            // disagree (e.g. malformed match where a variant pattern is used
            // on a scrutinee of an unrelated type — the typechecker emits
            // its own TypeMismatch in that case), pad/truncate to `arity`
            // so the recursion has matching column counts.
            let mut field_tys = ctor_field_types(ctor, head_ty, env);
            field_tys.resize(arity, Type::Unit);
            let mut new_head_types = field_tys;
            new_head_types.extend_from_slice(rest_tys);
            usefulness(&specialized, &new_q, &new_head_types, env)
                .map(|w| repackage_witness(ctor, arity, w))
        }
        Pat::Wildcard => {
            if let Some(all) = enumerate_ctors(head_ty, env) {
                for c in &all {
                    let arity = ctor_arity(c, head_ty, env);
                    let spec = specialize(matrix, c, arity);
                    let mut new_q: Vec<Pat> = (0..arity).map(|_| Pat::Wildcard).collect();
                    new_q.extend_from_slice(q_rest);
                    let mut new_head_types = ctor_field_types(c, head_ty, env);
                    new_head_types.extend_from_slice(rest_tys);
                    if let Some(w) = usefulness(&spec, &new_q, &new_head_types, env) {
                        return Some(repackage_witness(c, arity, w));
                    }
                }
                None
            } else {
                let default = default_matrix(matrix);
                usefulness(&default, q_rest, rest_tys, env).map(|w| {
                    let mut out = Vec::with_capacity(w.len() + 1);
                    out.push(Pat::Wildcard);
                    out.extend(w);
                    out
                })
            }
        }
    }
}

/// Take a sub-witness produced by recursing into `ctor`'s `arity` fields and
/// fold it back into a witness for the parent column: the first `arity`
/// elements of `inner` are the ctor's args, the remainder is the tail of
/// the parent query.
fn repackage_witness(ctor: &PatCtor, arity: usize, inner: Vec<Pat>) -> Vec<Pat> {
    let mut iter = inner.into_iter();
    let args: Vec<Pat> = iter.by_ref().take(arity).collect();
    let tail: Vec<Pat> = iter.collect();
    let mut out = Vec::with_capacity(tail.len() + 1);
    out.push(Pat::Ctor {
        ctor: ctor.clone(),
        args,
    });
    out.extend(tail);
    out
}

/// `S(c, P)`: rows that begin with `c(...)` or `_`, with the head replaced by
/// `c`'s arguments (a wildcard row contributes `arity` wildcards).
fn specialize(matrix: &Matrix, ctor: &PatCtor, arity: usize) -> Matrix {
    let mut out = Matrix::default();
    for row in &matrix.rows {
        let Some((head, tail)) = row.pats.split_first() else {
            continue;
        };
        match head {
            Pat::Wildcard => {
                let mut new_pats: Vec<Pat> = (0..arity).map(|_| Pat::Wildcard).collect();
                new_pats.extend_from_slice(tail);
                out.rows.push(Row { pats: new_pats });
            }
            Pat::Ctor {
                ctor: row_ctor,
                args,
            } if row_ctor == ctor => {
                let mut new_pats = args.clone();
                new_pats.extend_from_slice(tail);
                out.rows.push(Row { pats: new_pats });
            }
            Pat::Ctor { .. } => {}
            Pat::Or(alts) => {
                for alt in alts {
                    let mut alt_row = vec![alt.clone()];
                    alt_row.extend_from_slice(tail);
                    let mini = Matrix {
                        rows: vec![Row { pats: alt_row }],
                    };
                    out.rows.extend(specialize(&mini, ctor, arity).rows);
                }
            }
        }
    }
    out
}

/// `D(P)`: rows that begin with `_`, with the head dropped.
fn default_matrix(matrix: &Matrix) -> Matrix {
    let mut out = Matrix::default();
    for row in &matrix.rows {
        let Some((head, tail)) = row.pats.split_first() else {
            continue;
        };
        match head {
            Pat::Wildcard => out.rows.push(Row {
                pats: tail.to_vec(),
            }),
            Pat::Ctor { .. } => {}
            Pat::Or(alts) => {
                for alt in alts {
                    let mut alt_row = vec![alt.clone()];
                    alt_row.extend_from_slice(tail);
                    let mini = Matrix {
                        rows: vec![Row { pats: alt_row }],
                    };
                    out.rows.extend(default_matrix(&mini).rows);
                }
            }
        }
    }
    out
}

fn enumerate_ctors(ty: &Type, env: &TypeEnv) -> Option<Vec<PatCtor>> {
    match ty {
        Type::Bool => Some(vec![PatCtor::Bool(true), PatCtor::Bool(false)]),
        Type::Tuple(_) => Some(vec![PatCtor::Tuple]),
        // Never has no inhabitants — empty constructor list. Any match
        // (including the empty match) is vacuously exhaustive.
        Type::Never => Some(Vec::new()),
        Type::Named { name, .. } => {
            if let Some(info) = env.enums.get(name) {
                Some(
                    info.variants
                        .iter()
                        .map(|(v, _)| PatCtor::Variant(v.clone()))
                        .collect(),
                )
            } else if env.structs.contains_key(name) {
                Some(vec![PatCtor::Struct(name.clone())])
            } else {
                None
            }
        }
        // Integer / float / char / str / slice / array — open domains that
        // require a wildcard arm. Returning None routes the wildcard branch
        // of `is_useful` through the default matrix.
        _ => None,
    }
}

fn ctor_arity(ctor: &PatCtor, parent_ty: &Type, env: &TypeEnv) -> usize {
    ctor_field_types(ctor, parent_ty, env).len()
}

fn ctor_field_types(ctor: &PatCtor, parent_ty: &Type, env: &TypeEnv) -> Vec<Type> {
    match ctor {
        PatCtor::Bool(_) | PatCtor::Lit(_) => vec![],
        PatCtor::Variant(name) => {
            if let Type::Named {
                name: enum_name, ..
            } = parent_ty
            {
                if let Some(info) = env.enums.get(enum_name) {
                    if let Some((_, vinfo)) = info.variants.iter().find(|(v, _)| v == name) {
                        return match vinfo {
                            VariantTypeInfo::Unit => vec![],
                            VariantTypeInfo::Tuple(types) => types.clone(),
                            VariantTypeInfo::Struct(fields) => {
                                fields.iter().map(|(_, t)| t.clone()).collect()
                            }
                        };
                    }
                }
            }
            vec![]
        }
        PatCtor::Tuple => match parent_ty {
            Type::Tuple(elems) => elems.clone(),
            _ => vec![],
        },
        PatCtor::Struct(name) => env
            .structs
            .get(name)
            .map(|info| info.fields.iter().map(|(_, t, _)| t.clone()).collect())
            .unwrap_or_default(),
    }
}

fn render_witness(witness: &Pat, ty: &Type, env: &TypeEnv) -> String {
    match witness {
        Pat::Wildcard => "_".to_string(),
        // Or shouldn't appear in a witness — the recursion picks one
        // alternative — but render defensively just in case.
        Pat::Or(alts) => alts
            .first()
            .map(|a| render_witness(a, ty, env))
            .unwrap_or_else(|| "_".to_string()),
        Pat::Ctor { ctor, args } => render_ctor(ctor, args, ty, env),
    }
}

fn render_ctor(ctor: &PatCtor, args: &[Pat], ty: &Type, env: &TypeEnv) -> String {
    match ctor {
        PatCtor::Bool(b) => b.to_string(),
        PatCtor::Lit(PatLit::Integer(n)) => n.to_string(),
        PatCtor::Lit(PatLit::Char(c)) => format!("'{c}'"),
        PatCtor::Lit(PatLit::String(s)) => format!("{s:?}"),
        PatCtor::Variant(name) => render_variant(name, args, ty, env),
        PatCtor::Tuple => render_tuple(args, ty, env),
        PatCtor::Struct(name) => render_struct(name, args, env),
    }
}

fn render_variant(name: &str, args: &[Pat], ty: &Type, env: &TypeEnv) -> String {
    if args.is_empty() {
        return name.to_string();
    }
    if let Type::Named {
        name: enum_name, ..
    } = ty
    {
        if let Some(info) = env.enums.get(enum_name) {
            if let Some((_, vinfo)) = info.variants.iter().find(|(v, _)| v == name) {
                match vinfo {
                    VariantTypeInfo::Tuple(types) => {
                        let parts: Vec<String> = args
                            .iter()
                            .zip(types.iter().chain(std::iter::repeat(&Type::Unit)))
                            .map(|(a, t)| render_witness(a, t, env))
                            .collect();
                        return format!("{name}({})", parts.join(", "));
                    }
                    VariantTypeInfo::Struct(fields) => {
                        let parts: Vec<String> = args
                            .iter()
                            .zip(fields.iter())
                            .map(|(a, (fname, fty))| {
                                format!("{fname}: {}", render_witness(a, fty, env))
                            })
                            .collect();
                        return format!("{name} {{ {} }}", parts.join(", "));
                    }
                    VariantTypeInfo::Unit => return name.to_string(),
                }
            }
        }
    }
    let parts: Vec<String> = args
        .iter()
        .map(|a| render_witness(a, &Type::Unit, env))
        .collect();
    format!("{name}({})", parts.join(", "))
}

fn render_tuple(args: &[Pat], ty: &Type, env: &TypeEnv) -> String {
    let elem_types: Vec<Type> = match ty {
        Type::Tuple(elems) => elems.clone(),
        _ => Vec::new(),
    };
    let parts: Vec<String> = args
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let t = elem_types.get(i).cloned().unwrap_or(Type::Unit);
            render_witness(a, &t, env)
        })
        .collect();
    format!("({})", parts.join(", "))
}

fn render_struct(name: &str, args: &[Pat], env: &TypeEnv) -> String {
    if let Some(info) = env.structs.get(name) {
        let parts: Vec<String> = args
            .iter()
            .zip(info.fields.iter())
            .map(|(a, (fname, fty, _))| format!("{fname}: {}", render_witness(a, fty, env)))
            .collect();
        return format!("{name} {{ {} }}", parts.join(", "));
    }
    format!("{name} {{ .. }}")
}
