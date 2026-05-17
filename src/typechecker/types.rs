//! Leaf type definitions for the typechecker.
//!
//! Holds the `Type` enum and its primitive-size sub-enums
//! (`IntSize`, `UIntSize`, `FloatSize`), the `TypeVarId` / `ConstVarId`
//! metavariable ids, the `ConstArg` / `SubstValue` payloads consumed by
//! the inference solver, the `ScrutineeMode` match-arm classifier, and
//! the `VariantTypeInfo` descriptor used by exhaustiveness and ownership.
//!
//! Pure type predicates and display helpers will land here in
//! subsequent extractions; for now they remain alongside the
//! `TypeChecker` impl in `super`.

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
    /// parameter name (e.g. `"I"`); after `substitute_type_params` has run
    /// it carries the resolved receiver's bare type name (e.g. `"Wrapper"`
    /// for a `Wrapper[String]` receiver). `assoc` is the associated type
    /// name (e.g. `"Item"`). `args` carries the projection's own type
    /// arguments for a generic associated type (GAT) like `F.Mapped[i64]`
    /// (slice 4 of the GAT epic) — empty for the non-generic form `F.Item`.
    /// `receiver_args` carries the resolved receiver's own type arguments
    /// (e.g. `[String]` for `Wrapper[String]`) once `substitute_type_params`
    /// has solved the receiver — empty pre-substitution and for receivers
    /// that aren't generic (slice 5 addition). The split between `param`
    /// (base name) and `receiver_args` (the receiver's args) is what lets
    /// `resolve_assoc_projections` look up the impl entry by bare name AND
    /// substitute the impl-block's generic params, while the projection's
    /// own `args` substitute the GAT params. Slice 5 wires the two-sided
    /// substitution.
    AssocProjection {
        param: String,
        assoc: String,
        args: Vec<Type>,
        receiver_args: Vec<Type>,
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

// ── Type Operations ─────────────────────────────────────────────

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
/// Returns `None` for type variables, function types, slices, tuples,
/// etc. — none of which can satisfy a nominal trait bound today. Strips
/// outer `ref` / `mut ref` so a borrowed receiver discharges against
/// the same impls as its inner type.
pub(super) fn impl_table_key(ty: &Type) -> Option<(String, Vec<Type>)> {
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
pub(super) fn impl_args_match(stored: &[Type], call_site: &[Type]) -> bool {
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
pub(super) fn receiver_for_method_lookup(obj_ty: &Type) -> Type {
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
pub(super) fn type_is_fully_concrete(ty: &Type) -> bool {
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
        Type::AssocProjection {
            param,
            assoc,
            args,
            receiver_args,
        } => {
            let recv_str = if receiver_args.is_empty() {
                param.clone()
            } else {
                let inner: Vec<String> = receiver_args.iter().map(type_display).collect();
                format!("{}<{}>", param, inner.join(", "))
            };
            if args.is_empty() {
                format!("{}.{}", recv_str, assoc)
            } else {
                let inner: Vec<String> = args.iter().map(type_display).collect();
                format!("{}.{}[{}]", recv_str, assoc, inner.join(", "))
            }
        }
        Type::Error => "<error>".to_string(),
    }
}

// ── Type Compatibility ──────────────────────────────────────────

/// True iff `name` is a primitive / prelude type or stdlib module name
/// reachable at scope-0 — used by `resolve_identifier_type`'s variant
/// fallback to skip name-shadow cases like `Json.String(String)` where
/// the variant name collides with the primitive type name. See the
/// comment block at the variant-fallback site.
pub(super) fn is_prelude_type_or_module_name(name: &str) -> bool {
    crate::prelude::PRELUDE_PRIMITIVES.contains(&name)
        || crate::prelude::PRELUDE_TYPES.contains(&name)
}

pub(super) fn is_numeric(ty: &Type) -> bool {
    matches!(ty, Type::Int(_) | Type::UInt(_) | Type::Float(_))
}

pub(super) fn is_integer(ty: &Type) -> bool {
    matches!(ty, Type::Int(_) | Type::UInt(_))
}

/// Width of an integer type in bits, for the char→int narrowing check.
/// `usize` / `isize` are conservatively treated as 32-bit so a 32-bit
/// target rejects `char as usize`; on 64-bit targets the cast is still
/// allowed via the wider-int path. The actual address-width of `usize`
/// is platform-dependent and folded in at codegen.
pub(super) fn integer_width_bits(ty: &Type) -> Option<u32> {
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
pub(super) fn method_callee_type_name(ty: &Type) -> Option<String> {
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
pub(super) fn iterator_item_type_for(ty: &Type) -> Option<Type> {
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
pub(super) fn clone_self_type_for(ty: &Type) -> Option<Type> {
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
pub(super) fn contains_type_param(ty: &Type) -> bool {
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
pub(super) fn is_subtype(super_ty: &Type, sub_ty: &Type) -> bool {
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
pub(super) fn lub_block_type(tail: Type, breaks: &[Type]) -> Type {
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

pub(super) fn types_compatible(a: &Type, b: &Type) -> bool {
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

// ── Local Type Scope ────────────────────────────────────────────
