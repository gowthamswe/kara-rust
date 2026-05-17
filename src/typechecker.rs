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
use crate::resolver::{ResolveResult, SpanKey};
use crate::token::{FloatSuffix, IntSuffix, Span};
use std::collections::{HashMap, HashSet};

mod bounds;
mod closures;
mod const_eval;
mod derives;
pub mod env;
mod env_build;
mod expr_call;
mod expr_method_call;
mod expr_ops;
mod exprs;
mod fields;
mod inference;
mod items;
mod lowering;
mod patterns;
mod stdlib_io;
mod stdlib_iter;
mod stdlib_map;
mod stdlib_methods;
mod stdlib_seq;
#[cfg(test)]
mod tests;
pub mod types;

pub use const_eval::ConstEvalError;
use const_eval::{binop_glyph, const_value_type, format_const_value, unaryop_glyph};
pub use env::{EnumInfo, FunctionSig, ImplInfo, StructInfo, TraitInfo, TypeEnv};
#[cfg(test)]
use inference::substitute_type_params;
pub use types::{
    const_arg_display, type_display, type_to_concrete_or_param_name, ConstArg, ConstVarId,
    FloatSize, IntSize, SubstValue, Type, TypeVarId, UIntSize, VariantTypeInfo,
};
#[cfg(test)]
use types::{contains_type_param, receiver_for_method_lookup};
use types::{
    integer_width_bits, is_integer, is_numeric, is_subtype, projection_unresolvable_with,
    types_compatible,
};

// ── Attribute Helpers ───────────────────────────────────────────

/// Extract trait names from `#[derive(Eq, Hash, ...)]` attributes.
/// Also handles call-form args like `Display(snake_case)` — the trait name
/// (`"Display"`) is inserted regardless of arguments.
pub(super) fn extract_derived_traits(attributes: &[Attribute]) -> HashSet<String> {
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

/// Extract the `#[must_use]` message from a declaration's attribute list
/// (slice 4 of the `#[must_use]` mandate — see
/// `docs/implementation_checklist/phase-5-diagnostics.md` § `#[must_use]`
/// mandate, slice 4). Returns:
///
/// - `Some(message)` when `#[must_use = "msg"]` is present (string-value
///   form); `message` is the author's reason string.
/// - `Some(String::new())` when bare `#[must_use]` is present (no
///   reason supplied — the discard-site walker falls back to a generic
///   "value should not be discarded" message).
/// - `None` when the attribute is absent.
///
/// The empty-string-vs-None distinction is what lets the walker
/// distinguish "author marked must-use but gave no message" from "not
/// must-use at all". Mirrors the shape of `extract_derived_traits`:
/// pure attribute reading, no diagnostics emitted, no env mutation.
pub(super) fn extract_must_use_message(attributes: &[Attribute]) -> Option<String> {
    attributes
        .iter()
        .find(|a| a.name == "must_use")
        .map(|a| a.string_value.clone().unwrap_or_default())
}

/// Returns `true` when `attributes` contains `#[derive(Display(snake_case))]`.
pub(super) fn has_display_snake_case(attributes: &[Attribute]) -> bool {
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
pub(super) fn normalize_bounds_into_where_clause(
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

/// Mode for `closure_consumes_captured_non_copy`'s body walk: tracks
/// whether the current position is a Reading or Consuming context.
/// Mirrors `use_classifier::Mode` so the typechecker's capture-consume
/// detection lines up with the legacy ownership-side detector. Round
/// 12.44 (Step 2 — once-callability inference at construction).
#[derive(Copy, Clone, Eq, PartialEq)]
pub(super) enum CaptureWalkMode {
    Reading,
    Consuming,
}

pub(super) struct LocalTypeScope {
    pub(super) scopes: Vec<HashMap<String, Type>>,
}

impl LocalTypeScope {
    pub(super) fn new() -> Self {
        LocalTypeScope {
            scopes: vec![HashMap::new()],
        }
    }

    pub(super) fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(super) fn pop(&mut self) {
        self.scopes.pop();
    }

    pub(super) fn insert(&mut self, name: String, ty: Type) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, ty);
        }
    }

    pub(super) fn lookup(&self, name: &str) -> Option<&Type> {
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
    /// Lint name when this entry is a warning emitted via the lint
    /// machinery (e.g., `UnreachableArm` → `Some("unreachable_arm")`).
    /// `None` on hard errors and on warnings that don't yet route
    /// through a registered lint. Surfaced into the structured JSON
    /// diagnostic so `karac --output=json` consumers can filter,
    /// group, and route by lint name. Slice 7 of the lint-level
    /// entry — see `phase-5-diagnostics.md` § "Lint level attributes".
    pub lint_name: Option<String>,
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
    /// `#[non_exhaustive]` slice 4 — a cross-package consumer wrote a
    /// struct literal `Foo { ... }` (or an exhaustive struct pattern
    /// without `..`) against a `pub struct` defined in another package
    /// that carries the `#[non_exhaustive]` attribute. The defining
    /// package can add fields without breaking source compatibility,
    /// so consumers must construct via a public constructor (commonly
    /// `Foo.new(...)`) and pattern-match with `..` rest-binding.
    /// Same-package construction is unaffected — the diagnostic only
    /// fires at the cross-package boundary. See design.md
    /// § `#[non_exhaustive]` for Evolvable Public Types.
    NonExhaustiveCrossPackageLiteral,
    /// `#[non_exhaustive]` slice 5 — a cross-package consumer wrote a
    /// `match` on a `#[non_exhaustive]` enum without a wildcard arm.
    /// New variants may land without breaking source compatibility, so
    /// outside-package consumers cannot enumerate the current variant
    /// set exhaustively. Distinct from `NonExhaustiveMatch` (the
    /// strict variant-by-variant rule) because the wildcard requirement
    /// here is unconditional regardless of which variants are covered;
    /// same-package matches keep the strict rule.
    NonExhaustiveCrossPackageMatch,
    /// `#[non_exhaustive]` slice 4 pattern half — a cross-package
    /// consumer destructured a `#[non_exhaustive] pub struct` without
    /// a `..` rest-pattern. The defining package may add fields without
    /// breaking source compatibility, so the destructure must allow
    /// for them. Fix-it: insert `..` before the closing brace. Mirrors
    /// the literal-half rule (`NonExhaustiveCrossPackageLiteral`),
    /// applied at the pattern check site.
    NonExhaustiveCrossPackagePattern,
    /// Lint-level slice 4b follow-up — a `#[allow]` / `#[warn]` /
    /// `#[deny]` / `#[expect]` attribute named a lint that is not in
    /// the central registry (`crate::lints::STARTER_LINTS`). Routed
    /// through `type_lint_warning` with `lint_name = "unknown_lint"`
    /// so the same cascade walker that handles every other lint can
    /// suppress (`#[allow(unknown_lint, removed_lint)]`) or promote
    /// (`#[deny(unknown_lint)]`) it uniformly. Per design.md
    /// § Lint Level Attributes > Naming, *"code with
    /// `#[allow(removed_lint)]` continues to compile"* — the
    /// unknown name itself surfaces as a (suppressible) warning.
    UnknownLint,
    /// `#[deprecated]` slice 4 — a reference resolved to a symbol that
    /// carries a `Deprecation` payload (recorded in the resolver's
    /// symbol table by slice 3b). Routed through `type_lint_warning`
    /// with the `deprecated` lint name; the cascade walker (slice 4b)
    /// decides whether to suppress / warn / promote-to-error based on
    /// enclosing `#[allow(deprecated)]` / `#[warn(deprecated)]` /
    /// `#[deny(deprecated)]` / `#[expect(deprecated)]` attributes. The
    /// message surfaces the optional `note` / `since` fields when set.
    Deprecated,
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

/// Capture set for a single return-position `impl Trait` declaration —
/// slice 4 of the `impl Trait` epic. See [`TypeCheckResult::impl_trait_captures`]
/// for how the map is keyed and consumed.
///
/// **Type-parameter captures.** A type parameter name appears here when
/// it textually shows up in the return-type expression's trait args.
/// `fn make_iter[T](xs: Vec[T]) -> impl Iterator[Item = T]` captures `T`;
/// `fn count[T](xs: Vec[T]) -> impl Iterator[Item = i64]` does not.
///
/// **Input-borrow captures.** A `ref X` / `mut ref X` parameter name
/// appears here when the return-type expression contains a `ref` whose
/// elided source flows from that input. Today's elision is a simple
/// shape match: any `Ref`/`MutRef` occurrence inside the existential's
/// trait args triggers the capture of every `ref`-typed input parameter
/// (the design.md "single-ref-input → elide to that input" rule with the
/// conservative multi-input over-approximation). Bare-owned and shared
/// parameters never appear in this list — only their borrow regions are
/// at stake, and they don't have one.
#[derive(Debug, Clone, Default)]
pub struct ImplTraitCaptures {
    pub type_params: Vec<String>,
    pub input_borrows: Vec<String>,
}

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
    /// `impl Trait` slice 4 — per-existential capture set, keyed by the
    /// `SpanKey` of the `TypeKind::ImplTrait` AST node (same key shape
    /// used by [`Type::Existential::origin`]). For each return-position
    /// existential the entry records (a) the captured type-parameter
    /// names that appear in the return-type expression and (b) the names
    /// of `ref`/`mut ref` input parameters whose borrow flows into a
    /// `ref` in the return-type expression per design.md's elision rule.
    /// The ownership checker consumes this map to bound the existential's
    /// lifetime at call sites — a drop of any captured input while the
    /// returned existential is still bound fires the existing
    /// drop-of-borrowed diagnostic.
    pub impl_trait_captures: HashMap<SpanKey, ImplTraitCaptures>,
    /// MethodCall span → inner `TypeExpr` for `Option[T].unwrap`/`expect`
    /// and `Result[T, E].unwrap`/`expect` receivers. Populated by
    /// `infer_method_call` when the receiver type is `Option`/`Result` and
    /// the method is one of `unwrap`, `expect`, `is_some`, `is_none`,
    /// `is_ok`, `is_err` (the `is_*` arms record T for uniformity even
    /// though codegen only consumes the tag). Codegen consults this map
    /// to know the LLVM shape of the value to reconstitute from the
    /// Option/Result payload words. Keyed by MethodCall span (same key
    /// shape as `method_callee_types`); the receiver-span collision noted
    /// there does not apply here because we record the inner *element*
    /// type, not the receiver's whole type.
    pub method_unwrap_inner_types: HashMap<SpanKey, TypeExpr>,
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
    /// `#[must_use]` annotations on free functions and impl methods
    /// (slice 4 of the `#[must_use]` mandate — see
    /// `docs/implementation_checklist/phase-5-diagnostics.md`). Snapshot
    /// of `TypeEnv.must_use_functions` at end-of-typecheck. Keyed by
    /// `"name"` for free functions and `"TargetType.method"` for impl
    /// methods (matching the canonical shape produced by
    /// `method_callee_types` / `bare_assoc_fn_targets`). Consumed by
    /// the discard-site walker in `src/must_use_lint.rs`.
    pub must_use_functions: HashMap<String, Option<String>>,
}

// ── Cross-module visibility helpers (CR-24 slice 6) ─────────────

/// Return the declared `Visibility` of a top-level item named `name` inside
/// `module`. Returns `None` when the item does not exist or is not a kind
/// that carries top-level visibility (impl blocks, layouts, etc).
pub(super) fn find_item_visibility(
    module: &crate::module::Module,
    name: &str,
) -> Option<Visibility> {
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
pub(super) fn find_struct_def<'m>(
    module: &'m crate::module::Module,
    name: &str,
) -> Option<&'m StructDef> {
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
    pub(super) program: &'a Program,
    pub(super) resolve_result: &'a ResolveResult,
    /// Optional project-wide tree for cross-module checks (CR-24 slice 6b):
    /// extends `E0221 PrivateTypeInPublicSignature` to imported types and
    /// turns on field-access rejection for cross-module struct fields.
    pub(super) tree: Option<&'a crate::module::ProgramTree>,
    /// The id of the module being typechecked, when `tree` is set. Used to
    /// scope cross-module visibility checks — an access is "cross-module"
    /// when the accessed item's origin differs from `current_module`.
    pub(super) current_module: Option<crate::module::ModuleId>,
    /// Local name → (canonical origin module path, canonical item name,
    /// declared visibility) for items imported into the current module from
    /// elsewhere in the tree. Slice 7: re-exports collapse to the canonical
    /// entry — `import M.X` where M re-exports `a.b.X` records
    /// `("X" → (["a","b"], "X", ...))`, and an alias `import M.Y as Z` maps
    /// `"Z" → (["a","b"], "Y", ...)`. Populated during `build_type_env` when
    /// `tree` is set.
    pub(super) type_origins: HashMap<String, (Vec<String>, String, Visibility)>,
    pub(super) env: TypeEnv,
    pub(super) local_scope: LocalTypeScope,
    pub(super) errors: Vec<TypeError>,
    pub(super) warnings: Vec<TypeError>,
    pub(super) expr_types: HashMap<SpanKey, Type>,
    pub(super) current_return_type: Option<Type>,
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
    pub(super) break_value_types: Vec<(String, Vec<Type>)>,
    pub(super) current_self_type: Option<Type>,
    /// True when type-checking inside a defer/errdefer block.
    pub(super) in_defer: bool,
    /// `?` cross-error From conversions (span → target error type name).
    pub(super) question_conversions: HashMap<SpanKey, String>,
    /// `x.into()` conversions (span of the MethodCall → target type name).
    pub(super) into_conversions: HashMap<SpanKey, String>,
    /// `x.try_into()` conversions (span of the MethodCall → target type name,
    /// where target is the `T` extracted from `Result[T, E]`).
    pub(super) try_into_conversions: HashMap<SpanKey, String>,
    /// Enum names that derive `Display(snake_case)`. Populated during
    /// `env_add_enum`; transferred to `TypeCheckResult`.
    pub(super) display_snake_case_enums: HashSet<String>,
    /// MethodCall span → `Type.method` canonical callee key. See the
    /// matching field on `TypeCheckResult` for the full rationale.
    pub(super) method_callee_types: HashMap<SpanKey, String>,
    /// Per-existential capture sets, keyed by the SpanKey of the
    /// `TypeKind::ImplTrait` AST node. See the public copy on
    /// `TypeCheckResult` for the full rationale.
    pub(super) impl_trait_captures: HashMap<SpanKey, ImplTraitCaptures>,
    /// MethodCall span → inner `TypeExpr` for `Option`/`Result` unwrap
    /// dispatch. See the public copy on `TypeCheckResult` for the full
    /// rationale.
    pub(super) method_unwrap_inner_types: HashMap<SpanKey, TypeExpr>,
    /// Bare-call expected-type dispatch resolutions: call-expression span →
    /// resolved target type name (e.g. `"Wrapper"`). Populated when
    /// `try_apply_expected_assoc_fn_inference` resolves a bare `name(args)`
    /// call against a concrete expected type. The lowering pass rewrites
    /// these to `Target.name(args)` so the interpreter / codegen can dispatch
    /// through the existing `Type.method` impl table.
    pub(super) bare_assoc_fn_targets: HashMap<SpanKey, String>,
    /// Per-call-site type substitutions: call-expression span → name → resolved
    /// type name (concrete struct/enum, or another generic param if the caller
    /// is itself generic and propagates the binding). Populated by `infer_call`
    /// after solving and by `check_expr`'s expected-type-driven pass for
    /// zero-arg generic calls. Consumed by the interpreter at each call: it
    /// pushes the resolved frame so `T.method()` and bare-method calls inside
    /// the callee's body can look up `T`'s concrete binding.
    pub(super) call_type_subs: HashMap<SpanKey, HashMap<String, String>>,
    /// Pattern-binding name → canonical type name. See the public copy on
    /// `TypeCheckResult` for the consumer doc.
    pub(super) pattern_binding_types: HashMap<SpanKey, String>,
    /// Pattern-binding span → inner element `TypeExpr` for `Vec[T]` / `Slice[T]`
    /// bindings. Sibling to `pattern_binding_types`. See the public copy on
    /// `TypeCheckResult` for the full rationale (PB sibling slice 2026-05-09).
    pub(super) pattern_binding_inner_types: HashMap<SpanKey, TypeExpr>,
    /// Internal mirror of the public table; written by `check_pattern_against`
    /// at every leaf-binding site (and at struct shorthand fields) when
    /// the scrutinee mode is non-Owned. Surfaced in `check()`.
    pub(super) pattern_binding_borrow_modes: HashMap<SpanKey, crate::ast::PatternBindingBorrow>,
    /// Parallel to `pattern_binding_inner_types`, storing the raw `Type`
    /// (which may contain unresolved `Type::TypeVar`) captured at the
    /// recording site. After body inference completes, `finalize_pattern_
    /// binding_inner_types` walks this map, resolves typevars against
    /// `env.substitutions`, and overwrites `pattern_binding_inner_types`
    /// with the substituted `TypeExpr`. Without this, `let mut q =
    /// VecDeque.new(); q.push_back(x);` writes the inner-type entry at
    /// the let site (where `?T0` is still unsolved), and the resulting
    /// `TypeKind::Error` strands codegen with the wrong element type.
    pub(super) pattern_binding_inner_unresolved: HashMap<SpanKey, Type>,
    /// Trait bounds for the generic parameters in the current enclosing scope
    /// (impl-level + function/method-level). Indexed by the param's textual
    /// name so it pairs naturally with `Type::TypeParam(name)`. Populated on
    /// entering a generic-bearing scope and saved/restored on exit, mirroring
    /// the enclosing-generic-name list threaded through the lower / check
    /// path. Used to resolve bare `method(args)` calls at expected-type
    /// positions when the expected type is a generic param.
    pub(super) enclosing_bounds: HashMap<String, Vec<crate::ast::TraitBound>>,
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
    pub(super) enclosing_trait: Option<String>,
    /// Closure expression span → reason that closure became once-callable.
    /// Populated by `closure_type_with_capture_inference` when the body walk
    /// finds a captured-non-Copy consume; consumed by `check_assignable` so
    /// `E_ONCE_FN_INTO_FN_SLOT` can name the consumed binding when a closure
    /// literal is rejected at a `Fn` slot. Round 12.45 (Step 3).
    pub(super) closure_once_reasons: HashMap<SpanKey, OnceReason>,
    /// `stdlib_origin` of the function whose body we're currently
    /// checking. Saved/restored across `check_function` so nested
    /// item-checks (e.g. impl method bodies inside a stdlib-origin
    /// impl block) see the right value. Consumed by
    /// `#[non_exhaustive]` slice 4's cross-package check at struct
    /// literal sites — a stdlib-defined `#[non_exhaustive]` struct
    /// constructed from a user-origin fn body fires
    /// `E_NON_EXHAUSTIVE_CROSS_PACKAGE_LITERAL`. Defaults to `false`
    /// (the safer assumption — fires the check uniformly when we're
    /// outside any function context, e.g. at module-const init exprs).
    pub(super) current_fn_stdlib_origin: bool,
    /// Stack of `lint_overrides` frames for the items currently being
    /// type-checked (slice 4b of the lint-level entry). Frame
    /// pushed at every item-walk entry (`check_function`,
    /// `check_impl_block`, `check_trait_def`); popped on exit. The
    /// cascade reader [`Self::effective_lint_level`] walks the stack
    /// innermost-first and returns the first matching override's
    /// level, falling through to the lint's registered default. The
    /// emission site [`Self::type_lint_warning`] consults the
    /// effective level: `Allow` → skip, `Warn` → push to `warnings`,
    /// `Deny` → push to `errors`, `Expect` → push to `warnings`
    /// (fulfilment tracking lands in slice 5).
    pub(super) lint_override_stack: Vec<Vec<crate::lints::LintLevelOverride>>,
}

/// Why a closure is `OnceFunction`-typed: which captured outer binding the
/// body consumed, and where in the body the consume happened. Populated by
/// the once-callability walker when it flips its first identifier-leaf in
/// `Consuming` mode that resolves to an outer non-Copy binding.
#[derive(Debug, Clone)]
pub(super) struct OnceReason {
    /// The outer binding name (or `"self"`) that the closure body consumed.
    pub(super) consumed_binding: String,
    /// The body span where the consume occurred (the identifier-leaf, not
    /// the enclosing call). Used for diagnostics; not currently surfaced in
    /// the rejection message but kept for future polish in Step 5.
    #[allow(dead_code)]
    pub(super) consumed_span: Span,
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
            impl_trait_captures: HashMap::new(),
            method_unwrap_inner_types: HashMap::new(),
            bare_assoc_fn_targets: HashMap::new(),
            call_type_subs: HashMap::new(),
            pattern_binding_types: HashMap::new(),
            pattern_binding_inner_types: HashMap::new(),
            pattern_binding_borrow_modes: HashMap::new(),
            pattern_binding_inner_unresolved: HashMap::new(),
            enclosing_bounds: HashMap::new(),
            enclosing_trait: None,
            closure_once_reasons: HashMap::new(),
            current_fn_stdlib_origin: false,
            lint_override_stack: Vec::new(),
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
        // Lint-level slice 4b follow-up — synthesize `unknown_lint`
        // warnings for every override naming a lint not in the
        // central registry. Runs as a pre-pass so the per-item
        // cascade-aware emission path is already wired (see
        // `emit_unknown_lint_warnings`).
        self.emit_unknown_lint_warnings();
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
        let must_use_functions = self.env.must_use_functions.clone();
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
            impl_trait_captures: self.impl_trait_captures,
            method_unwrap_inner_types: self.method_unwrap_inner_types,
            bare_assoc_fn_targets: self.bare_assoc_fn_targets,
            call_type_subs: self.call_type_subs,
            pattern_binding_types: self.pattern_binding_types,
            pattern_binding_inner_types: self.pattern_binding_inner_types,
            pattern_binding_borrow_modes: self.pattern_binding_borrow_modes,
            compiler_builtins,
            must_use_functions,
        }
    }

    /// Walk every top-level item's `lint_overrides` (and impl-block
    /// methods' overrides) and emit `unknown_lint` warnings for any
    /// lint name not in the central registry. Each emission pushes
    /// the originating item's frame (and the surrounding impl-block
    /// frame for methods) onto `lint_override_stack` first, so the
    /// generic `type_lint_warning` cascade lookup sees the item's
    /// own overrides as the innermost scope — that's how
    /// `#[allow(unknown_lint, removed_lint)]` self-suppresses (the
    /// synthesized warning's span lives inside the attribute slot,
    /// which sits outside the item's body-span, so a span-based
    /// cascade walker would otherwise fail to find the item).
    ///
    /// Slice 4b follow-up — slice 4b core left this on the
    /// deferred-polish list; this method ships it.
    fn emit_unknown_lint_warnings(&mut self) {
        // Collect (outer_frame, inner_frame) pairs to emit against.
        // `outer_frame` is the surrounding impl block's overrides for
        // an impl-method; empty otherwise. `inner_frame` is the item's
        // own overrides, which become the innermost cascade frame at
        // emission time (so `#[allow(unknown_lint, ...)]` on the same
        // item self-suppresses).
        let mut emissions: Vec<(
            Vec<crate::lints::LintLevelOverride>,
            Vec<crate::lints::LintLevelOverride>,
        )> = Vec::new();
        for item in &self.program.items {
            if let Some(overs) = item_own_lint_overrides(item) {
                if has_unknown_lint(overs) {
                    emissions.push((Vec::new(), overs.to_vec()));
                }
            }
            if let Item::ImplBlock(imp) = item {
                for impl_item in &imp.items {
                    if let ImplItem::Method(f) = impl_item {
                        if has_unknown_lint(&f.lint_overrides) {
                            emissions.push((imp.lint_overrides.clone(), f.lint_overrides.clone()));
                        }
                    }
                }
            }
        }
        for (outer, inner) in emissions {
            // Push the cascade frames (impl block first, then item /
            // method) so the innermost frame is the item's own
            // overrides and `#[allow(unknown_lint)]` self-suppresses.
            let pushed_outer = !outer.is_empty();
            if pushed_outer {
                self.lint_override_stack.push(outer);
            }
            self.lint_override_stack.push(inner.clone());
            for ov in &inner {
                if crate::lints::lint_by_name(&ov.lint).is_none() {
                    let message = format!(
                        "error[E_UNKNOWN_LINT]: unknown lint name `{}` in lint-level attribute; \
                         remove the attribute or check the lint registry",
                        ov.lint,
                    );
                    self.type_lint_warning(
                        message,
                        ov.span.clone(),
                        TypeErrorKind::UnknownLint,
                        "unknown_lint",
                    );
                }
            }
            self.lint_override_stack.pop();
            if pushed_outer {
                self.lint_override_stack.pop();
            }
        }
    }

    pub(super) fn type_error(&mut self, message: String, span: Span, kind: TypeErrorKind) {
        self.errors.push(TypeError {
            message,
            span,
            kind,
            lint_name: None,
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

    /// Push a structured warning carrying the registered lint name so
    /// downstream JSON consumers can route by lint (slice 7 of the
    /// lint-level entry). All typechecker warnings flow through this
    /// helper — bare lint-less warnings are rejected by the spec
    /// ("every warning emitted by the compiler must record the lint
    /// name in the structured diagnostic"). Caller names a lint that
    /// exists in `crate::lints::STARTER_LINTS`; an unknown name routes
    /// through the cascade (slice 4b) with the default `Warn` level.
    ///
    /// **Slice 4b cascade integration.** Consults
    /// [`Self::effective_lint_level`] before deciding where to route
    /// the entry: `Allow` → suppressed silently; `Warn` / `Expect` →
    /// pushed to `warnings`; `Deny` → pushed to `errors` (the message
    /// is reused unchanged — the cascade only changes the severity).
    pub(super) fn type_lint_warning(
        &mut self,
        message: String,
        span: Span,
        kind: TypeErrorKind,
        lint_name: &'static str,
    ) {
        use crate::lints::LintLevel;
        let level = self.effective_lint_level(lint_name);
        let entry = TypeError {
            message,
            span,
            kind,
            lint_name: Some(lint_name.to_string()),
        };
        match level {
            LintLevel::Allow => {
                // Suppressed by an `#[allow(...)]` in the enclosing
                // scope chain (or by CLI `-A NAME` once that lands).
            }
            LintLevel::Warn => {
                self.warnings.push(entry);
            }
            LintLevel::Expect => {
                // `#[expect(NAME)]` on a scope whose lint fires is
                // silent per design.md § Lint Level Attributes (the
                // user told the compiler they expect the lint to
                // fire, so its firing is acknowledged not surfaced).
                // Slice 5 will additionally track fulfilment and
                // emit `unfulfilled_lint_expectation` when the lint
                // did NOT fire in the attributed scope.
            }
            LintLevel::Deny => {
                self.errors.push(entry);
            }
        }
    }

    /// Walk the `lint_override_stack` innermost-first looking for a
    /// matching override on `lint_name`. Returns the override's level
    /// when found, falling through to the lint's registered default
    /// (`Warn` when the name is unknown to the registry — matching the
    /// design.md "Naming" rule of "unknown lint names continue to
    /// compile"). Slice 4b cascade reader. CLI build-wide defaults
    /// (`-A NAME` / `-W NAME` / `-D NAME` / `-F NAME`) are a follow-up.
    pub(super) fn effective_lint_level(&self, lint_name: &str) -> crate::lints::LintLevel {
        for frame in self.lint_override_stack.iter().rev() {
            for ov in frame.iter().rev() {
                if ov.lint == lint_name {
                    return ov.level;
                }
            }
        }
        crate::lints::lint_by_name(lint_name)
            .map(|info| info.default_level)
            .unwrap_or(crate::lints::LintLevel::Warn)
    }

    /// `#[deprecated]` slice 4 — at a reference site, check whether
    /// the resolved name is a deprecated symbol and emit the
    /// `deprecated` lint warning through `type_lint_warning` if so.
    /// Routes via the slice-4b cascade so `#[allow(deprecated)]` on
    /// the enclosing scope suppresses, `#[deny(deprecated)]` promotes
    /// to error, etc.
    ///
    /// The resolver's symbol table (extended by slice 3b) carries a
    /// `Deprecation` payload keyed by `SymbolId`; the resolver's
    /// resolution map (`SpanKey → SymbolId`) connects the reference's
    /// span to its target. When the reference's span is not in the
    /// resolutions map (e.g., the typechecker resolved the name via
    /// its own env rather than the resolver, as it does for free
    /// functions and constants), `display_name` is used to look up the
    /// symbol by name in the global scope as a fallback.
    pub(super) fn check_deprecated_use_at(&mut self, span: &Span, display_name: &str) {
        use crate::resolver::ScopeId;
        let span_key = SpanKey::from_span(span);
        // First try the per-span resolution map (the precise path).
        let sym_id = self
            .resolve_result
            .resolutions
            .get(&span_key)
            .copied()
            .or_else(|| {
                // Fallback: look up by name in global scope. The
                // typechecker often resolves free fns / consts /
                // variants by env-lookup rather than threading the
                // resolver's SpanKey — global-scope name lookup is
                // the next-best signal.
                self.resolve_result
                    .symbol_table
                    .lookup_in_scope(ScopeId(0), display_name)
                    .map(|s| s.id)
            });
        let Some(sym_id) = sym_id else { return };
        let Some(dep) = self.resolve_result.symbol_table.deprecation_for(sym_id) else {
            return;
        };
        let mut message = format!("warning[deprecated]: use of deprecated item `{display_name}`");
        if let Some(ref note) = dep.note {
            message.push_str(&format!(": {note}"));
        }
        if let Some(ref since) = dep.since {
            message.push_str(&format!(" (since {since})"));
        }
        message.push_str(" — suppress with `#[allow(deprecated)]` on the enclosing item");
        self.type_lint_warning(
            message,
            span.clone(),
            TypeErrorKind::Deprecated,
            "deprecated",
        );
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

    /// Checker-aware projection-resolving wrapper around the pure
    /// `types_compatible`. GAT slice 8c carry-forward (d).
    ///
    /// The pure `types_compatible` in `types.rs` is structural-only on
    /// `AssocProjection`: two projections must match component-wise, and
    /// a one-sided projection vs concrete type returns `false`. That
    /// strictness needed projection-aware resolution at the
    /// `if`/`if-let`/`match`/`range` branch-compat checks and at
    /// `check_assignable` that today see substituted-but-unresolved
    /// projections (`F.Mapped[i64]` against `Vec[i64]` before the
    /// impl-table lookup). This wrapper resolves both sides through
    /// `resolve_assoc_projections` (the same engine
    /// `discharge_projection_bounds` uses) before delegating to the
    /// pure structural check — so a projection that resolves to a
    /// concrete `Vec[i64]` matches a slot of `Vec[i64]`.
    ///
    /// **Conservative fallback for unresolvable projections.** If a
    /// projection survives resolution (receiver still a `TypeParam`,
    /// no impl-table entry), the wrapper falls back to permissive
    /// (returns `true`). This preserves the pre-slice-8c "we don't
    /// know what this will become at monomorphization time, so don't
    /// fire a false-positive diagnostic" semantics for in-body checks
    /// inside a generic function — at the call site the projection
    /// resolves through `solutions` and the strict structural check
    /// catches a genuine mismatch. The slice 8c tightening targets
    /// the **resolved** path; the unresolved path remains permissive
    /// (which is observationally equivalent to the pre-slice-8c
    /// wildcard arm for that subset of inputs).
    ///
    /// Pure-context callers (`unify_types`, `lub_block_type`, the
    /// slice/array element-compat sub-calls inside `types_compatible`
    /// itself) keep using the bare `types_compatible` — those sites
    /// don't have `&self` and the pure structural rule is the right
    /// semantics for them.
    pub(super) fn types_compatible_with_projections(&self, a: &Type, b: &Type) -> bool {
        let a_resolved = self.resolve_assoc_projections(a);
        let b_resolved = self.resolve_assoc_projections(b);
        if projection_unresolvable_with(&a_resolved, &b_resolved) {
            return true;
        }
        types_compatible(&a_resolved, &b_resolved)
    }

    /// Subtyping counterpart to `types_compatible_with_projections`.
    /// Routes through `is_subtype` (which carries function-type
    /// variance + owned-to-ref coercion) after resolving projections
    /// on both sides. Used by `check_assignable` so an expected slot
    /// of `Vec[i64]` accepts an inferred `F.Mapped[i64]` that
    /// resolves to `Vec[i64]` through the impl table. Falls back to
    /// permissive when the projection remains unresolvable (see the
    /// rationale on `types_compatible_with_projections`).
    pub(super) fn is_subtype_with_projections(&self, super_ty: &Type, sub_ty: &Type) -> bool {
        let super_resolved = self.resolve_assoc_projections(super_ty);
        let sub_resolved = self.resolve_assoc_projections(sub_ty);
        if projection_unresolvable_with(&super_resolved, &sub_resolved) {
            return true;
        }
        // `impl Trait` slice 3 — caller-side opacity for return-position
        // existentials. An `expected: impl Trait, found: ConcreteWitness`
        // assignment is accepted when the witness implements the trait;
        // this is the "body's return-expr type satisfies the declared
        // trait" check at the body-tail position and the "call site
        // accepting an existential into an `impl Trait`-typed slot" path.
        // The reverse direction (`expected: Concrete, found: impl Trait`)
        // falls through to the structural rule below, which now rejects
        // the existential→concrete cross thanks to the existential arms
        // in `types_compatible`. Same-origin existentials are unified by
        // `types_compatible` directly.
        if let Type::Existential {
            trait_name,
            origin: super_origin,
            ..
        } = &super_resolved
        {
            if let Type::Existential {
                origin: sub_origin, ..
            } = &sub_resolved
            {
                return super_origin == sub_origin;
            }
            return self.type_satisfies_bound(&sub_resolved, trait_name);
        }
        is_subtype(&super_resolved, &sub_resolved)
    }

    pub(super) fn check_assignable(&mut self, expected: &Type, found: &Type, span: Span) -> bool {
        if self.is_subtype_with_projections(expected, found) {
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
        // `impl Trait` slice 3 — focused diagnostic for the body-return-
        // doesn't-satisfy-trait case. When the expected slot is a return-
        // position `impl Trait` (or any other existential-typed slot) and
        // the found type is a concrete witness that does not implement
        // the declared trait, surface `E_IMPL_TRAIT_MISSING_BOUND` rather
        // than the generic "expected X, found Y" so the user sees the
        // missing trait by name. The opaque cross-direction (expected:
        // Concrete, found: Existential) falls through to the generic
        // diagnostic — that case is "caller named the witness", and the
        // expected / found type names are already informative.
        if let Type::Existential { trait_name, .. } = expected {
            if !matches!(found, Type::Existential { .. }) {
                self.type_error(
                    format!(
                        "error[E_IMPL_TRAIT_MISSING_BOUND]: function returns \
                         `impl {trait_name}` but body type `{}` does not \
                         implement `{trait_name}`",
                        type_display(found),
                    ),
                    span,
                    TypeErrorKind::TypeMismatch,
                );
                return false;
            }
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

    // ── Check Items (Pass 2) ────────────────────────────────────

    // ── Match ───────────────────────────────────────────────────
}

fn has_unknown_lint(overrides: &[crate::lints::LintLevelOverride]) -> bool {
    overrides
        .iter()
        .any(|ov| crate::lints::lint_by_name(&ov.lint).is_none())
}

/// The item's own lint overrides (the attached `#[allow]` / `#[warn]`
/// / `#[deny]` / `#[expect]` slice from slice 4a). `None` for item
/// kinds that don't carry attributes (`UseDecl`, `Import`, etc.) —
/// callers skip them. `ImplBlock` returns the *block's* overrides;
/// per-method overrides are walked separately at the call site.
fn item_own_lint_overrides(item: &Item) -> Option<&[crate::lints::LintLevelOverride]> {
    match item {
        Item::Function(f) => Some(&f.lint_overrides),
        Item::StructDef(s) => Some(&s.lint_overrides),
        Item::EnumDef(e) => Some(&e.lint_overrides),
        Item::TraitDef(t) => Some(&t.lint_overrides),
        Item::TraitAlias(t) => Some(&t.lint_overrides),
        Item::MarkerTrait(t) => Some(&t.lint_overrides),
        Item::ImplBlock(i) => Some(&i.lint_overrides),
        Item::ConstDecl(c) => Some(&c.lint_overrides),
        Item::TypeAlias(t) => Some(&t.lint_overrides),
        Item::DistinctType(d) => Some(&d.lint_overrides),
        _ => None,
    }
}
