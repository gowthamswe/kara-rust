//! Type environment: registered structs, enums, functions, constants,
//! type aliases, traits, marker traits, trait aliases, distinct types,
//! opaque foreign types, impl blocks, and associated-type bindings.
//!
//! `TypeEnv` is the long-lived index that the `TypeChecker` populates
//! during `build_type_env` and consults throughout inference. The
//! methods here are pure index operations (lookups, dispatch, bound
//! discharge, supertrait closure walk); fresh-var minting and the
//! type-var substitution maps live here too so trait-dispatch can
//! discharge bounds against the same metavar substrate as call-site
//! inference.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

use super::types::{
    impl_args_match, impl_table_key, types_compatible, ConstArg, ConstVarId, Type, TypeVarId,
    VariantTypeInfo,
};

// ── Type Environment ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StructInfo {
    pub generic_params: Vec<String>,
    pub fields: Vec<(String, Type, bool)>, // (name, type, is_pub)
    pub derived_traits: HashSet<String>,
    pub no_rc: bool,
    pub is_shared: bool,
    /// `#[must_use]` annotation carried on the struct declaration
    /// (slice 4 of the `#[must_use]` mandate — see
    /// `docs/implementation_checklist/phase-5-diagnostics.md` §
    /// `#[must_use]` mandate, slice 4). `Some(message)` when the
    /// declaration has the attribute; `message` is the author's reason
    /// string (`#[must_use = "..."]` form) or an empty string for the
    /// bare `#[must_use]` form. `None` when no attribute is present.
    /// Read at discard-site enforcement in `src/must_use_lint.rs`.
    pub must_use_message: Option<String>,
    /// `#[non_exhaustive]` flag carried on the `pub struct` declaration
    /// (slice 1+2 parser captured this on `StructDef.is_non_exhaustive`).
    /// Read at struct-literal / struct-pattern check sites to enforce
    /// the cross-package wildcard / constructor-call rule
    /// (`E_NON_EXHAUSTIVE_CROSS_PACKAGE_LITERAL`).
    pub is_non_exhaustive: bool,
    /// `stdlib_origin` of the defining `StructDef`. Paired with
    /// `is_non_exhaustive` to detect the cross-package case at literal
    /// / pattern sites: a stdlib-defined `#[non_exhaustive]` struct
    /// constructed from a user-origin fn body is the diagnostic-firing
    /// path. Today "stdlib vs user" is the only inter-package boundary
    /// the compiler tracks; when richer per-package boundaries land,
    /// this widens to a `defining_package_id` and the comparison shifts
    /// at the use site without re-flowing through `env_add_struct`.
    pub defining_stdlib_origin: bool,
}

#[derive(Debug, Clone)]
pub struct EnumInfo {
    pub generic_params: Vec<String>,
    pub variants: Vec<(String, VariantTypeInfo)>,
    pub derived_traits: HashSet<String>,
    pub is_shared: bool,
    /// See [`StructInfo::must_use_message`]. Same role on enum
    /// declarations; slice 4 of the `#[must_use]` mandate.
    pub must_use_message: Option<String>,
    /// `#[non_exhaustive]` flag carried on the `pub enum` declaration
    /// (slice 1+2 parser captured this on `EnumDef.is_non_exhaustive`).
    /// Read at the match exhaustiveness site to enforce the cross-package
    /// wildcard rule (slice 5): a `match` on a non-exhaustive enum from
    /// another package must include a `_ =>` arm regardless of variant
    /// coverage, because new variants may land without breaking source
    /// compatibility. Same-package matches keep the strict
    /// variant-by-variant exhaustiveness check.
    pub is_non_exhaustive: bool,
    /// See [`StructInfo::defining_stdlib_origin`]. Same role on enums;
    /// today the only inter-package boundary is stdlib-vs-user.
    pub defining_stdlib_origin: bool,
}

#[derive(Debug, Clone)]
pub struct FunctionSig {
    pub generic_params: Vec<String>,
    pub param_names: Vec<Option<String>>,
    pub params: Vec<Type>,
    pub return_type: Type,
    /// Where-clause constraints carried alongside the signature
    /// (const generics slice 3c). `None` when the function declares
    /// no where clause. The call-site discharge engine walks
    /// `WhereConstraint::ConstPredicate(_)` entries here and
    /// evaluates each predicate against the bound const-args.
    pub where_clause: Option<WhereClause>,
}

#[derive(Debug, Clone)]
pub struct ImplInfo {
    pub target_type: String,
    /// Type arguments of the impl target (`impl Foo for Option[Ordering]`
    /// → `[Type::Named { name: "Ordering", args: [] }]`). Empty means the
    /// impl is generic-on-name — it applies to every instantiation of
    /// `target_type` (the status quo for every impl that pre-dates the
    /// Theme-4 slice). Non-empty means the impl is specialized to the
    /// listed concrete instantiation; lookup matches iff the call site's
    /// args vector-equal the stored args. `env_add_impl` populates this
    /// only when every recursive arg is fully concrete (no `TypeParam`
    /// or `TypeVar`) — generic impls (`impl Foo for Option[T]`) keep
    /// `target_args.is_empty()`.
    pub target_args: Vec<Type>,
    pub trait_name: Option<String>,
    pub methods: HashMap<String, FunctionSig>,
    /// Impl-level type-parameter declarations including their inline
    /// bounds (`impl[T: Ord] Foo for Bar[T]`). Populated by
    /// `env_add_impl`; consumed by the conditional-impl-filtering pass
    /// (slice 1 of the method-resolution CR — see
    /// `phase-4-interpreter.md`) to decide whether an impl applies at a
    /// given call site. `None` for unconditional impls (`impl Foo for
    /// Bar { ... }`).
    pub generic_params: Option<GenericParams>,
    /// Impl-level `where` clause predicates. Same role as
    /// `generic_params`'s inline bounds for the discharge engine; the
    /// two compose additively (every predicate must discharge for the
    /// impl to apply).
    pub where_clause: Option<WhereClause>,
}

/// Associated type names declared by a trait.
#[derive(Debug, Clone)]
pub struct TraitInfo {
    pub assoc_types: Vec<String>,
    /// Names of supertraits declared in `trait Foo: Bar + Baz`.
    pub supertraits: Vec<String>,
}

/// Storage entry for an impl block's associated-type binding.
/// `ty` is the binding's right-hand side, lowered as a template where the
/// substitutable TypeParams are (a) the impl block's own generic params
/// (e.g., `T` in `impl[T] Functor for Wrapper[T]`) and (b) the GAT's own
/// generic params (e.g., `U` in `type Mapped[U] = Pair[T, U]`).
/// `gat_params` lists the GAT-side names so the resolver can build a
/// substitution from the projection's own args. Empty for non-generic
/// bindings (`type Item = i64`). GAT slice 5.
///
/// GAT slice 8b carry-forwards (b) + (c): the trait's `AssocTypeDecl`
/// can attach inline bounds on each GAT param (`type Mapped[U: Trait]`)
/// and a `where`-clause (`type Mapped[U] where U: Trait`). Both are
/// constraints on valid projection-arg instantiations. The
/// `param_bound_traits` vector lists per-positional-param a vector of
/// bound-trait names (paired position-wise with `gat_params`).
/// `where_clause` carries the raw AST clause; the discharge engine
/// substitutes GAT-param names → projection args and walks each
/// `TypeBound` constraint via `type_satisfies_bound`.
#[derive(Debug, Clone)]
pub struct ImplAssocTypeEntry {
    pub ty: Type,
    pub gat_params: Vec<String>,
    /// Position-aligned with `gat_params`. `param_bound_traits[i]` is the
    /// list of bound trait names declared on the i-th GAT param at the
    /// trait's `AssocTypeDecl` (`type Mapped[U: Trait1 + Trait2]`).
    /// Bound trait names are stored as the last segment of each
    /// `TraitBound.path` so `type_satisfies_bound` consults the same
    /// surface as `discharge_type_bounds`. Empty entries for params
    /// with no inline bounds and for non-generic bindings.
    pub param_bound_traits: Vec<Vec<String>>,
    /// The trait's `AssocTypeDecl.where_clause`, cloned at impl-block
    /// registration. Constraints reference the GAT's params (and may
    /// reference the enclosing trait's params, although v1 traits
    /// don't carry generics in the assoc-type decl). Discharged at
    /// projection-resolution time after substituting
    /// `gat_params → projection.args`.
    pub where_clause: Option<crate::ast::WhereClause>,
}

pub struct TypeEnv {
    pub structs: HashMap<String, StructInfo>,
    pub enums: HashMap<String, EnumInfo>,
    /// Derived traits for each `distinct type` declaration.
    pub distinct_types: HashMap<String, HashSet<String>>,
    /// Names of opaque foreign types declared inside `unsafe extern "ABI" { ... }`
    /// blocks (`type Foo;`). Consulted by `lower_type_expr_inner` for
    /// `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`, by `infer_field_access` for
    /// `E_OPAQUE_TYPE_NO_FIELDS`, and by `env_add_impl` for
    /// `E_OPAQUE_TYPE_NO_INHERENT_OR_TRAIT_IMPLS` (slice 1b). Slice 1
    /// (registration only) shipped 2026-05-14; slice 1b (use-site
    /// precision) shipped alongside.
    pub opaque_foreign_types: HashSet<String>,
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
    /// assoc_type_name)`; value is an entry carrying the template type plus
    /// the GAT parameter names (empty for non-generic bindings). E.g.
    /// `impl Iterator for Vec[i32]` with `type Item = i32` inserts
    /// `("Vec", "Item") → ImplAssocTypeEntry { ty: i32, gat_params: [] }`;
    /// `impl[T] Functor for Wrapper[T] { type Mapped[U] = Pair[T, U]; }`
    /// inserts `("Wrapper", "Mapped") → ImplAssocTypeEntry { ty: Pair[T, U],
    /// gat_params: ["U"] }`, with both `T` (impl-side) and `U` (GAT-side)
    /// lowered as `Type::TypeParam`. Used by `resolve_assoc_projections` to
    /// substitute `T.Assoc[X1, X2, ...]` after `T` is solved to a concrete
    /// named type — both impl-side params (via the struct's `generic_params`
    /// zipped with the projection's `receiver_args`) and GAT-side params (via
    /// the entry's `gat_params` zipped with the projection's own `args`) are
    /// substituted in one pass. GAT slice 5.
    pub impl_assoc_types: HashMap<(String, String), ImplAssocTypeEntry>,
    /// Names of functions declared with `#[compiler_builtin]` in stdlib
    /// source (CR-202 slice 2). The signature still lives in `functions`
    /// — the entry here marks the function as having its body replaced by
    /// Rust dispatch, so `check_items` skips body type-checking and the
    /// interpreter knows not to evaluate the placeholder body. Slice 1's
    /// resolver gate (`E0237`) prevents user code from getting entries
    /// into this set.
    pub compiler_builtins: HashSet<String>,
    /// `#[must_use]` annotations on free functions and impl methods
    /// (slice 4 of the `#[must_use]` mandate). Keyed by the canonical
    /// name the discard-site lookup uses: `"name"` for free functions,
    /// `"TargetType.method"` for inherent / trait-impl methods (the
    /// same shape produced by `method_callee_types` and
    /// `bare_assoc_fn_targets`). Value is `Some(message)` when the
    /// attribute carries an author-supplied reason
    /// (`#[must_use = "..."]`), `Some("")` for bare `#[must_use]`,
    /// and the absence of the key means the function carries no
    /// attribute. Populated by `env_add_function` / `env_add_impl`;
    /// consumed by `must_use_lint`'s discard-site walker via the
    /// snapshot on `TypeCheckResult.must_use_functions`.
    pub must_use_functions: HashMap<String, Option<String>>,
    #[allow(dead_code)]
    pub(super) next_type_var: u32,
    #[allow(dead_code)]
    pub(super) substitutions: HashMap<TypeVarId, Type>,
    /// Const-arg metavar counter, parallel to `next_type_var`. Bumped
    /// when `instantiate_signature_with_fresh_vars` mints a fresh
    /// `ConstVarId` per unique const-param name in a signature (const
    /// generics slice 3b).
    #[allow(dead_code)]
    pub(super) next_const_var: u32,
    /// Const-arg metavar substitutions, parallel to `substitutions`.
    /// Populated by `unify_const_args` at call sites; consumed by
    /// `resolve_type_vars` (Array arm) to substitute `ConstArg::ConstVar`
    /// with its bound value.
    #[allow(dead_code)]
    pub(super) const_substitutions: HashMap<ConstVarId, ConstArg>,
}

impl TypeEnv {
    pub(super) fn new() -> Self {
        TypeEnv {
            structs: HashMap::new(),
            enums: HashMap::new(),
            distinct_types: HashMap::new(),
            opaque_foreign_types: HashSet::new(),
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
            must_use_functions: HashMap::new(),
            next_type_var: 0,
            substitutions: HashMap::new(),
            next_const_var: 0,
            const_substitutions: HashMap::new(),
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

    /// Look up the impl of `trait_name` for `target_type`. The match
    /// rule is the Theme-4 args-aware shape: a stored impl's
    /// `target_args` matches the call site iff the stored args are
    /// empty (impl is generic-on-name, applies to any instantiation)
    /// OR they vector-equal the call-site args. Callers without any
    /// generic-arg context pass `&[]`, which selectively sees only
    /// generic-on-name impls (specialized impls become invisible to
    /// these callers — correct conservative default).
    pub fn find_impl(
        &self,
        trait_name: &str,
        target_type: &str,
        target_args: &[Type],
    ) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get(trait_name)?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target_type && impl_args_match(&imp.target_args, target_args)
            })
    }

    pub fn has_impl(&self, trait_name: &str, target_type: &str, target_args: &[Type]) -> bool {
        self.find_impl(trait_name, target_type, target_args)
            .is_some()
    }

    /// Look up a method by name on `target_type` from `impls`, preferring
    /// inherent methods over trait methods per design.md § Method Resolution
    /// Step 3. Returns the first inherent impl's method if any matches;
    /// otherwise the first trait impl's method. First-match within each tier
    /// — multi-inherent and multi-trait ambiguity detection is deferred to
    /// the step-4 work tracked in phase-4-interpreter.md. The `target_args`
    /// parameter applies the Theme-4 args-match rule (see
    /// [`Self::find_impl`]).
    pub fn find_method(
        &self,
        target_type: &str,
        target_args: &[Type],
        method: &str,
    ) -> Option<&FunctionSig> {
        let mut inherent: Option<&FunctionSig> = None;
        let mut trait_method: Option<&FunctionSig> = None;
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
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
    /// Step 7). The `target_args` parameter applies the Theme-4 args-match
    /// rule (see [`Self::find_impl`]).
    pub fn collect_method_names(&self, target_type: &str, target_args: &[Type]) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
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

    /// Conditional-impl-aware variant of [`Self::find_method`] — slice 1 of
    /// the method-resolution CR (see `phase-4-interpreter.md`). Filters
    /// candidates by [`Self::impl_bounds_discharge`] before applying the
    /// inherent-beats-trait priority. Pass `target_args` from the receiver's
    /// `Type::Named { args }` so impl-level bounds (`impl[T: Ord] Foo for
    /// Bar[T]`) discharge against the concrete instantiation.
    pub fn find_method_with_args(
        &self,
        target_type: &str,
        target_args: &[Type],
        method: &str,
    ) -> Option<&FunctionSig> {
        let mut inherent: Option<&FunctionSig> = None;
        let mut trait_method: Option<&FunctionSig> = None;
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
                continue;
            }
            let Some(sig) = imp.methods.get(method) else {
                continue;
            };
            if !self.impl_bounds_discharge(imp, target_args) {
                continue;
            }
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

    /// Slice 3 of the method-resolution CR — all-candidates variant of
    /// [`Self::find_method_with_args`]. Returns every impl that matches
    /// `target_type` + `method` after the conditional-impl-filtering pass
    /// (`impl_bounds_discharge`), partitioned by inherent-vs-trait priority:
    /// if any inherent impls match, only those are returned; otherwise the
    /// surviving trait impls are returned. Each entry is `(&ImplInfo,
    /// &FunctionSig)` so callers can render the source impl in ambiguity
    /// diagnostics. The returned vec preserves source order.
    ///
    /// A length-1 result is the dispatch-normally case; length ≥ 2 is the
    /// ambiguity case (item 4 of the parent CR — see
    /// `phase-4-interpreter.md`).
    pub fn find_methods_with_args(
        &self,
        target_type: &str,
        target_args: &[Type],
        method: &str,
    ) -> Vec<(&ImplInfo, &FunctionSig)> {
        let mut inherent: Vec<(&ImplInfo, &FunctionSig)> = Vec::new();
        let mut trait_methods: Vec<(&ImplInfo, &FunctionSig)> = Vec::new();
        for imp in &self.impls {
            if imp.target_type != target_type || !impl_args_match(&imp.target_args, target_args) {
                continue;
            }
            let Some(sig) = imp.methods.get(method) else {
                continue;
            };
            if !self.impl_bounds_discharge(imp, target_args) {
                continue;
            }
            if imp.trait_name.is_none() {
                inherent.push((imp, sig));
            } else {
                trait_methods.push((imp, sig));
            }
        }
        if !inherent.is_empty() {
            inherent
        } else {
            trait_methods
        }
    }

    /// Slice 1 of the method-resolution CR — conditional impl filtering.
    /// Returns `true` when an impl applies at the call site whose receiver
    /// type instantiates with `target_args`. Unconditional impls (no
    /// `generic_params`) discharge trivially. Conditional impls
    /// (`impl[T: Ord] Foo for Bar[T]` and `where`-clause variants)
    /// substitute each impl-level type parameter with its concrete arg
    /// from `target_args` and check that every declared bound holds.
    /// Walks the supertrait graph, so `T: Ord` discharges against any
    /// type that impls `Ord` directly OR impls a trait whose supertrait
    /// closure reaches `Ord`.
    ///
    /// Out of scope for slice 1 (see roadmap in `phase-4-interpreter.md`):
    /// associated-type bounds (`T: Iterator<Item=i32>`), bounds with
    /// generic args on the trait (`T: Foo[U]` — only the path-tail trait
    /// name is consulted today), and the deeper "impl on a specific
    /// type-argument instantiation" key extension that unblocks
    /// `impl Option[Ordering]`.
    pub fn impl_bounds_discharge(&self, imp: &ImplInfo, target_args: &[Type]) -> bool {
        let Some(gp) = &imp.generic_params else {
            // No impl-level generic params → no inline bounds; the where
            // clause (if present) couldn't reference any names anyway.
            return true;
        };

        // Substitution map: impl-level generic-param name → concrete arg.
        let subs: std::collections::HashMap<&str, &Type> = gp
            .params
            .iter()
            .zip(target_args.iter())
            .map(|(p, a)| (p.name.as_str(), a))
            .collect();

        // Inline bounds on each generic param.
        for param in &gp.params {
            if param.bounds.is_empty() {
                continue;
            }
            let Some(&subst_ty) = subs.get(param.name.as_str()) else {
                // Receiver had fewer type args than the impl declares — can't
                // substitute the param to discharge its bounds. Conservative:
                // drop the candidate.
                return false;
            };
            for bound in &param.bounds {
                if !self.bound_satisfied(subst_ty, bound) {
                    return false;
                }
            }
        }

        // Where-clause `TypeBound` predicates: `where T: Bound`. Each
        // predicate's `type_name` is either an impl-level generic param
        // (substituted via `subs`) or a concrete type name (treated as
        // a bare `Type::Named` lookup against `env.impls`). The latter
        // covers cases like `where AnotherType: Ord` that name a type
        // unrelated to the impl's generics.
        if let Some(wc) = &imp.where_clause {
            for constraint in &wc.constraints {
                let WhereConstraint::TypeBound {
                    type_name, bounds, ..
                } = constraint
                else {
                    // `AssocTypeEq` predicates are out of scope for slice 1.
                    continue;
                };
                if bounds.is_empty() {
                    continue;
                }
                let owned;
                let target_ty: &Type = if let Some(&t) = subs.get(type_name.as_str()) {
                    t
                } else {
                    owned = Type::Named {
                        name: type_name.clone(),
                        args: Vec::new(),
                    };
                    &owned
                };
                for bound in bounds {
                    if !self.bound_satisfied(target_ty, bound) {
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Discharge `bound` against `ty`. The bound's last path segment names
    /// the trait. Walks the supertrait graph via [`Self::type_satisfies_trait`].
    fn bound_satisfied(&self, ty: &Type, bound: &TraitBound) -> bool {
        let Some(trait_name) = bound.path.last() else {
            return false;
        };
        let Some((ty_name, ty_args)) = impl_table_key(ty) else {
            // Type variables, function types, etc. don't appear in
            // `env.impls`. Conservative: drop. Generic call-site resolution
            // against bounds (item 8 of the method-resolution CR) is a
            // separate slice that handles `TypeParam` receivers properly.
            return false;
        };
        self.type_satisfies_trait(&ty_name, &ty_args, trait_name)
    }

    /// `true` when `ty_name` impls `trait_name` directly OR impls some
    /// trait whose supertrait closure reaches `trait_name`. The walk is
    /// finite — supertrait graphs are acyclic by construction. The
    /// `ty_args` parameter applies the Theme-4 args-match rule when
    /// scanning the impl table.
    pub(super) fn type_satisfies_trait(
        &self,
        ty_name: &str,
        ty_args: &[Type],
        trait_name: &str,
    ) -> bool {
        if self.has_impl(trait_name, ty_name, ty_args) {
            return true;
        }
        let directly_impld_traits: Vec<&str> = self
            .impls
            .iter()
            .filter(|imp| imp.target_type == ty_name && impl_args_match(&imp.target_args, ty_args))
            .filter_map(|imp| imp.trait_name.as_deref())
            .collect();
        for start in directly_impld_traits {
            if self.supertrait_closure_contains(start, trait_name) {
                return true;
            }
        }
        false
    }

    /// BFS over the supertrait graph. `true` iff `target_trait` is reachable
    /// from `start_trait` through `TraitInfo::supertraits` edges.
    fn supertrait_closure_contains(&self, start_trait: &str, target_trait: &str) -> bool {
        use std::collections::{HashSet, VecDeque};
        let mut frontier: VecDeque<&str> = VecDeque::from([start_trait]);
        let mut seen: HashSet<&str> = HashSet::from([start_trait]);
        while let Some(name) = frontier.pop_front() {
            let Some(info) = self.traits.get(name) else {
                continue;
            };
            for st in &info.supertraits {
                if st == target_trait {
                    return true;
                }
                if seen.insert(st.as_str()) {
                    frontier.push_back(st);
                }
            }
        }
        false
    }

    /// All trait names reachable from `start_trait` through
    /// `TraitInfo::supertraits` edges, including `start_trait` itself, in
    /// BFS order. Slice 3.5 of the method-resolution CR — the candidate
    /// trait list for `self.method()` dispatch in a trait default body.
    pub(super) fn supertrait_closure_traits(&self, start_trait: &str) -> Vec<String> {
        use std::collections::{HashSet, VecDeque};
        let mut order: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut frontier: VecDeque<String> = VecDeque::new();
        seen.insert(start_trait.to_string());
        frontier.push_back(start_trait.to_string());
        while let Some(name) = frontier.pop_front() {
            order.push(name.clone());
            if let Some(info) = self.traits.get(&name) {
                for st in &info.supertraits {
                    if seen.insert(st.clone()) {
                        frontier.push_back(st.clone());
                    }
                }
            }
        }
        order
    }

    /// Find a `From` impl mapping `source` → `target`. Disambiguates
    /// multiple `impl From[X] for T` impls for the same target by matching
    /// the `from` method's first parameter type against `source`. The
    /// `target_args` parameter applies the Theme-4 args-match rule
    /// (`&[]` selectively sees only generic-on-name From impls).
    pub fn find_from_impl(
        &self,
        source: &Type,
        target: &str,
        target_args: &[Type],
    ) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get("From")?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target
                    && impl_args_match(&imp.target_args, target_args)
                    && imp.methods.get("from").and_then(|sig| sig.params.first()) == Some(source)
            })
    }

    /// Find a `TryFrom` impl mapping `source` → `target`. Disambiguates
    /// multiple `impl TryFrom[X] for T` impls for the same target by matching
    /// the `try_from` method's first parameter type against `source`.
    /// The `target_args` parameter applies the Theme-4 args-match rule.
    pub fn find_tryfrom_impl(
        &self,
        source: &Type,
        target: &str,
        target_args: &[Type],
    ) -> Option<&ImplInfo> {
        self.impls_by_trait
            .get("TryFrom")?
            .iter()
            .map(|&i| &self.impls[i])
            .find(|imp| {
                imp.target_type == target
                    && impl_args_match(&imp.target_args, target_args)
                    && imp
                        .methods
                        .get("try_from")
                        .and_then(|sig| sig.params.first())
                        == Some(source)
            })
    }

    pub(super) fn fresh_type_var(&mut self) -> Type {
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
