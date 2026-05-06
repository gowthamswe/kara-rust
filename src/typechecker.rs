// src/typechecker.rs

//! Type checking for the Kāra language.
//!
//! Walks the AST with resolved names, builds a type environment from
//! top-level definitions, then type-checks function bodies. Produces
//! typed expression info and diagnostics.

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
        size: usize,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UIntSize {
    U8,
    U16,
    U32,
    U64,
    Usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatSize {
    F32,
    F64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeVarId(pub u32);

#[derive(Debug, Clone, PartialEq)]
pub enum VariantTypeInfo {
    Unit,
    Tuple(Vec<Type>),
    Struct(Vec<(String, Type)>),
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

pub fn type_display(ty: &Type) -> String {
    match ty {
        Type::Int(s) => match s {
            IntSize::I8 => "i8",
            IntSize::I16 => "i16",
            IntSize::I32 => "i32",
            IntSize::I64 => "i64",
        }
        .to_string(),
        Type::UInt(s) => match s {
            UIntSize::U8 => "u8",
            UIntSize::U16 => "u16",
            UIntSize::U32 => "u32",
            UIntSize::U64 => "u64",
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
        Type::Array { element, size } => format!("Array[{}, {}]", type_display(element), size),
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
        Type::Array { element, .. } | Type::Slice { element, .. } => contains_type_param(element),
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
/// from `subs`. Surviving callers (`element_type_of`,
/// `dispatch_trait_assoc_fn`, `check_pattern_against`) all build `subs`
/// externally from concrete types and use this purely as a tree-walk
/// utility — they do *not* perform type inference. Inference at call
/// sites uses the metavar substrate (`instantiate_signature_with_fresh_vars`
/// + `unify_types` + `resolve_type_vars`, item 131 sub-step 2b) instead.
///
/// Unsolved params pass through unchanged.
fn substitute_type_params(ty: &Type, subs: &HashMap<String, Type>) -> Type {
    match ty {
        Type::TypeParam(name) => subs.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Type::Tuple(elems) => Type::Tuple(
            elems
                .iter()
                .map(|e| substitute_type_params(e, subs))
                .collect(),
        ),
        Type::Array { element, size } => Type::Array {
            element: Box::new(substitute_type_params(element, subs)),
            size: *size,
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
            if let Some(concrete) = subs.get(param) {
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
) -> (
    Vec<Type>,
    Type,
    HashMap<String, TypeVarId>,
    HashMap<TypeVarId, String>,
) {
    fn collect(ty: &Type, names: &mut Vec<String>, seen: &mut HashSet<String>) {
        match ty {
            Type::TypeParam(n) if seen.insert(n.clone()) => {
                names.push(n.clone());
            }
            Type::TypeParam(_) => {}
            Type::Tuple(es) => {
                for e in es {
                    collect(e, names, seen);
                }
            }
            Type::Array { element, .. } | Type::Slice { element, .. } => {
                collect(element, names, seen)
            }
            Type::Ref(i) | Type::MutRef(i) | Type::Weak(i) => collect(i, names, seen),
            Type::Pointer { inner, .. } => collect(inner, names, seen),
            Type::Named { args, .. } => {
                for a in args {
                    collect(a, names, seen);
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
                    collect(p, names, seen);
                }
                collect(return_type, names, seen);
            }
            // AssocProjection.param is a String holding the resolved
            // concrete type name; not a TypeParam introduction site.
            _ => {}
        }
    }
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in params {
        collect(p, &mut names, &mut seen);
    }
    collect(return_type, &mut names, &mut seen);

    let mut name_to_id: HashMap<String, TypeVarId> = HashMap::new();
    let mut id_to_name: HashMap<TypeVarId, String> = HashMap::new();
    for name in &names {
        let id = TypeVarId(*next_type_var);
        *next_type_var += 1;
        name_to_id.insert(name.clone(), id);
        id_to_name.insert(id, name.clone());
    }

    fn substitute(ty: &Type, name_to_id: &HashMap<String, TypeVarId>) -> Type {
        match ty {
            Type::TypeParam(n) => name_to_id
                .get(n)
                .map(|&id| Type::TypeVar(id))
                .unwrap_or_else(|| ty.clone()),
            Type::Tuple(es) => Type::Tuple(es.iter().map(|e| substitute(e, name_to_id)).collect()),
            Type::Array { element, size } => Type::Array {
                element: Box::new(substitute(element, name_to_id)),
                size: *size,
            },
            Type::Slice { element, mutable } => Type::Slice {
                element: Box::new(substitute(element, name_to_id)),
                mutable: *mutable,
            },
            Type::Ref(inner) => Type::Ref(Box::new(substitute(inner, name_to_id))),
            Type::MutRef(inner) => Type::MutRef(Box::new(substitute(inner, name_to_id))),
            Type::Weak(inner) => Type::Weak(Box::new(substitute(inner, name_to_id))),
            Type::Pointer { is_mut, inner } => Type::Pointer {
                is_mut: *is_mut,
                inner: Box::new(substitute(inner, name_to_id)),
            },
            Type::Named { name, args } => Type::Named {
                name: name.clone(),
                args: args.iter().map(|a| substitute(a, name_to_id)).collect(),
            },
            Type::Function {
                params,
                return_type,
            } => Type::Function {
                params: params.iter().map(|p| substitute(p, name_to_id)).collect(),
                return_type: Box::new(substitute(return_type, name_to_id)),
            },
            Type::OnceFunction {
                params,
                return_type,
            } => Type::OnceFunction {
                params: params.iter().map(|p| substitute(p, name_to_id)).collect(),
                return_type: Box::new(substitute(return_type, name_to_id)),
            },
            _ => ty.clone(),
        }
    }
    let new_params: Vec<Type> = params.iter().map(|p| substitute(p, &name_to_id)).collect();
    let new_ret = substitute(return_type, &name_to_id);
    (new_params, new_ret, name_to_id, id_to_name)
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
) -> Type {
    match ty {
        Type::TypeVar(id) => {
            if let Some(resolved) = substitutions.get(id) {
                resolve_type_vars(resolved, substitutions, id_to_name)
            } else if let Some(name) = id_to_name.get(id) {
                Type::TypeParam(name.clone())
            } else {
                ty.clone()
            }
        }
        Type::Tuple(es) => Type::Tuple(
            es.iter()
                .map(|e| resolve_type_vars(e, substitutions, id_to_name))
                .collect(),
        ),
        Type::Array { element, size } => Type::Array {
            element: Box::new(resolve_type_vars(element, substitutions, id_to_name)),
            size: *size,
        },
        Type::Slice { element, mutable } => Type::Slice {
            element: Box::new(resolve_type_vars(element, substitutions, id_to_name)),
            mutable: *mutable,
        },
        Type::Ref(inner) => Type::Ref(Box::new(resolve_type_vars(
            inner,
            substitutions,
            id_to_name,
        ))),
        Type::MutRef(inner) => Type::MutRef(Box::new(resolve_type_vars(
            inner,
            substitutions,
            id_to_name,
        ))),
        Type::Weak(inner) => Type::Weak(Box::new(resolve_type_vars(
            inner,
            substitutions,
            id_to_name,
        ))),
        Type::Pointer { is_mut, inner } => Type::Pointer {
            is_mut: *is_mut,
            inner: Box::new(resolve_type_vars(inner, substitutions, id_to_name)),
        },
        Type::Named { name, args } => Type::Named {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| resolve_type_vars(a, substitutions, id_to_name))
                .collect(),
        },
        Type::Function {
            params,
            return_type,
        } => Type::Function {
            params: params
                .iter()
                .map(|p| resolve_type_vars(p, substitutions, id_to_name))
                .collect(),
            return_type: Box::new(resolve_type_vars(return_type, substitutions, id_to_name)),
        },
        Type::OnceFunction {
            params,
            return_type,
        } => Type::OnceFunction {
            params: params
                .iter()
                .map(|p| resolve_type_vars(p, substitutions, id_to_name))
                .collect(),
            return_type: Box::new(resolve_type_vars(return_type, substitutions, id_to_name)),
        },
        _ => ty.clone(),
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
fn unify_types(a: &Type, b: &Type, substitutions: &mut HashMap<TypeVarId, Type>) -> bool {
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
            .all(|(x, y)| unify_types(x, y, substitutions)),
        (
            Type::Named {
                name: an,
                args: aa,
            },
            Type::Named {
                name: bn,
                args: bb,
            },
        ) if an == bn && aa.len() == bb.len() => aa
            .iter()
            .zip(bb.iter())
            .all(|(x, y)| unify_types(x, y, substitutions)),
        (Type::Ref(x), Type::Ref(y))
        | (Type::MutRef(x), Type::MutRef(y))
        | (Type::Weak(x), Type::Weak(y)) => unify_types(x, y, substitutions),
        (
            Type::Array {
                element: xe,
                size: xs,
            },
            Type::Array {
                element: ye,
                size: ys,
            },
        ) if xs == ys => unify_types(xe, ye, substitutions),
        (
            Type::Slice {
                element: xe,
                mutable: xm,
            },
            Type::Slice {
                element: ye,
                mutable: ym,
            },
        ) if xm == ym => unify_types(xe, ye, substitutions),
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
                .all(|(x, y)| unify_types(x, y, substitutions))
                && unify_types(xr, yr, substitutions)
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
                .all(|(x, y)| unify_types(x, y, substitutions))
                && unify_types(xr, yr, substitutions)
        }
        // Terminal / cross-shape cases handled by the existing
        // structural compatibility check (covers integer-coercion,
        // never, slice/vec coercions, etc).
        _ => types_compatible(&a, &b),
    }
}

/// Walk `ty` for a `TypeParam(name)` whose name is **not** in
/// `in_scope`. Returns the first such name. Used by the unsolved-T
/// diagnostic (item 131 sub-step 2a) at synthesis-mode let bindings:
/// any TypeParam that didn't get pinned by arguments and doesn't
/// belong to an enclosing function/impl generic is unsolved at this
/// site.
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
        _ => types_compatible(super_ty, sub_ty),
    }
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
}

#[derive(Debug, Clone)]
pub struct ImplInfo {
    pub target_type: String,
    pub trait_name: Option<String>,
    pub methods: HashMap<String, FunctionSig>,
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
}

impl TypeEnv {
    fn new() -> Self {
        TypeEnv {
            structs: HashMap::new(),
            enums: HashMap::new(),
            distinct_types: HashMap::new(),
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

    /// Look up the impl of `trait_name` for `target_type`.
    /// Match is on the type's nominal head — generic parameters are ignored at this layer
    /// (callers responsible for any parameter-level subtype checks).
    pub fn find_impl(&self, trait_name: &str, target_type: &str) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get(trait_name)?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| imp.target_type == target_type)
    }

    pub fn has_impl(&self, trait_name: &str, target_type: &str) -> bool {
        self.find_impl(trait_name, target_type).is_some()
    }

    /// Look up a method by name on `target_type` from `impls`, preferring
    /// inherent methods over trait methods per design.md § Method Resolution
    /// Step 3. Returns the first inherent impl's method if any matches;
    /// otherwise the first trait impl's method. First-match within each tier
    /// — multi-inherent and multi-trait ambiguity detection is deferred to
    /// the step-4 work tracked in phase-4-interpreter.md.
    pub fn find_method(&self, target_type: &str, method: &str) -> Option<&FunctionSig> {
        let mut inherent: Option<&FunctionSig> = None;
        let mut trait_method: Option<&FunctionSig> = None;
        for imp in &self.impls {
            if imp.target_type != target_type {
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
    /// Step 7).
    pub fn collect_method_names(&self, target_type: &str) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for imp in &self.impls {
            if imp.target_type != target_type {
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

    /// Find a `From` impl mapping `source` → `target`. Disambiguates
    /// multiple `impl From[X] for T` impls for the same target by matching
    /// the `from` method's first parameter type against `source`.
    pub fn find_from_impl(&self, source: &Type, target: &str) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get("From")?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target
                    && imp.methods.get("from").and_then(|sig| sig.params.first()) == Some(source)
            })
    }

    /// Find a `TryFrom` impl mapping `source` → `target`. Disambiguates
    /// multiple `impl TryFrom[X] for T` impls for the same target by matching
    /// the `try_from` method's first parameter type against `source`.
    pub fn find_tryfrom_impl(&self, source: &Type, target: &str) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get("TryFrom")?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target
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
    /// Trait bounds for the generic parameters in the current enclosing scope
    /// (impl-level + function/method-level). Indexed by the param's textual
    /// name so it pairs naturally with `Type::TypeParam(name)`. Populated on
    /// entering a generic-bearing scope and saved/restored on exit, mirroring
    /// the enclosing-generic-name list threaded through the lower / check
    /// path. Used to resolve bare `method(args)` calls at expected-type
    /// positions when the expected type is a generic param.
    enclosing_bounds: HashMap<String, Vec<crate::ast::TraitBound>>,
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
            enclosing_bounds: HashMap::new(),
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
        self.validate_derive_arithmetic();
        self.check_signature_visibility();
        self.check_items();
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
        if let Some(suggestion) =
            crate::edit_distance::suggest_similar(method, known_methods)
        {
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

    /// Map a lexer-provided integer suffix to the concrete `Type` it denotes.
    /// `None` defaults to `i64`. `I128` / `U128` are not yet supported —
    /// emit a structured diagnostic and fall back to `i64` to keep inference
    /// going.
    fn type_from_int_suffix(&mut self, sfx: Option<IntSuffix>, span: Span) -> Type {
        match sfx {
            None => Type::Int(IntSize::I64),
            Some(IntSuffix::I8) => Type::Int(IntSize::I8),
            Some(IntSuffix::I16) => Type::Int(IntSize::I16),
            Some(IntSuffix::I32) => Type::Int(IntSize::I32),
            Some(IntSuffix::I64) => Type::Int(IntSize::I64),
            Some(IntSuffix::U8) => Type::UInt(UIntSize::U8),
            Some(IntSuffix::U16) => Type::UInt(UIntSize::U16),
            Some(IntSuffix::U32) => Type::UInt(UIntSize::U32),
            Some(IntSuffix::U64) => Type::UInt(UIntSize::U64),
            Some(IntSuffix::I128) | Some(IntSuffix::U128) => {
                self.type_error(
                    "128-bit integer literals are not yet supported".to_string(),
                    span,
                    TypeErrorKind::UnsupportedNumericSuffix,
                );
                Type::Int(IntSize::I64)
            }
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
            Type::Named { name, .. } => {
                // A user-provided `impl Eq for Name` is sufficient — the
                // lowering pass dispatches `==`/`!=` through it. Falls back
                // to `#[derive(Eq)]`/`#[derive(PartialEq)]` when no impl is
                // registered (e.g. for compiler-provided structural eq on
                // built-in enums like `Option`/`Result`).
                if self.env.has_impl("Eq", name) {
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
                    if self.env.has_impl("Display", name) {
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

    fn lower_type_expr(&self, ty: &TypeExpr, generic_scope: &[String]) -> Type {
        match &ty.kind {
            TypeKind::Path(path) => self.lower_path_type(path, generic_scope),
            TypeKind::Tuple(types) => Type::Tuple(
                types
                    .iter()
                    .map(|t| self.lower_type_expr(t, generic_scope))
                    .collect(),
            ),
            TypeKind::Array { element, .. } => {
                Type::Array {
                    element: Box::new(self.lower_type_expr(element, generic_scope)),
                    size: 0, // const eval deferred
                }
            }
            TypeKind::Pointer { is_mut, inner } => Type::Pointer {
                is_mut: *is_mut,
                inner: Box::new(self.lower_type_expr(inner, generic_scope)),
            },
            TypeKind::FnType {
                params,
                return_type,
                is_once,
                ..
            } => {
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|t| self.lower_type_expr(t, generic_scope))
                    .collect();
                let ret = return_type
                    .as_ref()
                    .map(|t| self.lower_type_expr(t, generic_scope))
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
            TypeKind::Ref(inner) => Type::Ref(Box::new(self.lower_type_expr(inner, generic_scope))),
            TypeKind::MutRef(inner) => {
                Type::MutRef(Box::new(self.lower_type_expr(inner, generic_scope)))
            }
            TypeKind::MutSlice(element) => Type::Slice {
                element: Box::new(self.lower_type_expr(element, generic_scope)),
                mutable: true,
            },
            TypeKind::Weak(inner) => {
                Type::Weak(Box::new(self.lower_type_expr(inner, generic_scope)))
            }
            TypeKind::Unit => Type::Unit,
            TypeKind::Error => Type::Error,
        }
    }

    fn lower_generic_args(
        &self,
        generic_args: &Option<Vec<GenericArg>>,
        generic_scope: &[String],
    ) -> Vec<Type> {
        generic_args
            .as_ref()
            .map(|ga| {
                ga.iter()
                    .filter_map(|arg| match arg {
                        GenericArg::Type(t) => Some(self.lower_type_expr(t, generic_scope)),
                        GenericArg::Const(_) => None, // const args don't produce types
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn lower_path_type(&self, path: &PathExpr, generic_scope: &[String]) -> Type {
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
            let args = self.lower_generic_args(&path.generic_args, generic_scope);
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
            let args = self.lower_generic_args(&path.generic_args, generic_scope);
            Type::Named { name, args }
        }
    }

    /// Lower `Array[T, N]` to `Type::Array { element, size }`.
    /// N must be a positive integer literal (const-eval of arithmetic expressions deferred).
    fn lower_array_type(
        &self,
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
        let size = match &args[1] {
            GenericArg::Const(expr) => match &expr.kind {
                ExprKind::Integer(n, _) if *n >= 0 => *n as usize,
                _ => return None,
            },
            GenericArg::Type(_) => return None,
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
        &self,
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
            "u8" => Some(Type::UInt(UIntSize::U8)),
            "u16" => Some(Type::UInt(UIntSize::U16)),
            "u32" => Some(Type::UInt(UIntSize::U32)),
            "u64" => Some(Type::UInt(UIntSize::U64)),
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
                size: *size,
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
                let subs: HashMap<String, Type> = generic_params
                    .iter()
                    .zip(args.iter())
                    .map(|(p, a)| (p.clone(), a.clone()))
                    .collect();
                substitute_type_params(item_ty, &subs)
            }
            _ => ty.clone(),
        }
    }

    // ── Build Type Environment (Pass 1) ─────────────────────────

    fn build_type_env(&mut self) {
        // Register built-in stdlib types
        self.register_builtin_types();

        let items: Vec<Item> = self.program.items.clone();
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
                        .insert(bound, (origin_path, origin_name, vis));
                }
            }
        }
    }

    /// Register built-in stdlib types (F32, F64, Atomic, Ordering, etc.)
    ///
    /// CR-24 slice 8: this was originally the *shim* that backs the synthetic
    /// `std.prelude` module added by `module::build_program_tree`. The
    /// stub items in `crate::prelude::synthetic_prelude_items` exist only
    /// so cross-module resolution can find a path for `import std.prelude.X;`;
    /// the real type-environment entries live here.
    ///
    /// CR-202 is incrementally migrating the prelude surface to real
    /// `runtime/stdlib/*.kara` source. The first pass below walks every
    /// item in `prelude::STDLIB_PROGRAMS` and registers it via the same
    /// `env_add_*` path that user items use; the hardcoded blocks below
    /// stay in place for everything not yet migrated, and are deleted in
    /// per-type slice 4+ commits as the bake surface grows. The pilot
    /// (`Option`, slice 3) is the first deletion.
    fn register_builtin_types(&mut self) {
        // ── Baked stdlib source (CR-202 source-of-truth swap) ─────────────
        // For every item in `runtime/stdlib/*.kara`, register it through
        // the same path user items use. This replaces the corresponding
        // hardcoded blocks below as each prelude type is migrated.
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
                | Item::DistinctType(_)
                | Item::EffectResource(_)
                | Item::EffectGroup(_)
                | Item::EffectVerbDecl(_)
                | Item::UseDecl(_)
                | Item::Import(_)
                | Item::LayoutDef(_)
                | Item::AliasDecl(_)
                | Item::IndependentDecl(_) => {
                    // Not yet exercised by baked stdlib source — slice 4+
                    // will broaden this match as the surface grows.
                }
            }
        }

        // Map[K, V] — insertion-order key-value map. Method dispatch is handled
        // by `infer_map_method`. Registered with two type params K, V.
        self.env.structs.insert(
            "Map".to_string(),
            StructInfo {
                generic_params: vec!["K".to_string(), "V".to_string()],
                fields: vec![],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            },
        );

        // SortedSet[T: Ord] — B-tree–backed ordered set. Registered as a
        // generic struct with one type param so the typechecker accepts
        // `SortedSet[i64]` in type positions. Method dispatch is handled by
        // `infer_sorted_set_method` rather than the impl table.
        self.env.structs.insert(
            "SortedSet".to_string(),
            StructInfo {
                generic_params: vec!["T".to_string()],
                fields: vec![],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            },
        );

        // Channel[T] / Sender[T] / Receiver[T] — concurrency primitives.
        // Channel is only used at construction time (Channel.new()); Sender
        // and Receiver are the values users hold and pass around. Method
        // dispatch is handled by `infer_channel_method`.
        for name in &["Channel", "Sender", "Receiver"] {
            self.env.structs.insert(
                name.to_string(),
                StructInfo {
                    generic_params: vec!["T".to_string()],
                    fields: vec![],
                    derived_traits: HashSet::new(),
                    no_rc: false,
                    is_shared: false,
                },
            );
        }

        // ── Iterator and IntoIterator traits ──────────────────────────────
        //
        // Register as real traits with associated types so that:
        //   (a) `impl Iterator for MyType { type Item = T; }` validates,
        //   (b) `T: Iterator` bounds check `T.Item` projections,
        //   (c) the for-loop element-type lookup can go through `element_type_of`.
        self.env.traits.insert(
            "Iterator".to_string(),
            TraitInfo {
                assoc_types: vec!["Item".to_string()],
                supertraits: vec![],
            },
        );
        self.env.traits.insert(
            "IntoIterator".to_string(),
            TraitInfo {
                assoc_types: vec!["Item".to_string(), "IntoIter".to_string()],
                supertraits: vec![],
            },
        );

        // ── Builtin IntoIterator / Iterator impls for stdlib collections ──
        //
        // Stored in `impl_assoc_types` as `(concrete_type_name, assoc_name) → Type`
        // where the type still contains `TypeParam` placeholders. `element_type_of`
        // resolves those against the struct's `generic_params` and the concrete
        // type arguments at use-site. Range* use segment 0 as the element type.
        let t = || Type::TypeParam("T".to_string());
        let k = || Type::TypeParam("K".to_string());
        let v = || Type::TypeParam("V".to_string());

        // `Vec` was previously registered in this loop; CR-202 slice 4b
        // moved its shape to `runtime/stdlib/vec.kara` (baked-source pass
        // at the top of this function). The `("Vec", "Item")` entry in
        // `impl_assoc_types` is restored explicitly below so the iterator
        // element-type resolution keeps working.
        for name in &["Array", "SortedSet", "Set", "Iterator", "Peekable"] {
            // Register in env.structs so element_type_of can find generic_params.
            // `Iterator` is a trait per design.md but is treated as a parametric
            // pseudo-type at this layer so `for x in v.iter()` resolves the
            // bound element via the same impl_assoc_types path as collections.
            // `Peekable[T]` is the result type of `Iterator.peekable()` — same
            // shape as Iterator (single Item type) plus an extra `peek()`
            // method dispatched only when the receiver carries this name.
            // See `wip-list2.md` § Iterator trait — full adaptor surface.
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
        self.env
            .impl_assoc_types
            .insert(("Vec".to_string(), "Item".to_string()), t());
        // Map[K, V] yields (K, V) tuples
        self.env.impl_assoc_types.insert(
            ("Map".to_string(), "Item".to_string()),
            Type::Tuple(vec![k(), v()]),
        );
        // Range types yield the element type (stored at args[0]).
        // Register both the struct (so element_type_of can find generic_params)
        // and the impl_assoc_types entry.
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
        // Slice[T] — iteration yields T
        self.env
            .impl_assoc_types
            .insert(("Slice".to_string(), "Item".to_string()), t());
        // Set[T] (unordered) — yields T
        self.env
            .impl_assoc_types
            .insert(("Set".to_string(), "Item".to_string()), t());

        // F32: total-order float wrapper with Eq, Ord, Hash
        let f32_traits: HashSet<String> = ["Eq", "Ord", "Hash", "PartialEq", "PartialOrd", "Copy"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        self.env.structs.insert(
            "F32".to_string(),
            StructInfo {
                generic_params: vec![],
                fields: vec![("value".to_string(), Type::Float(FloatSize::F32), false)],
                derived_traits: f32_traits,
                no_rc: false,
                is_shared: false,
            },
        );

        // F64: total-order float wrapper with Eq, Ord, Hash
        let f64_traits: HashSet<String> = ["Eq", "Ord", "Hash", "PartialEq", "PartialOrd", "Copy"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        self.env.structs.insert(
            "F64".to_string(),
            StructInfo {
                generic_params: vec![],
                fields: vec![("value".to_string(), Type::Float(FloatSize::F64), false)],
                derived_traits: f64_traits,
                no_rc: false,
                is_shared: false,
            },
        );

        // Ordering enum for Atomic operations
        let ordering_traits: HashSet<String> =
            ["Eq", "Copy"].iter().map(|s| s.to_string()).collect();
        self.env.enums.insert(
            "Ordering".to_string(),
            EnumInfo {
                generic_params: vec![],
                variants: vec![
                    ("Relaxed".to_string(), VariantTypeInfo::Unit),
                    ("Acquire".to_string(), VariantTypeInfo::Unit),
                    ("Release".to_string(), VariantTypeInfo::Unit),
                    ("AcqRel".to_string(), VariantTypeInfo::Unit),
                    ("SeqCst".to_string(), VariantTypeInfo::Unit),
                ],
                derived_traits: ordering_traits,
                is_shared: false,
            },
        );

        // CR-202: `Option` (slice 3c) and `Result` (slice 4a) are now
        // provided by `runtime/stdlib/{option,result}.kara` via the
        // baked-source pass at the top of this function. Structural
        // derives (`Eq`, `PartialEq`, `Hash`, `Ord`, `PartialOrd`) flow
        // through `#[derive(...)]` on the baked enums. `Entry` and the
        // rest stay hardcoded until later slices migrate them.

        // Prelude enum `Entry[K, V]` — view returned by `Map.entry(k)` for
        // O(1) insert-or-modify without double-hashing. See design.md
        // § Entry[K, V]. First stdlib type with `mut ref` payload fields.
        self.env.enums.insert(
            "Entry".to_string(),
            EnumInfo {
                generic_params: vec!["K".to_string(), "V".to_string()],
                variants: vec![
                    (
                        "Occupied".to_string(),
                        VariantTypeInfo::Struct(vec![(
                            "value".to_string(),
                            Type::MutRef(Box::new(Type::TypeParam("V".to_string()))),
                        )]),
                    ),
                    (
                        "Vacant".to_string(),
                        VariantTypeInfo::Struct(vec![
                            ("key".to_string(), Type::TypeParam("K".to_string())),
                            (
                                "map".to_string(),
                                Type::MutRef(Box::new(Type::Named {
                                    name: "Map".to_string(),
                                    args: vec![
                                        Type::TypeParam("K".to_string()),
                                        Type::TypeParam("V".to_string()),
                                    ],
                                })),
                            ),
                        ]),
                    ),
                ],
                derived_traits: HashSet::new(),
                is_shared: false,
            },
        );

        // `VarError` — error type returned by `env.var(name)`. Resolved as
        // `std.env.VarError` per brainstorming v49 (Q2=B): not part of the
        // prelude (so it stays out of `PRELUDE_TYPES` / `PRELUDE_VARIANTS`),
        // but registered here in the same shim as `Option`/`Result` per Q3=A
        // until real stdlib materialisation lands. Single `NotPresent` variant
        // (Q1=B): `Result[String, VarError]` stays forward-compatible if we
        // ever add reasons (e.g. permission errors), but no payload today —
        // Kāra's strict-UTF-8 `String` rules out a Rust-style
        // `NotUnicode(OsString)` carrier.
        self.env.enums.insert(
            "VarError".to_string(),
            EnumInfo {
                generic_params: vec![],
                variants: vec![
                    ("NotPresent".to_string(), VariantTypeInfo::Unit),
                    ("NotUnicode".to_string(), VariantTypeInfo::Unit),
                ],
                derived_traits: HashSet::new(),
                is_shared: false,
            },
        );

        // `IoError` — error type returned by I/O standard library functions.
        // Variants: NotFound, PermissionDenied, AlreadyExists, UnexpectedEof,
        // InvalidUtf8, Interrupted (all unit), and Other(String) with payload.
        // Lives in the prelude so it does not need an import.
        self.env.enums.insert(
            "IoError".to_string(),
            EnumInfo {
                generic_params: vec![],
                variants: vec![
                    ("NotFound".to_string(), VariantTypeInfo::Unit),
                    ("PermissionDenied".to_string(), VariantTypeInfo::Unit),
                    ("AlreadyExists".to_string(), VariantTypeInfo::Unit),
                    ("UnexpectedEof".to_string(), VariantTypeInfo::Unit),
                    ("InvalidUtf8".to_string(), VariantTypeInfo::Unit),
                    ("Interrupted".to_string(), VariantTypeInfo::Unit),
                    ("Other".to_string(), VariantTypeInfo::Tuple(vec![Type::Str])),
                ],
                derived_traits: HashSet::new(),
                is_shared: false,
            },
        );

        // ── Standard I/O function signatures ───────────────────────────────────

        let io_error_ty = Type::Named {
            name: "IoError".to_string(),
            args: vec![],
        };
        let result_str_io = Type::Named {
            name: "Result".to_string(),
            args: vec![Type::Str, io_error_ty.clone()],
        };
        let result_unit_io = Type::Named {
            name: "Result".to_string(),
            args: vec![Type::Unit, io_error_ty],
        };

        // Stdin.read_line() -> Result[str, IoError]
        self.env.functions.insert(
            "Stdin.read_line".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: result_str_io.clone(),
            },
        );
        // Stdin.read_to_string() -> Result[str, IoError]
        self.env.functions.insert(
            "Stdin.read_to_string".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: result_str_io.clone(),
            },
        );
        // Stdout.flush() -> Unit
        self.env.functions.insert(
            "Stdout.flush".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: Type::Unit,
            },
        );
        // Stderr.flush() -> Unit
        self.env.functions.insert(
            "Stderr.flush".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: Type::Unit,
            },
        );
        // FileSystem.read_to_string(path: str) -> Result[str, IoError]
        self.env.functions.insert(
            "FileSystem.read_to_string".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("path".to_string())],
                params: vec![Type::Str],
                return_type: result_str_io,
            },
        );
        // FileSystem.write(path: str, contents: str) -> Result[Unit, IoError]
        self.env.functions.insert(
            "FileSystem.write".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("path".to_string()), Some("contents".to_string())],
                params: vec![Type::Str, Type::Str],
                return_type: result_unit_io,
            },
        );

        // Env.args() -> Vec[String] with reads(Env)
        // Registered under both "Env.args" (capitalized, for Env.args() form)
        // and "env.args" (lowercase module path, design.md § I/O).
        let vec_string = Type::Named {
            name: "Vec".to_string(),
            args: vec![Type::Str],
        };
        let args_sig = FunctionSig {
            generic_params: vec![],
            param_names: vec![],
            params: vec![],
            return_type: vec_string,
        };
        self.env
            .functions
            .insert("Env.args".to_string(), args_sig.clone());
        self.env.functions.insert("env.args".to_string(), args_sig);

        // Env.var(name: String) -> Result[String, VarError] with reads(Env)
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
        };
        self.env
            .functions
            .insert("Env.var".to_string(), var_sig.clone());
        self.env.functions.insert("env.var".to_string(), var_sig);

        // Register process.exit in the function table
        self.env.functions.insert(
            "process.exit".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("code".to_string())],
                params: vec![Type::Int(IntSize::I32)],
                return_type: Type::Never,
            },
        );

        // ── Stats namespace ───────────────────────────────────────────────────
        // Free statistical functions over Slice[f64]. Effect-free.
        let slice_f64 = Type::Slice {
            element: Box::new(Type::Float(FloatSize::F64)),
            mutable: false,
        };
        let option_f64 = Type::Named {
            name: "Option".to_string(),
            args: vec![Type::Float(FloatSize::F64)],
        };
        for name in &[
            "Stats.sum",
            "Stats.prod",
            "Stats.mean",
            "Stats.variance",
            "Stats.stddev",
            "Stats.median",
        ] {
            self.env.functions.insert(
                name.to_string(),
                FunctionSig {
                    generic_params: vec![],
                    param_names: vec![Some("xs".to_string())],
                    params: vec![slice_f64.clone()],
                    return_type: Type::Float(FloatSize::F64),
                },
            );
        }
        // min/max return Option[f64] to be safe on empty slices
        for name in &["Stats.min", "Stats.max"] {
            self.env.functions.insert(
                name.to_string(),
                FunctionSig {
                    generic_params: vec![],
                    param_names: vec![Some("xs".to_string())],
                    params: vec![slice_f64.clone()],
                    return_type: option_f64.clone(),
                },
            );
        }

        // ── Regex namespace ───────────────────────────────────────────────────
        // Interpreter-only (no codegen). Backed by the `regex` crate at runtime.
        let regex_ty = Type::Named {
            name: "Regex".to_string(),
            args: vec![],
        };
        let regex_error_ty = Type::Named {
            name: "RegexError".to_string(),
            args: vec![],
        };
        let result_regex = Type::Named {
            name: "Result".to_string(),
            args: vec![regex_ty.clone(), regex_error_ty.clone()],
        };
        let match_ty = Type::Named {
            name: "Match".to_string(),
            args: vec![],
        };
        let option_match = Type::Named {
            name: "Option".to_string(),
            args: vec![match_ty.clone()],
        };
        let vec_match = Type::Named {
            name: "Vec".to_string(),
            args: vec![match_ty.clone()],
        };
        // Regex.compile(pattern: str) -> Result[Regex, RegexError]
        self.env.functions.insert(
            "Regex.compile".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("pattern".to_string())],
                params: vec![Type::Str],
                return_type: result_regex,
            },
        );
        // Regex methods — registered as Regex.method in the function table so
        // resolve_path_type can find them. Actual dispatch is in eval_method_call.
        self.env.functions.insert(
            "Regex.is_match".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string())],
                params: vec![Type::Str],
                return_type: Type::Bool,
            },
        );
        self.env.functions.insert(
            "Regex.find".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string())],
                params: vec![Type::Str],
                return_type: option_match,
            },
        );
        self.env.functions.insert(
            "Regex.find_all".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string())],
                params: vec![Type::Str],
                return_type: vec_match,
            },
        );
        self.env.functions.insert(
            "Regex.replace_all".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string()), Some("replacement".to_string())],
                params: vec![Type::Str, Type::Str],
                return_type: Type::Str,
            },
        );
        // Register Regex, RegexError, Match as structs so the typechecker
        // accepts them in type positions.
        self.env
            .structs
            .entry("Regex".to_string())
            .or_insert_with(|| StructInfo {
                generic_params: vec![],
                fields: vec![("pattern".to_string(), Type::Str, false)],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            });
        self.env
            .structs
            .entry("RegexError".to_string())
            .or_insert_with(|| StructInfo {
                generic_params: vec![],
                fields: vec![("message".to_string(), Type::Str, false)],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            });
        self.env
            .structs
            .entry("Match".to_string())
            .or_insert_with(|| StructInfo {
                generic_params: vec![],
                fields: vec![
                    ("text".to_string(), Type::Str, false),
                    ("start".to_string(), Type::Int(IntSize::I64), false),
                    ("end".to_string(), Type::Int(IntSize::I64), false),
                ],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            });

        // ── std.http namespace ────────────────────────────────────────────────
        // Interpreter-only. Backed by the `ureq` crate at runtime.
        // Effects: Client.get / Client.post carry sends(Network) + receives(Network).
        let client_ty = Type::Named {
            name: "Client".to_string(),
            args: vec![],
        };
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
            args: vec![response_ty.clone(), http_error_ty.clone()],
        };
        let option_str = Type::Named {
            name: "Option".to_string(),
            args: vec![Type::Str],
        };
        // Client.new() -> Client
        self.env.functions.insert(
            "Client.new".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: client_ty,
            },
        );
        // Client methods (registered in function table for path-call resolution)
        self.env.functions.insert(
            "Client.get".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("url".to_string())],
                params: vec![Type::Str],
                return_type: result_response.clone(),
            },
        );
        self.env.functions.insert(
            "Client.post".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("url".to_string()), Some("body".to_string())],
                params: vec![Type::Str, Type::Str],
                return_type: result_response.clone(),
            },
        );
        // Response methods
        self.env.functions.insert(
            "Response.status".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: Type::Int(IntSize::I64),
            },
        );
        self.env.functions.insert(
            "Response.body".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: Type::Str,
            },
        );
        self.env.functions.insert(
            "Response.header".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("name".to_string())],
                params: vec![Type::Str],
                return_type: option_str,
            },
        );
        // HttpError.message() -> str
        self.env.functions.insert(
            "HttpError.message".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![],
                params: vec![],
                return_type: Type::Str,
            },
        );
        // Register Client, Response, HttpError as structs
        self.env
            .structs
            .entry("Client".to_string())
            .or_insert_with(|| StructInfo {
                generic_params: vec![],
                fields: vec![],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            });
        self.env
            .structs
            .entry("Response".to_string())
            .or_insert_with(|| StructInfo {
                generic_params: vec![],
                fields: vec![
                    ("status".to_string(), Type::Int(IntSize::I64), false),
                    ("body".to_string(), Type::Str, false),
                ],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            });
        self.env
            .structs
            .entry("HttpError".to_string())
            .or_insert_with(|| StructInfo {
                generic_params: vec![],
                fields: vec![("message".to_string(), Type::Str, false)],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            });

        // ── std.encoding namespace (Base64 / Hex / Url) ───────────────────────
        // Interpreter-only. Pure-Rust helpers in `eval_encoding_fn`. Effect-free
        // (encoding/decoding does not touch any tracked resource).
        let slice_u8 = Type::Slice {
            element: Box::new(Type::UInt(UIntSize::U8)),
            mutable: false,
        };
        let vec_u8 = Type::Named {
            name: "Vec".to_string(),
            args: vec![Type::UInt(UIntSize::U8)],
        };
        let decode_error_ty = Type::Named {
            name: "DecodeError".to_string(),
            args: vec![],
        };
        let result_bytes = Type::Named {
            name: "Result".to_string(),
            args: vec![vec_u8.clone(), decode_error_ty.clone()],
        };
        let result_string = Type::Named {
            name: "Result".to_string(),
            args: vec![Type::Str, decode_error_ty.clone()],
        };
        for name in &["Base64.encode", "Base64.encode_url_safe"] {
            self.env.functions.insert(
                name.to_string(),
                FunctionSig {
                    generic_params: vec![],
                    param_names: vec![Some("bytes".to_string())],
                    params: vec![slice_u8.clone()],
                    return_type: Type::Str,
                },
            );
        }
        self.env.functions.insert(
            "Base64.decode".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string())],
                params: vec![Type::Str],
                return_type: result_bytes.clone(),
            },
        );
        for name in &["Hex.encode", "Hex.encode_upper"] {
            self.env.functions.insert(
                name.to_string(),
                FunctionSig {
                    generic_params: vec![],
                    param_names: vec![Some("bytes".to_string())],
                    params: vec![slice_u8.clone()],
                    return_type: Type::Str,
                },
            );
        }
        self.env.functions.insert(
            "Hex.decode".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string())],
                params: vec![Type::Str],
                return_type: result_bytes,
            },
        );
        self.env.functions.insert(
            "Url.encode".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string())],
                params: vec![Type::Str],
                return_type: Type::Str,
            },
        );
        self.env.functions.insert(
            "Url.decode".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("s".to_string())],
                params: vec![Type::Str],
                return_type: result_string,
            },
        );
        for name in &["Base64", "Hex", "Url"] {
            self.env
                .structs
                .entry((*name).to_string())
                .or_insert_with(|| StructInfo {
                    generic_params: vec![],
                    fields: vec![],
                    derived_traits: HashSet::new(),
                    no_rc: false,
                    is_shared: false,
                });
        }
        self.env
            .structs
            .entry("DecodeError".to_string())
            .or_insert_with(|| StructInfo {
                generic_params: vec![],
                fields: vec![("message".to_string(), Type::Str, false)],
                derived_traits: HashSet::new(),
                no_rc: false,
                is_shared: false,
            });

        self.register_stdlib_traits();
        self.register_stdlib_impls();
    }

    /// Register stdlib operator and conversion traits in `env.traits`.
    /// Trait method signatures and impls are registered in subsequent steps;
    /// this pass only seeds the trait names so where-clause validation,
    /// resolver prelude, and impl lookup have something to key off.
    fn register_stdlib_traits(&mut self) {
        // (name, assoc_types)
        let traits: &[(&str, &[&str])] = &[
            // Conversion traits
            ("From", &[]),
            ("Into", &[]),
            ("TryFrom", &["Error"]),
            ("TryInto", &["Error"]),
            // Arithmetic operators
            ("Add", &[]),
            ("Sub", &[]),
            ("Mul", &[]),
            ("Div", &[]),
            ("Rem", &[]),
            ("Neg", &[]),
            // Equality and ordering. `Eq` (slice 5b) and `Ord` (slice 5d)
            // are now provided by `runtime/stdlib/{eq,ord}.kara`; the
            // bake walk at the top of `register_builtin_types` registers
            // them via `env_add_trait`.
            // Bitwise operators
            ("BitAnd", &[]),
            ("BitOr", &[]),
            ("BitXor", &[]),
            ("Shl", &[]),
            ("Shr", &[]),
            ("Not", &[]),
            // Indexing — Output is the element type
            ("Index", &["Output"]),
            ("IndexMut", &["Output"]),
            // String conversion (used by f-string interpolation)
            ("Display", &[]),
        ];
        for (name, assoc) in traits {
            self.env
                .traits
                .entry(name.to_string())
                .or_insert_with(|| TraitInfo {
                    assoc_types: assoc.iter().map(|s| s.to_string()).collect(),
                    supertraits: Vec::new(),
                });
        }
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
        };
        let unop = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into())],
            params: vec![ty.clone()],
            return_type: ty.clone(),
        };
        let eq_sig = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("other".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: Type::Bool,
        };
        let ord_sig = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("other".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: Type::Named {
                name: "Ordering".into(),
                args: vec![],
            },
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
    /// `Ordering`) whose derived-trait bundles are hand-verified.
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
            },
        );
        if f.attributes.iter().any(|a| a.name == "compiler_builtin") {
            self.env.compiler_builtins.insert(f.name.clone());
        }
    }

    fn env_add_impl(&mut self, imp: &ImplBlock) {
        let type_name = match &imp.target_type.kind {
            TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
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
            let gp = Self::generic_param_names(&method.generic_params);
            let param_names: Vec<Option<String>> = method
                .params
                .iter()
                .map(|p: &Param| p.name().map(|s| s.to_string()))
                .collect();
            let params: Vec<Type> = method
                .params
                .iter()
                .map(|p| self.lower_type_expr(&p.ty, &gp))
                .collect();
            let return_type = method
                .return_type
                .as_ref()
                .map(|t| self.lower_type_expr(t, &gp))
                .unwrap_or(Type::Unit);
            methods.insert(
                method.name.clone(),
                FunctionSig {
                    generic_params: gp,
                    param_names,
                    params,
                    return_type,
                },
            );
        }
        self.env.add_impl(ImplInfo {
            target_type: type_name,
            trait_name,
            methods,
        });
    }

    /// Register a built-in stdlib impl programmatically (no AST source).
    /// Used by `register_builtin_types` to seed primitive operator impls.
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
            trait_name: Some(trait_name.to_string()),
            methods,
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
            || self.program.items.iter().any(
                |item| matches!(item, Item::TraitAlias(t) if t.name == trait_name),
            )
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
        if let Some(ref wc) = where_clause {
            self.validate_where_clause(wc, generic_scope);
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
                // Verify the default is a constant expression
                if let Some(bad_span) = self.find_non_const_span(default_expr) {
                    self.type_error(
                        "default parameter value must be a constant expression \
                         (no function calls, closures, or runtime-only values)"
                            .to_string(),
                        bad_span,
                        TypeErrorKind::TypeMismatch,
                    );
                }
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

    /// Returns the span of the first sub-expression that makes `expr`
    /// non-constant, or `None` if the whole expression is a valid constant.
    ///
    /// Constant expressions: literals, unary negation of a literal, named
    /// constants (`ConstDecl`), tuples and arrays of constant expressions,
    /// and binary arithmetic / comparison on constants.
    fn find_non_const_span(&self, expr: &Expr) -> Option<Span> {
        match &expr.kind {
            // Always constant
            ExprKind::Integer(..)
            | ExprKind::Float(..)
            | ExprKind::Bool(_)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_) => None,

            // Negation of a constant is constant
            ExprKind::Unary { operand: inner, .. } => self.find_non_const_span(inner),

            // Binary op on two constants is constant
            ExprKind::Binary { left, right, .. } => self
                .find_non_const_span(left)
                .or_else(|| self.find_non_const_span(right)),

            // Named identifier: constant iff it refers to a ConstDecl
            ExprKind::Identifier(name) => {
                let is_const = self
                    .program
                    .items
                    .iter()
                    .any(|item| matches!(item, Item::ConstDecl(c) if c.name == *name));
                if is_const {
                    None
                } else {
                    Some(expr.span.clone())
                }
            }

            // Tuple/array of constants is constant
            ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
                elems.iter().find_map(|e| self.find_non_const_span(e))
            }

            // Everything else (calls, closures, method calls, etc.) is non-constant
            _ => Some(expr.span.clone()),
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
            | ExprKind::Path(..)
            | ExprKind::SelfType
            | ExprKind::PipePlaceholder
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
            ExprKind::Path(segs) => segs.join("."),
            _ => return None,
        };
        if let Some(sig) = self.env.functions.get(&key) {
            return Some(sig.params.iter().map(Self::is_borrow_param_type).collect());
        }
        if let Some((target, method)) = key.split_once('.') {
            for imp in &self.env.impls {
                if imp.target_type == target {
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
                let has_items = imp.items.iter().any(|item| {
                    matches!(
                        item,
                        ImplItem::Method(_) | ImplItem::AssocType(_)
                    )
                });
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
                // have an impl for the same target type.
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
        let in_scope: HashSet<&str> = self
            .enclosing_bounds
            .keys()
            .map(|s| s.as_str())
            .collect();
        if let Some(name) = find_unbound_type_param(inferred, &in_scope) {
            self.type_error(
                format!(
                    "cannot infer type parameter '{}'; add a type annotation to this binding",
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
            if elements.len() != *size {
                self.type_error(
                    format!(
                        "array literal has {} element(s), expected {}",
                        elements.len(),
                        size
                    ),
                    expr.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
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
                if *n < 0 || *n as usize != *size {
                    self.type_error(
                        format!(
                            "repeat-literal count {} does not match expected array length {}",
                            n, size
                        ),
                        count.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
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
            || matches!(name, "todo" | "unreachable" | "println" | "print" | "panic")
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
                name: target_name, ..
            } => {
                // Match against impl methods registered on this concrete type.
                // Trait impls and inherent impls share the same `env.impls`
                // table; we collect every impl whose target is `target_name`
                // and whose method set contains `name`.
                let matching: Vec<&ImplInfo> = self
                    .env
                    .impls
                    .iter()
                    .filter(|imp| imp.target_type == *target_name && imp.methods.contains_key(name))
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
        let has_generic =
            params.iter().any(contains_type_param) || contains_type_param(return_type);
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
        let (sub_params, sub_ret, name_to_id, id_to_name) =
            instantiate_signature_with_fresh_vars(
                params,
                return_type,
                &mut self.env.next_type_var,
            );

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
                unify_types(sub_param_ty, arg_ty, &mut self.env.substitutions);
            }
        }
        // Pass 2: check each arg against the resolved slot. For
        // closure args, the resolved slot may be a concrete
        // `Fn(i64) -> i64` (when T solved) and check_expr's pushdown
        // gives the closure params their types.
        for ((arg, sub_param_ty), arg_ty_opt) in
            args.iter().zip(sub_params.iter()).zip(arg_tys.iter())
        {
            let resolved = resolve_type_vars(sub_param_ty, &self.env.substitutions, &id_to_name);
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
            let resolved =
                resolve_type_vars(&Type::TypeVar(id), &self.env.substitutions, &id_to_name);
            if !matches!(&resolved, Type::TypeParam(n) if n == name) {
                solutions.insert(name.clone(), resolved);
            }
        }
        self.record_call_type_subs(record_subs_for_span, &solutions);

        // Resolve the return type. Unsolved metavars come back as
        // `TypeParam(originating_name)` so the caller's
        // `find_unbound_type_param` (slice 2a) still surfaces the
        // unsolved-T diagnostic.
        let ret = resolve_type_vars(&sub_ret, &self.env.substitutions, &id_to_name);
        self.resolve_assoc_projections(&ret)
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
        if self.env.find_from_impl(&src_ty, &target_name).is_some() {
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
        if self.env.find_tryfrom_impl(&src_ty, &target_name).is_some() {
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
            ExprKind::Path(segments) => self.resolve_path_type(segments, &expr.span),

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
                pattern: _,
                value,
                then_block,
                else_branch,
            } => {
                // Type-check the value expression
                self.infer_expr(value);
                // Pattern type-checking would go here when pattern typing is implemented
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

            ExprKind::Break { value, .. } => {
                if let Some(ref e) = value {
                    self.infer_expr(e);
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
                            size: 0,
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
                            size: items.len(),
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
                            size,
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

            ExprKind::Error => Type::Error,
        }
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
        for (enum_name, enum_info) in &self.env.enums {
            for (variant_name, variant_type) in &enum_info.variants {
                if variant_name == name {
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

            // Check for associated function (from impl)
            for imp in &self.env.impls.clone() {
                if imp.target_type == *type_name {
                    if let Some(sig) = imp.methods.get(member) {
                        return Type::Function {
                            params: sig.params.clone(),
                            return_type: Box::new(sig.return_type.clone()),
                        };
                    }
                }
            }

            // Check for ambient resource methods registered as "TypeName.method"
            // in the function table (e.g. "Stdin.read_line", "FileSystem.write").
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
                _ => {
                    self.type_error(
                        format!(
                            "unary '*' requires 'ref T' or 'mut ref T', found '{}'",
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

    fn infer_call(&mut self, callee: &Expr, args: &[CallArg], span: &Span) -> Type {
        // Type-parameter associated calls: `T.method(args)` parses as
        // `Call { callee: Path(["T", "method"]), args }`. Intercept this
        // shape before the generic call infrastructure tries to read `T`
        // as a value. Concrete types (`Wrapper.method()`) fall through —
        // `resolve_path_type` already finds their impl methods.
        if let ExprKind::Path(segments) = &callee.kind {
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

        // Built-in output functions: println() and print()
        // Accept 0 or 1 Display-implementing argument; return Unit.
        if let ExprKind::Identifier(name) = &callee.kind {
            if name == "println" || name == "print" {
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
            ExprKind::Path(segments) => segments.last().and_then(|name| {
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
                self.check_call_args_with_substitution(
                    args,
                    &params,
                    &return_type,
                    span,
                    /* apply_call_site_marker = */ true,
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
            ExprKind::Identifier(_) | ExprKind::Path(_) => {
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
                if self.env.find_from_impl(inner_err, &target_name).is_some() {
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
            || matches!(name, "todo" | "unreachable" | "println" | "print" | "panic")
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
        let mut subs: HashMap<String, Type> = HashMap::new();
        subs.insert("Self".to_string(), Type::TypeParam(target.to_string()));

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
        // shared "Resource.method" function-table entries are found.
        if let ExprKind::Identifier(mod_name) = &object.kind {
            let resource_name = match mod_name.as_str() {
                "env" => Some("Env"),
                _ => None,
            };
            if let Some(resource) = resource_name {
                let dotted = format!("{}.{}", resource, method);
                if let Some(sig) = self.env.functions.get(&dotted).cloned() {
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
                    if let Some(imp) = self.env.find_from_impl(&arg_ty, type_name) {
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
                if let Some(sig) = self.env.find_method(type_name, method).cloned() {
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
        if let Type::Named {
            name,
            args: type_args,
        } = &obj_ty
        {
            if name == "Iterator" || name == "Peekable" {
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
                self.check_assignable(&elem, &arg_ty, args[0].value.span.clone());
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

        // Strip outer `ref` / `mut ref` to get the named receiver per
        // design.md § Method Resolution Step 1 (autoref candidates `T`,
        // `ref T`, `mut ref T` collapse to the same name lookup; the
        // receiver/self-mode compatibility check happens at the
        // param-binding layer). The `shared struct` / `Rc[S]` / `Arc[S]`
        // and refinement-base candidates are deferred — those types have
        // no Type-level representation in v1.
        let receiver_for_lookup = match &obj_ty {
            Type::Ref(inner) | Type::MutRef(inner) => inner.as_ref(),
            other => other,
        };
        let type_name = match receiver_for_lookup {
            Type::Named { name, .. } => name.clone(),
            _ => {
                // For non-named types, just type-check args and return Error
                for arg in args {
                    self.infer_expr(&arg.value);
                }
                return Type::Error;
            }
        };

        // Look up method on the receiver type with inherent-beats-trait
        // priority per design.md § Method Resolution Step 3. Multi-inherent
        // / multi-trait ambiguity detection (Step 4) is deferred.
        let method_sig = self.env.find_method(&type_name, method).cloned();

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
                if is_user_defined {
                    let candidates = self.env.collect_method_names(&type_name);
                    let candidate_refs: Vec<&str> =
                        candidates.iter().map(String::as_str).collect();
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
                } else {
                    self.infer_expr(&args[0].value);
                }
                Type::Str
            }
            // Unknown string method — typo-suggestion diagnostic if close to
            // a known name, silent otherwise (`len`, `contains`, `is_empty`,
            // … are runtime-only and not yet wired through the typechecker).
            // Flip to always-error once enumeration catches up to the
            // interpreter's String surface — design.md § Method Resolution
            // Step 7.
            _ => self.handle_unknown_method(
                "String",
                method,
                &["sorted", "sorted_by"],
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
                for arg in args {
                    self.infer_expr(&arg.value);
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
            _ => self.handle_unknown_method(
                "Slice",
                method,
                &[
                    "binary_search", "chunks", "contains", "fill", "first",
                    "get", "is_empty", "last", "len", "reverse", "sort",
                    "sort_by", "split_at", "swap", "windows",
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
            _ => self.handle_unknown_method(
                "Map",
                method,
                &[
                    "clear", "contains_key", "entries", "entry", "get",
                    "get_or", "insert", "is_empty", "keys", "len", "merge",
                    "remove", "values",
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
            _ => self.handle_unknown_method(
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
            _ => self.handle_unknown_method(
                "SortedSet",
                method,
                &[
                    "contains", "difference", "insert", "intersection",
                    "is_empty", "len", "max", "min", "remove", "union",
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
            _ => self.handle_unknown_method(
                "Set",
                method,
                &[
                    "contains", "difference", "insert", "intersection",
                    "is_empty", "len", "remove", "union",
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
            _ => self.handle_unknown_method(
                "Iterator",
                method,
                &[
                    "all", "any", "chain", "chunk_by", "chunks", "collect",
                    "count", "cycle", "enumerate", "filter", "flat_map",
                    "fold", "inspect", "map", "next", "peek", "peekable",
                    "scan", "skip", "skip_while", "step_by", "take",
                    "take_while", "windows", "zip",
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
            _ => self.handle_unknown_method(
                "Client",
                method,
                &["get", "post"],
                args,
                span,
            ),
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
            _ => self.handle_unknown_method(
                "HttpError",
                method,
                &["message"],
                args,
                span,
            ),
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
                _ => self.handle_unknown_method(
                    "Sender",
                    method,
                    &["clone", "send"],
                    args,
                    span,
                ),
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
                _ => self.handle_unknown_method(
                    "Receiver",
                    method,
                    &["recv", "try_recv"],
                    args,
                    span,
                ),
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
        let obj_ty = self.infer_expr(object);
        if obj_ty == Type::Error {
            return Type::Error;
        }

        let type_name = match &obj_ty {
            Type::Named { name, .. } => name.clone(),
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

        Type::Named {
            name: struct_name,
            args: Vec::new(),
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
        _pattern: &Pattern,
        value: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
        expected: &Type,
        span: &Span,
    ) -> Type {
        self.infer_expr(value);
        let then_ty = self.check_block_against(then_block, expected);
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
        let mut arm_types: Vec<Type> = Vec::new();
        for arm in arms {
            self.local_scope.push();
            self.check_pattern_against(&arm.pattern, &scrut_ty);
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
        let mut arm_types: Vec<Type> = Vec::new();

        for arm in arms {
            self.local_scope.push();
            self.check_pattern_against(&arm.pattern, &scrut_ty);
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

    fn check_pattern_against(&mut self, pattern: &Pattern, expected: &Type) {
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
                self.local_scope.insert(name.clone(), expected.clone());
                // Mirror bind_pattern_types's side-table write so codegen
                // can reconstitute struct payloads for match-arm bindings.
                if let Type::Named {
                    name: type_name, ..
                } = expected
                {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                }
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
                            let subs: HashMap<String, Type> = enum_info
                                .generic_params
                                .iter()
                                .cloned()
                                .zip(args.iter().cloned())
                                .collect();
                            for (pat, ty) in patterns.iter().zip(field_types.iter()) {
                                let resolved = if subs.is_empty() {
                                    ty.clone()
                                } else {
                                    substitute_type_params(ty, &subs)
                                };
                                self.check_pattern_against(pat, &resolved);
                            }
                            return;
                        }
                    }
                }
                // Fallback: bind sub-patterns to Error
                for pat in patterns {
                    self.check_pattern_against(pat, &Type::Error);
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
                            self.check_pattern_against(sub_pattern, &field_ty);
                        } else {
                            // Shorthand: field name becomes binding
                            self.local_scope.insert(field.name.clone(), field_ty);
                        }
                    }
                }
            }
            PatternKind::Tuple(patterns) => {
                if let Type::Tuple(types) = expected {
                    for (pat, ty) in patterns.iter().zip(types.iter()) {
                        self.check_pattern_against(pat, ty);
                    }
                }
            }
            PatternKind::RangePattern { .. } => {
                // Nothing to bind for range patterns
            }
            PatternKind::AtBinding { name, pattern } => {
                self.local_scope.insert(name.clone(), expected.clone());
                self.check_pattern_against(pattern, expected);
            }
            PatternKind::Or(alternatives) => {
                for alt in alternatives {
                    self.check_pattern_against(alt, expected);
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

    fn bind_pattern_types(&mut self, pattern: &Pattern, ty: &Type) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                self.local_scope.insert(name.clone(), ty.clone());
                // Record the surface type for codegen so it can reconstitute
                // struct payloads from the i64 word at match-arm bind sites
                // (see TypeCheckResult.pattern_binding_types). Only Named
                // types need this — primitives and references don't.
                if let Type::Named {
                    name: type_name, ..
                } = ty
                {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                }
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
        subs.insert("T".to_string(), Type::Bool);
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
