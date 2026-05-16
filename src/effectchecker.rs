// src/effectchecker.rs

//! Effect system analysis for the Kāra language.
//!
//! Infers effects for private functions by tracing the call graph,
//! verifies public function effect declarations match their bodies,
//! and detects effect conflicts for auto-concurrency analysis.

use crate::ast::*;
use crate::manifest::CompileProfile;
use crate::resolver::SpanKey;
use crate::token::Span;
use std::collections::{HashMap, HashSet};

mod inference;
mod subtyping;
mod verify;

// ── Configuration ───────────────────────────────────────────────

/// Policy for effect declarations on public functions.
///
/// Mirrors the `public_effects` knob in `kara.toml`:
/// - `Declared` (default): every `pub fn` must declare its effects; the
///   compiler verifies the declaration matches the inferred set.
/// - `Inferred`: `pub fn` effects may be left undeclared and are inferred
///   from the body (same treatment as private functions). If a `pub fn`
///   does write an explicit `with ...` clause, it is still verified —
///   writing the declaration is an implicit per-function opt-in. This
///   mode targets prototyping and application crates where the stability
///   contract of declared effects isn't worth the annotation cost.
///
/// Does not affect private-function inference or trait method policy —
/// trait method rules are specified separately in `docs/design.md`
/// § Trait Coherence and Effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PublicEffectsPolicy {
    #[default]
    Declared,
    Inferred,
}

// ── Core Types ──────────────────────────────────────────────────

/// A single effect: a verb applied to a resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Effect {
    pub verb: EffectVerbKind,
    pub resource: String,
}

/// Where an effect originates (for diagnostic tracing).
#[derive(Debug, Clone)]
pub enum EffectOrigin {
    /// Effect performed directly in this function.
    Direct(Span),
    /// Effect inherited from calling another function.
    Callee { fn_name: String, span: Span },
}

/// A traced effect with its origin for diagnostics.
#[derive(Debug, Clone)]
pub struct TracedEffect {
    pub effect: Effect,
    pub origin: EffectOrigin,
}

/// The resolved set of effects for a function.
#[derive(Debug, Clone, Default)]
pub struct EffectSet {
    pub effects: Vec<TracedEffect>,
}

impl EffectSet {
    pub fn new() -> Self {
        EffectSet {
            effects: Vec::new(),
        }
    }

    pub fn add(&mut self, effect: Effect, origin: EffectOrigin) {
        // Don't add duplicates (same verb+resource)
        if !self.effects.iter().any(|e| e.effect == effect) {
            self.effects.push(TracedEffect { effect, origin });
        }
    }

    fn contains(&self, effect: &Effect) -> bool {
        self.effects.iter().any(|e| &e.effect == effect)
    }

    fn effect_set(&self) -> HashSet<Effect> {
        self.effects.iter().map(|e| e.effect.clone()).collect()
    }

    fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }
}

/// Whether a function's effects are declared, polymorphic, or absent.
#[derive(Debug, Clone)]
pub enum DeclaredEffects {
    /// Explicit effect list declared on the function.
    Explicit(EffectSet),
    /// `with _` — effects vary by context. May also carry fixed effects
    /// from concrete verbs declared alongside `_` (e.g. `with reads(X) + _`).
    Polymorphic,
    /// `with _` combined with concrete effect declarations.
    /// The `EffectSet` holds the fixed (non-polymorphic) portion.
    PolymorphicWithFixed(EffectSet),
    /// No effect annotation — pure or inferred.
    None,
}

/// A conflict between two effects.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub effect_a: Effect,
    pub effect_b: Effect,
    pub resource: String,
}

// ── Errors ──────────────────────────────────────────────────────

/// Structured payload for E0404 `EffectSubtypeViolation` diagnostics.
///
/// Carries the slot effect set, the argument effect set, and the specific
/// offending effects so JSON clients can display a precise subset trace
/// without re-parsing the human-readable message.
#[derive(Debug, Clone)]
pub struct EffectSubtypeTrace {
    /// Declared effects of the slot (the parameter's `with` annotation).
    pub slot_effects: Vec<String>,
    /// Full effect set inferred for the argument.
    pub argument_effects: Vec<String>,
    /// Effects in the argument that are not in the slot (`argument \ slot`).
    pub offending_effects: Vec<String>,
    /// Round 10.3 step 7: the callee's signature with type parameters resolved
    /// to concrete types and effect variables resolved to their per-call
    /// bindings. `None` when the callee has no generic parameters or no
    /// substitutions are known. Always concrete when `Some` — by step 6's
    /// error-ordering guarantee, type substitution finished before the
    /// effect-subtyping check fired.
    pub monomorphized_signature: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EffectError {
    pub message: String,
    pub span: Span,
    pub kind: EffectErrorKind,
    /// Optional structured payload — currently only populated for E0404.
    pub subtype_trace: Option<EffectSubtypeTrace>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EffectErrorKind {
    MissingEffectDeclaration,
    OverDeclaredEffect,
    CircularEffectGroup,
    UndefinedEffectGroup,
    /// A function argument's effect set is not a subset of the slot's declared effects.
    EffectSubtypeViolation,
    /// An `extern` declaration includes an effect forbidden by the active compile profile.
    ProfileViolation,
    /// Advisory lint hint on an `extern` declaration — never a compile error.
    FfiLintHint,
    /// An impl method's inferred effects exceed the trait method's declared ceiling.
    ImplExceedsTraitCeiling,
    /// A trait default method body's inferred effects exceed the method's declared ceiling.
    TraitDefaultExceedsCeiling,
    /// A named effect variable `E` (declared in `[with E]`) appears at multiple
    /// `with E` slots in a call's signature, but the closure arguments at those
    /// slots have inferred effect sets that disagree. Per design.md:315–317,
    /// `E` is single-assignment per call: the first `with E` slot binds it; later
    /// slots must match. Two `with _` slots, by contrast, are independent.
    EffectVariableConflict,
}

impl std::fmt::Display for EffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

// ── Mutual Recursion Tracing ───────────────────────────────────

/// A single step in the effect resolution trace for a mutual recursion group.
#[derive(Debug, Clone)]
pub struct ResolvedEffect {
    /// The function containing the call site.
    pub call_site_function: String,
    /// Line number of the call site.
    pub call_site_line: usize,
    /// The callee through which the effect was resolved.
    pub resolved_via: String,
    /// The effect that was resolved, e.g. "reads(DB)".
    pub effect: String,
}

/// A mutual recursion group (SCC with >1 function) and its effect resolution trace.
#[derive(Debug, Clone)]
pub struct MutualRecursionGroup {
    /// Functions in the group.
    pub functions: Vec<String>,
    /// The resolution trace showing how effects propagated through the cycle.
    pub resolution_trace: Vec<ResolvedEffect>,
}

// ── Result ──────────────────────────────────────────────────────

pub struct EffectCheckResult {
    /// Inferred effects for each function (by name).
    pub inferred_effects: HashMap<String, EffectSet>,
    /// Declared effects for each function.
    pub declared_effects: HashMap<String, DeclaredEffects>,
    /// Expanded effect groups.
    pub expanded_groups: HashMap<String, EffectSet>,
    /// Transparent effect verbs.
    pub transparent_effects: HashSet<String>,
    /// Detected mutual recursion groups with effect resolution traces.
    pub mutual_recursion_groups: Vec<MutualRecursionGroup>,
    /// Whether each function is public (by name). Used by CLI output to
    /// surface per-public-fn inferred effects.
    pub function_visibility: HashMap<String, bool>,
    /// The policy this result was produced under.
    pub public_effects_policy: PublicEffectsPolicy,
    /// Errors and warnings.
    pub errors: Vec<EffectError>,
}

// ── Effect Checker ──────────────────────────────────────────────

pub struct EffectChecker<'a> {
    pub(crate) program: &'a Program,
    /// Public-function effect declaration policy (see `PublicEffectsPolicy`).
    pub(crate) public_effects_policy: PublicEffectsPolicy,
    /// Expanded effect groups: group name → EffectSet.
    pub(crate) expanded_groups: HashMap<String, EffectSet>,
    /// Active compile profile — determines which effects are forbidden at extern sites.
    pub(crate) profile: CompileProfile,
    /// Transparent effect verb names.
    pub(crate) transparent_effects: HashSet<String>,
    /// Declared effects per function name.
    pub(crate) declared_effects: HashMap<String, DeclaredEffects>,
    /// Inferred effects per function name.
    pub(crate) inferred_effects: HashMap<String, EffectSet>,
    /// Whether each function is public.
    pub(crate) function_visibility: HashMap<String, bool>,
    /// Function spans for error reporting.
    pub(crate) function_spans: HashMap<String, Span>,
    /// Functions and their AST bodies (for inference).
    pub(crate) function_bodies: HashMap<String, Function>,
    /// Impl method bodies: "TypeName.method" → Function
    pub(crate) method_bodies: HashMap<String, Function>,
    /// Functions that call polymorphic (`with _`) callees.
    pub(crate) calls_polymorphic: HashSet<String>,
    /// Functions that explicitly declare `with _` (anonymous polymorphism)
    /// — distinct from `with E` (named) declarations. The viral rule fires
    /// only for `with _` callees: `with E` is resolved at the call site
    /// against concrete bindings, so it does not "leak" through callers
    /// that lack a `with _` of their own.
    pub(crate) fn_uses_with_underscore: HashSet<String>,
    /// Per-function generic-parameter bounds index: function key (e.g.
    /// `"sort"` or `"Wrapper.default"`) → param name → bounds. Populated
    /// once before inference; consulted by `extract_trait_assoc_fn_keys`
    /// to redirect `T.method()` and bare `method()` calls to the matching
    /// `Trait.method` ceiling key.
    pub(crate) fn_bounds_index: HashMap<String, HashMap<String, Vec<TraitBound>>>,
    /// Per-function effect-variable position index: function key → effect
    /// variable name → list of parameter indices whose `Fn(...) with E`
    /// type references that variable. Populated after `collect_function_info`;
    /// consulted by `check_with_e_unification` to enforce single-assignment
    /// of `E` across multiple `with E` slots in one signature
    /// (design.md:315–317). A variable that appears at only one position
    /// adds no constraint beyond the existing `with _` polymorphic behavior;
    /// a variable at 2+ positions requires every closure argument's effect
    /// set to agree, with a conflict diagnostic otherwise.
    pub(crate) fn_effect_var_positions: HashMap<String, HashMap<String, Vec<usize>>>,
    /// Method-call → resolved `Type.method` key, populated by the typechecker
    /// (`TypeCheckResult.method_callee_types`). Used by `MethodCall` arms in
    /// `collect_calls_in_expr` (function-reference arg propagation), in
    /// `check_with_e_in_expr` (`with E` unification), and in
    /// `check_subtyping_in_expr_owned` (Fn-slot subtyping) to resolve the
    /// callee precisely instead of falling back to per-method-name heuristics.
    /// Empty when constructed via the unparameterised `new` family.
    pub(crate) method_callee_types: HashMap<SpanKey, String>,
    /// Per-call-site type-parameter substitutions, populated by the
    /// typechecker (`TypeCheckResult.call_type_subs`). Maps a call-expression
    /// span to a `param_name → resolved_type_name` table — concrete entries
    /// (`"Vec[i64]"`) come from the typechecker's solver. Round 10.3 step 7
    /// reads this to render a fully-monomorphized callee signature in E0404
    /// diagnostics, so the user sees `Fn(i64) -> ()` instead of `Fn(T) -> ()`
    /// when a generic call's effect-subtyping check fails. Empty when
    /// constructed via the unparameterised `new` family.
    pub(crate) call_type_subs: HashMap<SpanKey, HashMap<String, String>>,
    pub(crate) errors: Vec<EffectError>,
}

impl<'a> EffectChecker<'a> {
    pub fn new(program: &'a Program) -> Self {
        Self::new_with_policy(program, PublicEffectsPolicy::default())
    }

    pub fn new_with_policy(program: &'a Program, policy: PublicEffectsPolicy) -> Self {
        Self::new_with_policy_and_profile(program, policy, CompileProfile::Default)
    }

    pub fn new_with_policy_and_profile(
        program: &'a Program,
        policy: PublicEffectsPolicy,
        profile: CompileProfile,
    ) -> Self {
        EffectChecker {
            program,
            public_effects_policy: policy,
            profile,
            expanded_groups: HashMap::new(),
            transparent_effects: HashSet::new(),
            declared_effects: HashMap::new(),
            inferred_effects: HashMap::new(),
            function_visibility: HashMap::new(),
            function_spans: HashMap::new(),
            function_bodies: HashMap::new(),
            method_bodies: HashMap::new(),
            calls_polymorphic: HashSet::new(),
            fn_uses_with_underscore: HashSet::new(),
            fn_bounds_index: HashMap::new(),
            fn_effect_var_positions: HashMap::new(),
            method_callee_types: HashMap::new(),
            call_type_subs: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// Attach typechecker-resolved `MethodCall` callee keys so the `MethodCall`
    /// arms of effect collection / `with E` unification / Fn-slot subtyping
    /// can resolve to the precise `Type.method` (mirroring the precision the
    /// `Call` arms already get from `extract_callee_name`).
    pub fn with_method_callee_types(mut self, types: HashMap<SpanKey, String>) -> Self {
        self.method_callee_types = types;
        self
    }

    /// Attach the typechecker's per-call-site type-parameter substitution
    /// table so E0404 diagnostics on compound polymorphic calls can render
    /// the callee's signature with type variables fully resolved (Round 10.3
    /// step 7). Without these, an effect-subtyping failure inside a
    /// `f[T, with E]` call shows the slot type with `T` literally — leaving
    /// the user to mentally substitute. With them, the diagnostic carries the
    /// monomorphized form (`Fn(i64) -> ()` instead of `Fn(T) -> ()`).
    pub fn with_call_type_subs(mut self, subs: HashMap<SpanKey, HashMap<String, String>>) -> Self {
        self.call_type_subs = subs;
        self
    }

    /// Resolve the callee key for a `MethodCall` expression's span. Returns
    /// `None` when the typechecker did not populate an entry (e.g. tests that
    /// skip typecheck, builtin/intrinsic dispatch handled outside `infer_method_call`).
    fn resolve_method_callee_key(&self, span: &Span) -> Option<String> {
        self.method_callee_types
            .get(&SpanKey::from_span(span))
            .cloned()
    }

    pub fn check(mut self) -> EffectCheckResult {
        // Seed built-in diverging functions with their known effects.
        // todo() and unreachable() always panic — any function calling them inherits panics.
        let builtin_span = Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        };
        let panics_effect = Effect {
            verb: EffectVerbKind::Panics,
            resource: String::new(),
        };
        // Exhaustive list per design.md § Effect Inference and Boundaries (F-057):
        // unwrap/expect/panic/assert/assert_eq/assert_ne/todo/unreachable/index/div/rem.
        // Arithmetic overflow is NOT in this list — it traps at runtime but is outside
        // the effect system's scope.
        for builtin in [
            "todo",
            "unreachable",
            "panic",
            "assert",
            "assert_eq",
            "assert_ne",
            "__builtin_index",
            "__builtin_unwrap",
            "__builtin_expect",
            "__builtin_div_rem",
            "process.exit",
        ] {
            let mut set = EffectSet::new();
            set.add(
                panics_effect.clone(),
                EffectOrigin::Direct(builtin_span.clone()),
            );
            self.inferred_effects.insert(builtin.to_string(), set);
        }

        // Seed effects for stdlib heap-allocating constructors and methods.
        // These are builtins — no AST body to infer from — so we seed
        // inferred_effects directly. Callers accumulate these effects through
        // the normal call-graph propagation.
        let alloc_heap = Effect {
            verb: EffectVerbKind::Allocates,
            resource: "Heap".to_string(),
        };
        let suspends_effect = Effect {
            verb: EffectVerbKind::Suspends,
            resource: String::new(),
        };
        // Path-call constructors: `Type.new()` form.
        for stdlib_fn in [
            "Vec.new",
            "Vec.from_slice",
            "Vec.push",
            "String.new",
            "String.push_str",
            "Map.new",
            "Map.with_capacity",
            "Map.insert",
            "Map.try_insert",
            "Map.entry",
            "Map.clone",
            "Map.from_iter",
            "Map.extend",
            "Map.merge",
            "Map.keys",
            "Map.values",
            "Map.entries",
            "SortedSet.new",
            "SortedSet.insert",
            "Set.new",
            "Set.with_capacity",
            "Set.insert",
            "Set.clone",
            "Set.from_iter",
            "Set.union",
            "Set.intersection",
            "Set.difference",
            "Channel.new",
            "Sender.send",
            "Iterator.chunk_by",
            "Iterator.chunks",
            "Iterator.windows",
        ] {
            let mut set = EffectSet::new();
            set.add(
                alloc_heap.clone(),
                EffectOrigin::Direct(builtin_span.clone()),
            );
            self.inferred_effects.insert(stdlib_fn.to_string(), set);
        }
        // Receiver.recv suspends (blocks until a message arrives).
        {
            let mut set = EffectSet::new();
            set.add(
                suspends_effect.clone(),
                EffectOrigin::Direct(builtin_span.clone()),
            );
            self.inferred_effects
                .insert("Receiver.recv".to_string(), set);
        }
        // Client.get / Client.post: sends(Network) + receives(Network).
        {
            let sends_network = Effect {
                verb: EffectVerbKind::Sends,
                resource: "Network".to_string(),
            };
            let receives_network = Effect {
                verb: EffectVerbKind::Receives,
                resource: "Network".to_string(),
            };
            for fn_name in ["Client.get", "Client.post"] {
                let mut set = EffectSet::new();
                set.add(
                    sends_network.clone(),
                    EffectOrigin::Direct(builtin_span.clone()),
                );
                set.add(
                    receives_network.clone(),
                    EffectOrigin::Direct(builtin_span.clone()),
                );
                self.inferred_effects.insert(fn_name.to_string(), set);
            }
        }

        // `Env.set(name, value)` / `env.set(name, value)`: writes(Env). The
        // POSIX `setenv` shape mutates the process environment block, so a
        // public caller must declare `writes(Env)` to forward the effect.
        // Both the capitalized (baked-stdlib `impl Env { fn set }`) and
        // lowercase (`env.functions.insert("env.set", …)`) keys are seeded
        // so either dispatch path picks up the effect.
        {
            let writes_env = Effect {
                verb: EffectVerbKind::Writes,
                resource: "Env".to_string(),
            };
            for fn_name in ["Env.set", "env.set"] {
                let mut set = EffectSet::new();
                set.add(
                    writes_env.clone(),
                    EffectOrigin::Direct(builtin_span.clone()),
                );
                self.inferred_effects.insert(fn_name.to_string(), set);
            }
        }

        // Stdlib conversion traits (`From`, `Into`, `TryFrom`, `TryInto`) are
        // registered as trait names only by the typechecker — they have no
        // AST `TraitDef`, so `collect_declared_effects` skips them. Seed
        // `with _` ceilings here so user impls may carry effects: per-method
        // narrowing under a `Polymorphic` ceiling is a no-op (see
        // `verify_impl_trait_ceilings`), and `.into()` / `.try_into()` will
        // route effects through the resolved `From` / `TryFrom` impl.
        for (trait_name, method) in [
            ("From", "from"),
            ("Into", "into"),
            ("TryFrom", "try_from"),
            ("TryInto", "try_into"),
        ] {
            let key = format!("{}.{}", trait_name, method);
            self.declared_effects
                .insert(key.clone(), DeclaredEffects::Polymorphic);
            self.function_visibility.insert(key.clone(), true);
            self.function_spans
                .insert(key.clone(), builtin_span.clone());
            self.fn_uses_with_underscore.insert(key);
        }

        // Phase A: Collect declarations
        self.collect_transparent_effects();
        self.expand_effect_groups();
        self.collect_declared_effects();
        self.apply_unimpled_private_trait_rule();
        self.collect_function_info();
        self.fn_bounds_index = self.build_fn_bounds_index();
        self.fn_effect_var_positions = self.build_fn_effect_var_positions();

        // Phase B: Infer and verify
        self.infer_effects();
        self.infer_private_trait_ceilings();
        self.verify_declarations();
        self.verify_impl_trait_ceilings();
        self.verify_trait_default_bodies();
        self.check_call_site_subtyping();
        self.check_with_e_unification();

        // Phase C: Detect mutual recursion groups and build resolution traces
        let mutual_recursion_groups = self.detect_mutual_recursion_groups();

        EffectCheckResult {
            inferred_effects: self.inferred_effects,
            declared_effects: self.declared_effects,
            expanded_groups: self.expanded_groups,
            transparent_effects: self.transparent_effects,
            mutual_recursion_groups,
            function_visibility: self.function_visibility,
            public_effects_policy: self.public_effects_policy,
            errors: self.errors,
        }
    }

    // ── Phase A: Collection ─────────────────────────────────────

    fn collect_transparent_effects(&mut self) {
        for item in &self.program.items {
            if let Item::EffectVerbDecl(t) = item {
                if t.is_transparent {
                    self.transparent_effects.insert(t.verb_name.clone());
                }
            }
        }
    }

    fn expand_effect_groups(&mut self) {
        // First, collect all group declarations
        let mut group_decls: HashMap<String, &EffectGroupDecl> = HashMap::new();
        for item in &self.program.items {
            if let Item::EffectGroup(g) = item {
                group_decls.insert(g.name.clone(), g);
            }
        }

        // Expand each group (with cycle detection)
        let names: Vec<String> = group_decls.keys().cloned().collect();
        for name in &names {
            if !self.expanded_groups.contains_key(name) {
                let mut visited = HashSet::new();
                self.expand_group(name, &group_decls, &mut visited);
            }
        }
    }

    fn expand_group(
        &mut self,
        name: &str,
        decls: &HashMap<String, &EffectGroupDecl>,
        visited: &mut HashSet<String>,
    ) {
        if self.expanded_groups.contains_key(name) {
            return;
        }
        if visited.contains(name) {
            // Circular reference
            if let Some(decl) = decls.get(name) {
                self.errors.push(EffectError {
                    message: format!("circular effect group reference: '{}'", name),
                    span: decl.span.clone(),
                    kind: EffectErrorKind::CircularEffectGroup,
                    subtype_trace: None,
                });
            }
            self.expanded_groups
                .insert(name.to_string(), EffectSet::new());
            return;
        }
        visited.insert(name.to_string());

        let mut result = EffectSet::new();
        if let Some(decl) = decls.get(name) {
            for term in &decl.body {
                match term {
                    EffectGroupTerm::Verb(verb) => {
                        for resource in &verb.resources {
                            let res_name = resource.path.join(".");
                            result.add(
                                Effect {
                                    verb: verb.kind.clone(),
                                    resource: res_name,
                                },
                                EffectOrigin::Direct(verb.span.clone()),
                            );
                        }
                    }
                    EffectGroupTerm::GroupRef(ref_name) => {
                        if !decls.contains_key(ref_name) {
                            self.errors.push(EffectError {
                                message: format!(
                                    "undefined effect group: '{}' referenced by group '{}'",
                                    ref_name, name
                                ),
                                span: decl.span.clone(),
                                kind: EffectErrorKind::UndefinedEffectGroup,
                                subtype_trace: None,
                            });
                            continue;
                        }
                        // Recursively expand referenced group
                        self.expand_group(ref_name, decls, visited);
                        if let Some(sub_group) = self.expanded_groups.get(ref_name) {
                            for traced in &sub_group.effects {
                                result.add(traced.effect.clone(), traced.origin.clone());
                            }
                        }
                    }
                }
            }
        }
        self.expanded_groups.insert(name.to_string(), result);
    }

    fn collect_declared_effects(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) => {
                    let decl = self.parse_effect_list(&f.effects);
                    if effects_contain_with_underscore(&f.effects) {
                        self.fn_uses_with_underscore.insert(f.name.clone());
                    }
                    self.declared_effects.insert(f.name.clone(), decl);
                    self.function_visibility.insert(f.name.clone(), f.is_pub);
                    self.function_spans.insert(f.name.clone(), f.span.clone());
                }
                Item::ExternFunction(e) => self.register_extern_function_effects(e, &[]),
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(e) => {
                                self.register_extern_function_effects(e, &b.attributes);
                            }
                            // Opaque foreign type declarations carry no
                            // effects — they are type definitions, not
                            // operations. Per design.md § Opaque Foreign
                            // Types > Effect integration.
                            ExternItem::OpaqueType(_) => {}
                        }
                    }
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for item in &imp.items {
                        let method = match item {
                            ImplItem::Method(m) => m,
                            ImplItem::AssocType(_) => continue,
                        };
                        let key = format!("{}.{}", type_name, method.name);
                        let decl = self.parse_effect_list(&method.effects);
                        if effects_contain_with_underscore(&method.effects) {
                            self.fn_uses_with_underscore.insert(key.clone());
                        }
                        self.declared_effects.insert(key.clone(), decl);
                        self.function_visibility.insert(key.clone(), method.is_pub);
                        self.function_spans.insert(key.clone(), method.span.clone());
                    }
                }
                Item::TraitDef(t) => {
                    // Parse the trait-level default effect ceiling (step 3).
                    let trait_ceiling = self.parse_effect_list(&t.trait_effects);
                    for item in &t.items {
                        let method = match item {
                            TraitItem::Method(m) => m,
                            TraitItem::AssocType(_) => continue,
                        };
                        // Method-level `with` fully overrides the trait-level default; no union.
                        let decl = if method.effects.is_some() {
                            self.parse_effect_list(&method.effects)
                        } else {
                            trait_ceiling.clone()
                        };
                        let key = format!("{}.{}", t.name, method.name);
                        let with_underscore_source = if method.effects.is_some() {
                            &method.effects
                        } else {
                            &t.trait_effects
                        };
                        if effects_contain_with_underscore(with_underscore_source) {
                            self.fn_uses_with_underscore.insert(key.clone());
                        }
                        self.declared_effects.insert(key.clone(), decl);
                        // Trait methods inherit trait visibility for the purpose of
                        // public-declaration verification.
                        self.function_visibility.insert(key.clone(), t.is_pub);
                        self.function_spans.insert(key.clone(), method.span.clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn parse_effect_list(&mut self, effects: &Option<EffectList>) -> DeclaredEffects {
        let effects = match effects {
            Some(e) => e,
            None => return DeclaredEffects::None,
        };

        let mut result = EffectSet::new();
        let mut is_polymorphic = false;
        for item in &effects.items {
            match item {
                EffectItem::Verb(verb) => {
                    for resource in &verb.resources {
                        let res_name = resource.path.join(".");
                        result.add(
                            Effect {
                                verb: verb.kind.clone(),
                                resource: res_name,
                            },
                            EffectOrigin::Direct(verb.span.clone()),
                        );
                    }
                }
                EffectItem::Group(name) => {
                    if let Some(group) = self.expanded_groups.get(name) {
                        for traced in &group.effects {
                            result.add(traced.effect.clone(), traced.origin.clone());
                        }
                    } else {
                        self.errors.push(EffectError {
                            message: format!("undefined effect group: '{}'", name),
                            span: effects.span.clone(),
                            kind: EffectErrorKind::UndefinedEffectGroup,
                            subtype_trace: None,
                        });
                    }
                }
                EffectItem::Polymorphic | EffectItem::Variable(_) => {
                    // `with _` and `with E` both make the function effect-polymorphic.
                    // The variable name is available via GenericParams.effect_params
                    // for future SCC unification; here we just mark the function polymorphic.
                    is_polymorphic = true;
                }
            }
        }
        if is_polymorphic {
            if result.is_empty() {
                DeclaredEffects::Polymorphic
            } else {
                DeclaredEffects::PolymorphicWithFixed(result)
            }
        } else {
            DeclaredEffects::Explicit(result)
        }
    }

    /// For every private trait that has no visible `impl Trait for Type` block in
    /// the current program, override all its method ceilings to
    /// `DeclaredEffects::Polymorphic` (unbound / `with _`).
    ///
    /// Rationale: a private trait with no impls has no inference data — we cannot
    /// verify that any future impl will stay within a narrower declared ceiling, so
    /// we treat it as opaque (`with _`). Callers through such a trait propagate
    /// `with _` the same way as an effect-opaque public trait, ensuring they do not
    /// silently drop effects from impls that arrive later.
    ///
    /// Design ref: `docs/design.md § Trait Coherence and Effects`
    /// ("Private-trait SCC inference: unimpled = unbound ceiling").
    fn apply_unimpled_private_trait_rule(&mut self) {
        // Collect all trait names that have at least one ImplBlock.
        let implemented_traits: HashSet<String> = self
            .program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::ImplBlock(imp) => imp
                    .trait_name
                    .as_ref()
                    .map(|p| p.segments.last().cloned().unwrap_or_default()),
                _ => None,
            })
            .collect();

        for item in &self.program.items.clone() {
            let t = match item {
                Item::TraitDef(t) => t,
                _ => continue,
            };
            if t.is_pub {
                continue; // Public traits always use declared-ceiling rules.
            }
            if implemented_traits.contains(&t.name) {
                continue; // Has at least one impl — normal ceiling rules apply.
            }
            // Private trait with no visible impls: override all method ceilings to unbound.
            for trait_item in &t.items {
                let m = match trait_item {
                    TraitItem::Method(m) => m,
                    TraitItem::AssocType(_) => continue,
                };
                let key = format!("{}.{}", t.name, m.name);
                self.declared_effects
                    .insert(key.clone(), DeclaredEffects::Polymorphic);
                // Unimpled private traits are treated as opaque `with _`,
                // not `with E` — callers must propagate the polymorphic
                // marker via the viral rule.
                self.fn_uses_with_underscore.insert(key);
            }
        }
    }

    fn collect_function_info(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) => {
                    self.function_bodies.insert(f.name.clone(), f.clone());
                    // Seed inferred effects from declarations.
                    // Functions that declare effects are trusted — their bodies
                    // may contain effectful calls we can't trace (e.g., FFI, stdlib).
                    if let Some(DeclaredEffects::Explicit(ref set)) =
                        self.declared_effects.get(&f.name)
                    {
                        self.inferred_effects.insert(f.name.clone(), set.clone());
                    }
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for item in &imp.items {
                        let method = match item {
                            ImplItem::Method(m) => m,
                            ImplItem::AssocType(_) => continue,
                        };
                        let key = format!("{}.{}", type_name, method.name);
                        self.method_bodies.insert(key.clone(), (**method).clone());
                        if let Some(DeclaredEffects::Explicit(ref set)) =
                            self.declared_effects.get(&key)
                        {
                            self.inferred_effects.insert(key, set.clone());
                        }
                    }
                }
                Item::TraitDef(t) => {
                    for item in &t.items {
                        let m = match item {
                            TraitItem::Method(m) => m,
                            TraitItem::AssocType(_) => continue,
                        };
                        let body = match &m.body {
                            Some(b) => b.clone(),
                            None => continue,
                        };
                        let key = format!("{}.{}", t.name, m.name);
                        // Synthesise a Function so the SCC inference loop can walk the body.
                        // We intentionally do NOT seed inferred_effects from the declared
                        // ceiling here — the body's inferred effects must stand on their own
                        // so verify_trait_default_bodies can detect violations.
                        let stub = Function {
                            span: m.span.clone(),
                            attributes: Vec::new(),
                            doc_comment: None,
                            is_pub: t.is_pub,
                            is_private: t.is_private,
                            is_unsafe: false,
                            name: m.name.clone(),
                            generic_params: m.generic_params.clone(),
                            params: m.params.clone(),
                            self_param: m.self_param.clone(),
                            return_type: m.return_type.clone(),
                            effects: m.effects.clone(),
                            requires: m.requires.clone(),
                            ensures: m.ensures.clone(),
                            where_clause: m.where_clause.clone(),
                            body,
                            stdlib_origin: t.stdlib_origin,
                        };
                        self.method_bodies.insert(key, stub);
                    }
                }
                _ => {}
            }
        }
    }

    fn extract_callee_name(&self, callee: &Expr) -> Option<String> {
        match &callee.kind {
            ExprKind::Identifier(name) => Some(name.clone()),
            ExprKind::Path { segments, .. } => {
                if segments.len() == 2 {
                    Some(format!("{}.{}", segments[0], segments[1]))
                } else {
                    segments.last().cloned()
                }
            }
            _ => None,
        }
    }

    /// Resolve a call's callee to one or more `Trait.method` keys when the
    /// callee dispatches through a trait associated function. Returns an
    /// empty vec if the callee does not need redirection (concrete-type
    /// dispatch, ordinary function call, etc.) and the existing
    /// `extract_callee_name` result is sufficient on its own.
    ///
    /// `bounds` carries the enclosing function's generic-param bounds — a
    /// `Path([T, m])` whose head is a generic-param key contributes the
    /// bound trait's ceiling; a bare `Identifier(m)` whose name resolves
    /// only via a typeparam-bound trait contributes the same.
    fn extract_trait_assoc_fn_keys(
        &self,
        callee: &Expr,
        bounds: &HashMap<String, Vec<TraitBound>>,
    ) -> Vec<String> {
        match &callee.kind {
            ExprKind::Path { segments, .. } if segments.len() == 2 => {
                let head = &segments[0];
                let method = &segments[1];
                if let Some(bs) = bounds.get(head) {
                    return bs
                        .iter()
                        .filter_map(|b| b.path.last().cloned())
                        .filter(|t| self.trait_declares_no_self_method(t, method))
                        .map(|t| format!("{}.{}", t, method))
                        .collect();
                }
                Vec::new()
            }
            ExprKind::Identifier(name) => {
                // Only redirect if the bare name does not resolve as a value
                // (free function, builtin, enum variant); otherwise the
                // existing key flows through unchanged.
                if self.function_bodies.contains_key(name)
                    || self.declared_effects.contains_key(name)
                {
                    return Vec::new();
                }
                let mut seen: HashSet<String> = HashSet::new();
                bounds
                    .values()
                    .flat_map(|bs| bs.iter())
                    .filter_map(|b| b.path.last().cloned())
                    .filter(|t| self.trait_declares_no_self_method(t, name))
                    .filter(|t| seen.insert(t.clone()))
                    .map(|t| format!("{}.{}", t, name))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// True when trait `trait_name` declares `method_name` as an associated
    /// function (no `self` receiver). Used by the trait-assoc-fn redirection
    /// in call collection.
    fn trait_declares_no_self_method(&self, trait_name: &str, method_name: &str) -> bool {
        for item in &self.program.items {
            if let Item::TraitDef(t) = item {
                if t.name != trait_name {
                    continue;
                }
                for ti in &t.items {
                    if let TraitItem::Method(m) = ti {
                        if m.name == method_name && m.self_param.is_none() {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Collect the inline + where-clause trait bounds for each generic
    /// parameter of `f`, keyed by the param's textual name.
    fn fn_generic_bounds(f: &Function) -> HashMap<String, Vec<TraitBound>> {
        let mut map: HashMap<String, Vec<TraitBound>> = HashMap::new();
        if let Some(ref gp) = f.generic_params {
            for param in &gp.params {
                if !param.bounds.is_empty() {
                    map.entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.iter().cloned());
                }
            }
        }
        if let Some(ref wc) = f.where_clause {
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

    /// Build the bounds map for an impl method: the impl block's generics +
    /// the method's own generics + where clauses. Self stays concrete (the
    /// impl target type) — concrete-type dispatch handles `Self.method()`.
    fn impl_method_bounds(imp: &ImplBlock, method: &Function) -> HashMap<String, Vec<TraitBound>> {
        let mut map: HashMap<String, Vec<TraitBound>> = HashMap::new();
        if let Some(ref gp) = imp.generic_params {
            for param in &gp.params {
                if !param.bounds.is_empty() {
                    map.entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.iter().cloned());
                }
            }
        }
        if let Some(ref wc) = imp.where_clause {
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
        for (name, bounds) in Self::fn_generic_bounds(method) {
            map.entry(name).or_default().extend(bounds);
        }
        map
    }

    /// Build the bounds map for a trait default method body. Self carries the
    /// trait's supertraits as bounds; the trait's own generics + the method's
    /// own generics contribute their bounds.
    fn trait_method_bounds(t: &TraitDef, method: &TraitMethod) -> HashMap<String, Vec<TraitBound>> {
        let mut map: HashMap<String, Vec<TraitBound>> = HashMap::new();
        if !t.supertraits.is_empty() {
            map.entry("Self".to_string())
                .or_default()
                .extend(t.supertraits.iter().cloned());
        }
        if let Some(ref gp) = t.generic_params {
            for param in &gp.params {
                if !param.bounds.is_empty() {
                    map.entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.iter().cloned());
                }
            }
        }
        if let Some(ref wc) = t.where_clause {
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
        if let Some(ref gp) = method.generic_params {
            for param in &gp.params {
                if !param.bounds.is_empty() {
                    map.entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.iter().cloned());
                }
            }
        }
        if let Some(ref wc) = method.where_clause {
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

    /// Build the per-function bounds index: function key (e.g. `"sort"` or
    /// `"Wrapper.default"`) → name → bounds. Called once at the start of
    /// inference.
    fn build_fn_bounds_index(&self) -> HashMap<String, HashMap<String, Vec<TraitBound>>> {
        let mut index: HashMap<String, HashMap<String, Vec<TraitBound>>> = HashMap::new();
        for item in &self.program.items {
            match item {
                Item::Function(f) => {
                    let bounds = Self::fn_generic_bounds(f);
                    if !bounds.is_empty() {
                        index.insert(f.name.clone(), bounds);
                    }
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for impl_item in &imp.items {
                        if let ImplItem::Method(m) = impl_item {
                            let key = format!("{}.{}", type_name, m.name);
                            let bounds = Self::impl_method_bounds(imp, m);
                            if !bounds.is_empty() {
                                index.insert(key, bounds);
                            }
                        }
                    }
                }
                Item::TraitDef(t) => {
                    for ti in &t.items {
                        if let TraitItem::Method(m) = ti {
                            if m.body.is_some() {
                                let key = format!("{}.{}", t.name, m.name);
                                let bounds = Self::trait_method_bounds(t, m);
                                if !bounds.is_empty() {
                                    index.insert(key, bounds);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        index
    }

    /// Build the per-function effect-variable position index.
    /// For each function with `[with E, ...]` declared variables, scans
    /// parameter types: if a param's type is `Fn(...) with E` for some
    /// declared variable name `E`, record the param index under that
    /// name. The function's own `with E` clause and return-position
    /// `with E` slots are not tracked here — only param positions
    /// participate in same-signature unification (the function's own
    /// effect set is a *consequence* of E, not a constraint on it; the
    /// return-position slot likewise inherits from input bindings).
    fn build_fn_effect_var_positions(&self) -> HashMap<String, HashMap<String, Vec<usize>>> {
        let mut index: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        let scan = |f: &Function| -> HashMap<String, Vec<usize>> {
            let declared: HashSet<String> = f
                .generic_params
                .as_ref()
                .map(|gp| gp.effect_params.iter().cloned().collect())
                .unwrap_or_default();
            if declared.is_empty() {
                return HashMap::new();
            }
            let mut by_var: HashMap<String, Vec<usize>> = HashMap::new();
            for (idx, p) in f.params.iter().enumerate() {
                let names = effect_var_names_in_type(&p.ty);
                for name in names {
                    if declared.contains(&name) {
                        by_var.entry(name).or_default().push(idx);
                    }
                }
            }
            by_var
        };
        for item in &self.program.items {
            match item {
                Item::Function(f) => {
                    let by_var = scan(f);
                    if !by_var.is_empty() {
                        index.insert(f.name.clone(), by_var);
                    }
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for impl_item in &imp.items {
                        if let ImplItem::Method(m) = impl_item {
                            let by_var = scan(m);
                            if !by_var.is_empty() {
                                index.insert(format!("{}.{}", type_name, m.name), by_var);
                            }
                        }
                    }
                }
                Item::TraitDef(t) => {
                    for ti in &t.items {
                        if let TraitItem::Method(m) = ti {
                            // Construct a minimal Function-like view of the trait method
                            // for the scanner. Only `params` and `generic_params` matter.
                            let stub = Function {
                                span: m.span.clone(),
                                attributes: Vec::new(),
                                doc_comment: None,
                                is_pub: t.is_pub,
                                is_private: t.is_private,
                                is_unsafe: false,
                                name: m.name.clone(),
                                generic_params: m.generic_params.clone(),
                                params: m.params.clone(),
                                self_param: m.self_param.clone(),
                                return_type: m.return_type.clone(),
                                effects: m.effects.clone(),
                                requires: m.requires.clone(),
                                ensures: m.ensures.clone(),
                                where_clause: m.where_clause.clone(),
                                body: m.body.clone().unwrap_or(Block {
                                    stmts: Vec::new(),
                                    final_expr: None,
                                    span: m.span.clone(),
                                }),
                                stdlib_origin: t.stdlib_origin,
                            };
                            let by_var = scan(&stub);
                            if !by_var.is_empty() {
                                index.insert(format!("{}.{}", t.name, m.name), by_var);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        index
    }

    // ── SCC Detection (Tarjan's Algorithm) ───────────────────────

    /// Detect mutual recursion groups (SCCs with >1 function).
    /// For each group, build a resolution trace showing how effects propagated.
    fn detect_mutual_recursion_groups(&self) -> Vec<MutualRecursionGroup> {
        let call_graph = self.build_call_graph();
        let all_fn_names: HashSet<String> = self
            .function_bodies
            .keys()
            .chain(self.method_bodies.keys())
            .cloned()
            .collect();
        let sccs = tarjan_scc(&all_fn_names, &call_graph);

        // Filter to SCCs with >1 function (actual mutual recursion)
        let mut groups = Vec::new();
        for scc in sccs {
            if scc.len() < 2 {
                continue;
            }

            let scc_set: HashSet<&String> = scc.iter().collect();

            // Build resolution trace: for each function in the SCC, find calls
            // to other SCC members and record which effects were resolved through them
            let mut trace = Vec::new();
            for fn_name in &scc {
                if let Some(calls) = call_graph.get(fn_name) {
                    for (callee, span) in calls {
                        if !scc_set.contains(callee) || callee == fn_name {
                            continue;
                        }
                        // Find effects that this caller inherited from this callee
                        if let Some(callee_effects) = self.inferred_effects.get(callee) {
                            for te in &callee_effects.effects {
                                let effect_str = format!(
                                    "{}({})",
                                    verb_name(&te.effect.verb),
                                    te.effect.resource,
                                );
                                trace.push(ResolvedEffect {
                                    call_site_function: fn_name.clone(),
                                    call_site_line: span.line,
                                    resolved_via: callee.clone(),
                                    effect: effect_str,
                                });
                            }
                        }
                    }
                }
            }

            // Deduplicate trace entries
            let mut seen: HashSet<(String, usize, String, String)> = HashSet::new();
            trace.retain(|r| {
                seen.insert((
                    r.call_site_function.clone(),
                    r.call_site_line,
                    r.resolved_via.clone(),
                    r.effect.clone(),
                ))
            });

            groups.push(MutualRecursionGroup {
                functions: scc,
                resolution_trace: trace,
            });
        }

        groups
    }

    fn is_transparent_verb(&self, verb: &EffectVerbKind) -> bool {
        let verb_str = verb_name(verb);
        self.transparent_effects.contains(&verb_str)
    }

    /// Resolve an `EffectList` from a function-type parameter annotation to a set of Effects.
    /// Used by the call-site subtyping check to get the slot's declared effect set (B).
    ///
    /// `var_bindings` supplies per-call resolutions for `EffectItem::Variable(E)`
    /// so that `Fn(...) with E` slots can be checked against concrete sets
    /// rather than treated as `[pure]`. Pass `None` to get the legacy behavior
    /// (Variable contributes nothing to the slot set).
    fn resolve_effect_list_to_set(
        &self,
        list: &EffectList,
        var_bindings: Option<&HashMap<String, HashSet<Effect>>>,
    ) -> HashSet<Effect> {
        let mut result = HashSet::new();
        for item in &list.items {
            match item {
                EffectItem::Verb(verb) => {
                    for resource in &verb.resources {
                        result.insert(Effect {
                            verb: verb.kind.clone(),
                            resource: resource.path.join("."),
                        });
                    }
                }
                EffectItem::Group(name) => {
                    if let Some(group) = self.expanded_groups.get(name) {
                        result.extend(group.effect_set());
                    }
                }
                EffectItem::Polymorphic => {}
                EffectItem::Variable(name) => {
                    if let Some(bindings) = var_bindings {
                        if let Some(set) = bindings.get(name) {
                            result.extend(set.iter().cloned());
                        }
                    }
                }
            }
        }
        result
    }

    /// Compute per-call bindings for the callee's named effect variables.
    /// For each `E` in `fn_effect_var_positions[callee]`, the binding is the
    /// union of `get_arg_effects(arg)` (excluding transparent verbs) over
    /// every parameter position that references `E`. This is used by the
    /// Fn-slot subtyping check to resolve `with E` slots to concrete sets.
    /// If positions disagree, round 9's unification check fires separately;
    /// using the union here means subtyping passes vacuously in that case
    /// (avoiding double-reporting the same disagreement).
    fn compute_call_var_bindings(
        &self,
        callee_name: &str,
        args: &[CallArg],
    ) -> HashMap<String, HashSet<Effect>> {
        let mut bindings: HashMap<String, HashSet<Effect>> = HashMap::new();
        let positions = match self.fn_effect_var_positions.get(callee_name) {
            Some(p) => p,
            None => return bindings,
        };
        for (var_name, indices) in positions {
            let mut set: HashSet<Effect> = HashSet::new();
            for &idx in indices {
                let Some(arg) = args.get(idx) else { continue };
                let arg_effects = self.get_arg_effects(&arg.value);
                for te in &arg_effects.effects {
                    if !self.is_transparent_verb(&te.effect.verb) {
                        set.insert(te.effect.clone());
                    }
                }
            }
            bindings.insert(var_name.clone(), set);
        }
        bindings
    }

    /// Get the effect set of a function argument expression.
    ///
    /// - Named function reference (`Identifier`): looks up via `get_callee_effects`
    ///   (respects the inference firewall for public functions).
    /// - Closure literal: collects calls in the closure body and unions their effects.
    /// - Anything else (literals, field accesses, etc.): treated as pure.
    fn get_arg_effects(&self, arg: &Expr) -> EffectSet {
        match &arg.kind {
            ExprKind::Identifier(name) => {
                let effects = self.get_callee_effects(name);
                let mut result = EffectSet::new();
                for e in effects {
                    result.add(e, EffectOrigin::Direct(arg.span.clone()));
                }
                result
            }
            ExprKind::Closure { body, .. } => {
                let mut calls = Vec::new();
                let empty_bounds: HashMap<String, Vec<TraitBound>> = HashMap::new();
                self.collect_calls_in_expr(body, &mut calls, &empty_bounds);
                let mut result = EffectSet::new();
                for (callee, span) in calls {
                    for e in self.get_callee_effects(&callee) {
                        result.add(
                            e,
                            EffectOrigin::Callee {
                                fn_name: callee.clone(),
                                span: span.clone(),
                            },
                        );
                    }
                }
                result
            }
            _ => EffectSet::new(),
        }
    }

    // ── Phase D: `with E` same-signature unification ────────────────

    /// At each call site to a function with named effect-variable positions,
    /// enforce single-assignment of each `E` across its referenced parameter
    /// slots (design.md:315–317).
    ///
    /// Algorithm (per call):
    ///   for each variable name `E` declared in the callee's `[with E]`:
    ///     for each parameter index in `fn_effect_var_positions[callee][E]`:
    ///       infer the closure argument's effect set
    ///       on first match, bind `E` to that set
    ///       on subsequent match, compare against the binding; emit
    ///       `EffectVariableConflict` (with both bindings) on disagreement.
    ///
    /// `with _` slots are not in `fn_effect_var_positions` (they're not named),
    /// so they remain independent — a function with two `with _` slots gets
    /// no cross-slot constraint, exactly as today.
    fn check_with_e_unification(&mut self) {
        let bodies: Vec<Block> = self
            .function_bodies
            .values()
            .map(|f| f.body.clone())
            .chain(self.method_bodies.values().map(|f| f.body.clone()))
            .collect();
        for body in bodies {
            self.check_with_e_in_block(&body);
        }
    }

    fn check_with_e_in_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.check_with_e_in_stmt(stmt);
        }
        if let Some(expr) = &block.final_expr {
            self.check_with_e_in_expr(expr);
        }
    }

    fn check_with_e_in_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => self.check_with_e_in_expr(value),
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_block(else_block);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.check_with_e_in_block(body);
            }
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                self.check_with_e_in_expr(target);
                self.check_with_e_in_expr(value);
            }
            StmtKind::Expr(expr) => self.check_with_e_in_expr(expr),
        }
    }

    fn check_with_e_in_expr(&mut self, expr: &Expr) {
        if let ExprKind::Call { callee, args } = &expr.kind {
            if let Some(cname) = self.extract_callee_name(callee) {
                self.check_call_with_e_unification(&cname, args);
            }
            self.check_with_e_in_expr(callee);
            for a in args {
                self.check_with_e_in_expr(&a.value);
            }
            return;
        }
        // Generic structural recursion for everything else.
        match &expr.kind {
            ExprKind::MethodCall { object, args, .. } => {
                // Mirror the `Call` branch: resolve to `Type.method` via the
                // typechecker side-table and run the same `with E` unification
                // pass. The callee's `params` are the explicit (non-self)
                // parameters, so `args` indices align 1:1 with the indices
                // recorded in `fn_effect_var_positions`.
                if let Some(callee_key) = self.resolve_method_callee_key(&expr.span) {
                    self.check_call_with_e_unification(&callee_key, args);
                }
                self.check_with_e_in_expr(object);
                for a in args {
                    self.check_with_e_in_expr(&a.value);
                }
            }
            ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
                self.check_with_e_in_expr(left);
                self.check_with_e_in_expr(right);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.check_with_e_in_expr(left);
                self.check_with_e_in_expr(right);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Question(operand) => {
                self.check_with_e_in_expr(operand);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.check_with_e_in_expr(object);
            }
            ExprKind::Index { object, index } => {
                self.check_with_e_in_expr(object);
                self.check_with_e_in_expr(index);
            }
            ExprKind::Tuple(es) | ExprKind::ArrayLiteral(es) => {
                for e in es {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_expr(count);
            }
            ExprKind::MapLiteral(pairs) => {
                for (k, v) in pairs {
                    self.check_with_e_in_expr(k);
                    self.check_with_e_in_expr(v);
                }
            }
            ExprKind::Block(b)
            | ExprKind::Unsafe(b)
            | ExprKind::Try(b)
            | ExprKind::Seq(b)
            | ExprKind::Par(b) => {
                self.check_with_e_in_block(b);
            }
            ExprKind::Lock { body, .. } => self.check_with_e_in_block(body),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.check_with_e_in_expr(condition);
                self.check_with_e_in_block(then_block);
                if let Some(e) = else_branch {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_block(then_block);
                if let Some(e) = else_branch {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_with_e_in_expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.check_with_e_in_expr(g);
                    }
                    self.check_with_e_in_expr(&arm.body);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.check_with_e_in_expr(condition);
                self.check_with_e_in_block(body);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_block(body);
            }
            ExprKind::For { iterable, body, .. } => {
                self.check_with_e_in_expr(iterable);
                self.check_with_e_in_block(body);
            }
            ExprKind::Loop { body, .. } => self.check_with_e_in_block(body),
            ExprKind::Closure { body, .. } => self.check_with_e_in_expr(body),
            ExprKind::Return(Some(e))
            | ExprKind::Break { value: Some(e), .. }
            | ExprKind::Cast { expr: e, .. } => self.check_with_e_in_expr(e),
            ExprKind::OptionalChain { object, args, .. } => {
                self.check_with_e_in_expr(object);
                if let Some(args) = args {
                    for a in args {
                        self.check_with_e_in_expr(&a.value);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_call_with_e_unification(&mut self, callee_name: &str, args: &[CallArg]) {
        let positions = match self.fn_effect_var_positions.get(callee_name).cloned() {
            Some(p) => p,
            None => return,
        };
        for (var_name, indices) in &positions {
            // Single position → no cross-position unification needed.
            if indices.len() < 2 {
                continue;
            }
            let mut binding: Option<(usize, EffectSet)> = None;
            for &idx in indices {
                let Some(arg) = args.get(idx) else { continue };
                let arg_effects = self.get_arg_effects(&arg.value);
                let arg_set = arg_effects.effects.iter().map(|te| te.effect.clone());
                let arg_concrete: HashSet<Effect> = arg_set
                    .filter(|e| !self.is_transparent_verb(&e.verb))
                    .collect();
                match &binding {
                    None => {
                        let mut seed = EffectSet::new();
                        for e in &arg_concrete {
                            seed.add(e.clone(), EffectOrigin::Direct(arg.value.span.clone()));
                        }
                        binding = Some((idx, seed));
                    }
                    Some((first_idx, first_set)) => {
                        let first_concrete: HashSet<Effect> = first_set
                            .effects
                            .iter()
                            .map(|te| te.effect.clone())
                            .filter(|e| !self.is_transparent_verb(&e.verb))
                            .collect();
                        if first_concrete != arg_concrete {
                            let render = |s: &HashSet<Effect>| -> String {
                                let mut parts: Vec<String> = s
                                    .iter()
                                    .map(|e| format!("{}({})", verb_name(&e.verb), e.resource))
                                    .collect();
                                parts.sort();
                                if parts.is_empty() {
                                    "{}".to_string()
                                } else {
                                    format!("{{{}}}", parts.join(", "))
                                }
                            };
                            self.errors.push(EffectError {
                                message: format!(
                                    "effect variable `{}` is bound to {} at argument {} but \
                                     {} at argument {}; `with {}` requires every slot to agree",
                                    var_name,
                                    render(&first_concrete),
                                    first_idx,
                                    render(&arg_concrete),
                                    idx,
                                    var_name,
                                ),
                                span: arg.value.span.clone(),
                                kind: EffectErrorKind::EffectVariableConflict,
                                subtype_trace: None,
                            });
                        }
                    }
                }
            }
        }
    }

    // ── Phase D: Call-site effect subtyping ─────────────────────────

    /// Whether the effect was contributed through a typeparam-bound trait
    /// dispatch where the trait method itself is polymorphic. Concretely:
    /// the origin is an impl method `X.method`, and the verifying function
    /// has a generic param bounded by a trait that declares `method` with
    /// `with _` or `with E`. The effect routed through `T → Trait.method`
    /// (polymorphic in T), so it is part of E by transitivity and must
    /// not be flagged as a concrete body leak.
    ///
    /// Counter-cases (intentionally NOT skipped):
    ///   - Origin is a polymorphic callee (e.g., a private `with E` helper
    ///     that leaks). The leak still propagates concretely; the verifying
    ///     function is leaking by transitivity.
    ///   - Origin is a `with _` callee whose body has its own concrete leak
    ///     (`with _` permits this in the callee, but the effect surfaces
    ///     concretely in the caller's inferred set).
    fn effect_came_via_polymorphic_callee(&self, te: &TracedEffect, verifying_fn: &str) -> bool {
        let EffectOrigin::Callee { fn_name, .. } = &te.origin else {
            return false;
        };
        let Some((_, method_name)) = fn_name.split_once('.') else {
            return false;
        };
        let Some(bounds_map) = self.fn_bounds_index.get(verifying_fn) else {
            return false;
        };
        for bounds in bounds_map.values() {
            for b in bounds {
                let Some(trait_name) = b.path.last() else {
                    continue;
                };
                let trait_key = format!("{}.{}", trait_name, method_name);
                let trait_decl = self.declared_effects.get(&trait_key);
                let trait_is_poly = matches!(
                    trait_decl,
                    Some(DeclaredEffects::Polymorphic | DeclaredEffects::PolymorphicWithFixed(_))
                );
                if trait_is_poly {
                    return true;
                }
            }
        }
        false
    }

    fn format_effect_origin(&self, fn_name: &str, effect: &Effect) -> String {
        if let Some(inferred_set) = self.inferred_effects.get(fn_name) {
            for traced in &inferred_set.effects {
                if traced.effect == *effect {
                    match &traced.origin {
                        EffectOrigin::Direct(_) => return String::new(),
                        EffectOrigin::Callee {
                            fn_name: callee,
                            span,
                        } => {
                            return format!(" (via call to '{}' at line {})", callee, span.line);
                        }
                    }
                }
            }
        }
        String::new()
    }

    // ── Conflict Detection (public API) ─────────────────────────

    /// Find conflicts between two effect sets.
    pub fn find_conflicts(
        a: &EffectSet,
        b: &EffectSet,
        transparent: &HashSet<String>,
    ) -> Vec<Conflict> {
        let mut conflicts = Vec::new();

        for ea in &a.effects {
            // Skip transparent effects
            let verb_a_name = verb_name(&ea.effect.verb);
            if transparent.contains(&verb_a_name) {
                continue;
            }

            for eb in &b.effects {
                let verb_b_name = verb_name(&eb.effect.verb);
                if transparent.contains(&verb_b_name) {
                    continue;
                }

                // Only check same resource
                if ea.effect.resource != eb.effect.resource {
                    continue;
                }

                // Apply conflict rules
                if effects_conflict(&ea.effect.verb, &eb.effect.verb) {
                    conflicts.push(Conflict {
                        effect_a: ea.effect.clone(),
                        effect_b: eb.effect.clone(),
                        resource: ea.effect.resource.clone(),
                    });
                }
            }
        }
        conflicts
    }

    /// Returns `Some(error_message)` when `effect` is forbidden by the active
    /// compile profile on an `extern` declaration for `fn_name` with `abi`.
    /// Returns `None` when the effect is allowed.
    /// Emit advisory `FfiLintHint` diagnostics for `extern` symbols whose names
    /// suggest commonly-omitted effects (`blocks`, `allocates(Heap)`).
    ///
    /// Never rejects — linter advice only.
    /// Per-`ExternFunction` effect-set registration. Used at both the
    /// (now-dead) top-level `Item::ExternFunction` arm (with
    /// `block_attrs = &[]`) and the per-item arm inside an
    /// `Item::ExternBlock` (with `block_attrs = &b.attributes`).
    /// Block-level attributes are NOT pre-merged into per-item
    /// `attributes` — they're passed through here so the `@noblock`
    /// check sees the union of block + per-item attrs.
    fn register_extern_function_effects(&mut self, e: &ExternFunction, block_attrs: &[Attribute]) {
        // ABI-keyed default effect set (trust-not-verify: extern has no body).
        // `extern "C"` → {blocks}; `extern "C-unwind"` → {blocks, panics}.
        // `@noblock` removes blocks from the default (e.g. a pure-CPU C++ fn).
        let builtin_span = Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        };
        let has_noblock = e.attributes.iter().any(|a| a.name == "noblock")
            || block_attrs.iter().any(|a| a.name == "noblock");
        let mut abi_defaults = EffectSet::new();
        match e.abi.as_str() {
            "C" if !has_noblock => {
                abi_defaults.add(
                    Effect {
                        verb: EffectVerbKind::Blocks,
                        resource: String::new(),
                    },
                    EffectOrigin::Direct(builtin_span.clone()),
                );
            }
            "C" => {}
            "C-unwind" => {
                if !has_noblock {
                    abi_defaults.add(
                        Effect {
                            verb: EffectVerbKind::Blocks,
                            resource: String::new(),
                        },
                        EffectOrigin::Direct(builtin_span.clone()),
                    );
                }
                // panics is always included for C-unwind (throws across FFI boundary).
                // @noblock cannot suppress panics.
                abi_defaults.add(
                    Effect {
                        verb: EffectVerbKind::Panics,
                        resource: String::new(),
                    },
                    EffectOrigin::Direct(builtin_span),
                );
            }
            _ => {} // other ABIs: no defaults until implemented
        }

        // Parse programmer-supplied annotations, then union with ABI defaults.
        let programmer_decl = self.parse_effect_list(&e.effects);
        let final_decl = match &programmer_decl {
            DeclaredEffects::Polymorphic | DeclaredEffects::PolymorphicWithFixed(_) => {
                // Polymorphic extern: unusual but accepted; ABI defaults dropped.
                programmer_decl.clone()
            }
            DeclaredEffects::Explicit(prog_set) => {
                let mut merged = abi_defaults;
                for te in &prog_set.effects {
                    merged.add(te.effect.clone(), te.origin.clone());
                }
                DeclaredEffects::Explicit(merged)
            }
            DeclaredEffects::None => {
                if abi_defaults.is_empty() {
                    DeclaredEffects::None
                } else {
                    DeclaredEffects::Explicit(abi_defaults)
                }
            }
        };

        // Profile-compatibility check: reject effects forbidden by the
        // active compile profile at the extern declaration site.
        if let DeclaredEffects::Explicit(ref set) = final_decl {
            for te in &set.effects {
                if let Some(forbidden_reason) = self.profile_forbids(&te.effect, &e.name, &e.abi) {
                    self.errors.push(EffectError {
                        message: forbidden_reason,
                        span: e.span.clone(),
                        kind: EffectErrorKind::ProfileViolation,
                        subtype_trace: None,
                    });
                }
            }
        }

        // Advisory linter hints for commonly-omitted effects.
        self.check_ffi_linter_hints(&e.name, &e.span, &final_decl);

        self.declared_effects.insert(e.name.clone(), final_decl);
        self.function_visibility.insert(e.name.clone(), true);
        self.function_spans.insert(e.name.clone(), e.span.clone());
        // Seed inferred_effects from the merged set so callers accumulate
        // the correct leaf effects (ABI defaults + programmer annotations).
        if let Some(DeclaredEffects::Explicit(ref set)) = self.declared_effects.get(&e.name) {
            self.inferred_effects.insert(e.name.clone(), set.clone());
        } else {
            self.inferred_effects
                .insert(e.name.clone(), EffectSet::new());
        }
    }

    fn check_ffi_linter_hints(&mut self, symbol: &str, span: &Span, decl: &DeclaredEffects) {
        // Normalize: take the last segment after any `.` separator, strip a
        // leading `_` that some platforms prepend (e.g. macOS `_malloc`).
        let base = symbol.rsplit('.').next().unwrap_or(symbol);
        let base = base.strip_prefix('_').unwrap_or(base);

        let declared_set: Option<&EffectSet> = match decl {
            DeclaredEffects::Explicit(s) => Some(s),
            DeclaredEffects::PolymorphicWithFixed(s) => Some(s),
            _ => None,
        };

        let has_verb = |verb: EffectVerbKind, resource: &str| -> bool {
            declared_set.is_some_and(|s| {
                s.effects.iter().any(|te| {
                    te.effect.verb == verb
                        && (resource.is_empty() || te.effect.resource == resource)
                })
            })
        };

        // Known-blocking symbols — suggest `blocks`.
        const KNOWN_BLOCKING: &[&str] = &[
            "sleep",
            "usleep",
            "nanosleep",
            "read",
            "write",
            "recv",
            "recvfrom",
            "recvmsg",
            "send",
            "sendto",
            "sendmsg",
            "accept",
            "accept4",
            "connect",
            "poll",
            "select",
            "pselect",
            "epoll_wait",
            "kevent",
            "waitpid",
            "wait",
            "wait4",
            "flock",
            "lockf",
            "pthread_mutex_lock",
            "pthread_cond_wait",
            "pthread_join",
            "open",
            "fopen",
            "openat",
            "creat",
            "close",
            "fsync",
            "fdatasync",
            "gethostbyname",
            "getaddrinfo",
        ];

        if KNOWN_BLOCKING.contains(&base) && !has_verb(EffectVerbKind::Blocks, "") {
            self.errors.push(EffectError {
                message: format!(
                    "FFI lint: '{}' is commonly blocking; consider adding `blocks` to its \
                     effect list (or `@noblock` to confirm it is non-blocking in this context)",
                    symbol
                ),
                span: span.clone(),
                kind: EffectErrorKind::FfiLintHint,
                subtype_trace: None,
            });
        }

        // Known-allocating symbols — suggest `allocates(Heap)`.
        const KNOWN_ALLOCATING: &[&str] = &[
            "malloc",
            "calloc",
            "realloc",
            "reallocarray",
            "strdup",
            "strndup",
            "asprintf",
            "vasprintf",
            "posix_memalign",
            "memalign",
            "aligned_alloc",
            "getaddrinfo",
        ];

        if KNOWN_ALLOCATING.contains(&base) && !has_verb(EffectVerbKind::Allocates, "Heap") {
            self.errors.push(EffectError {
                message: format!(
                    "FFI lint: '{}' is commonly allocating; consider adding `allocates(Heap)` \
                     to its effect list",
                    symbol
                ),
                span: span.clone(),
                kind: EffectErrorKind::FfiLintHint,
                subtype_trace: None,
            });
        }
    }

    fn profile_forbids(&self, effect: &Effect, fn_name: &str, abi: &str) -> Option<String> {
        let forbidden = match self.profile {
            CompileProfile::Default => return None,
            CompileProfile::Embedded => matches!(
                (&effect.verb, effect.resource.as_str()),
                (EffectVerbKind::Allocates, "Heap")
            ),
            CompileProfile::Kernel => matches!(
                &effect.verb,
                EffectVerbKind::Allocates
                    | EffectVerbKind::Panics
                    | EffectVerbKind::Blocks
                    | EffectVerbKind::Suspends
            ),
        };
        if forbidden {
            let effect_str = if effect.resource.is_empty() {
                verb_name(&effect.verb)
            } else {
                format!("{}({})", verb_name(&effect.verb), effect.resource)
            };
            Some(format!(
                "extern \"{}\" fn {} declares effect `{}`, which is forbidden by the '{}' profile",
                abi,
                fn_name,
                effect_str,
                self.profile.as_str(),
            ))
        } else {
            None
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────

/// True if the supplied effect declaration contains at least one
/// `EffectItem::Polymorphic` (i.e. an explicit `with _`). This is the
/// distinguishing axis for the viral rule: `with _` is opaque and
/// propagates to callers, whereas `with E` is named and resolves at the
/// call site.
pub(crate) fn effects_contain_with_underscore(effects: &Option<EffectList>) -> bool {
    let Some(list) = effects else {
        return false;
    };
    list.items
        .iter()
        .any(|item| matches!(item, EffectItem::Polymorphic))
}

/// Walk a type expression and collect every effect-variable name that
/// appears in any nested `FnType`'s `effect_spec`. Includes nested Fn
/// types (e.g., `Fn(i64) -> Fn() with E`) and effect lists with mixed
/// items (`with reads(R), E` records `E`).
pub(crate) fn effect_var_names_in_type(ty: &TypeExpr) -> Vec<String> {
    let mut out = Vec::new();
    collect_effect_var_names_in_type(ty, &mut out);
    out
}

fn collect_effect_var_names_in_type(ty: &TypeExpr, out: &mut Vec<String>) {
    match &ty.kind {
        TypeKind::FnType {
            params,
            return_type,
            effect_spec,
            is_once: _,
        } => {
            if let Some(EffectSpec::Specific(list)) = effect_spec {
                for item in &list.items {
                    if let EffectItem::Variable(name) = item {
                        if !out.contains(name) {
                            out.push(name.clone());
                        }
                    }
                }
            }
            for p in params {
                collect_effect_var_names_in_type(p, out);
            }
            if let Some(rt) = return_type {
                collect_effect_var_names_in_type(rt, out);
            }
        }
        TypeKind::Tuple(elems) => {
            for e in elems {
                collect_effect_var_names_in_type(e, out);
            }
        }
        TypeKind::Array { element, .. } => {
            collect_effect_var_names_in_type(element, out);
        }
        TypeKind::Pointer { inner, .. }
        | TypeKind::Ref(inner)
        | TypeKind::MutRef(inner)
        | TypeKind::MutSlice(inner)
        | TypeKind::Weak(inner) => {
            collect_effect_var_names_in_type(inner, out);
        }
        TypeKind::Path(_) | TypeKind::Unit | TypeKind::Error => {}
    }
}

pub(crate) fn verb_name(verb: &EffectVerbKind) -> String {
    match verb {
        EffectVerbKind::Reads => "reads".to_string(),
        EffectVerbKind::Writes => "writes".to_string(),
        EffectVerbKind::Sends => "sends".to_string(),
        EffectVerbKind::Receives => "receives".to_string(),
        EffectVerbKind::Allocates => "allocates".to_string(),
        EffectVerbKind::Panics => "panics".to_string(),
        EffectVerbKind::Blocks => "blocks".to_string(),
        EffectVerbKind::Suspends => "suspends".to_string(),
        EffectVerbKind::UserDefined(name) => name.clone(),
    }
}

/// Render a callee's signature with type-parameter substitutions applied
/// (Round 10.3 step 7). Used in E0404 diagnostics for compound polymorphic
/// calls so the user sees `Fn(i64) -> ()` instead of `Fn(T) -> ()`.
///
/// `type_subs` is `param_name → resolved_type_name` from the typechecker;
/// `var_bindings` resolves named effect variables (`with E`) to concrete sets.
pub(crate) fn format_monomorphized_signature(
    callee_name: &str,
    params: &[Param],
    return_type: Option<&TypeExpr>,
    type_subs: &HashMap<String, String>,
    var_bindings: &HashMap<String, HashSet<Effect>>,
) -> String {
    let mut s = String::new();
    s.push_str(callee_name);
    s.push('(');
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        format_type_expr_with_subs(&p.ty, type_subs, var_bindings, &mut s);
    }
    s.push(')');
    if let Some(rt) = return_type {
        s.push_str(" -> ");
        format_type_expr_with_subs(rt, type_subs, var_bindings, &mut s);
    }
    s
}

/// Recursively render a `TypeExpr` to string, substituting any single-segment
/// `Path(["T"])` whose name matches a key in `type_subs`. `FnType` effect
/// specs are resolved against `var_bindings` so `with E` becomes the concrete
/// set bound at the call site.
fn format_type_expr_with_subs(
    ty: &TypeExpr,
    type_subs: &HashMap<String, String>,
    var_bindings: &HashMap<String, HashSet<Effect>>,
    out: &mut String,
) {
    match &ty.kind {
        TypeKind::Path(p) => {
            // Single-segment path matching a substitution key → resolved name.
            // Multi-segment paths (e.g. `T.Item`) likewise resolve when the
            // first segment matches; the rest is appended verbatim since the
            // typechecker already resolved associated-type projections in step 2.
            if p.segments.len() == 1 {
                if let Some(resolved) = type_subs.get(&p.segments[0]) {
                    out.push_str(resolved);
                    return;
                }
            }
            if p.segments.len() > 1 {
                if let Some(resolved) = type_subs.get(&p.segments[0]) {
                    out.push_str(resolved);
                    for seg in &p.segments[1..] {
                        out.push('.');
                        out.push_str(seg);
                    }
                    if let Some(args) = &p.generic_args {
                        format_generic_args(args, type_subs, var_bindings, out);
                    }
                    return;
                }
            }
            out.push_str(&p.segments.join("."));
            if let Some(args) = &p.generic_args {
                format_generic_args(args, type_subs, var_bindings, out);
            }
        }
        TypeKind::Tuple(types) => {
            out.push('(');
            for (i, t) in types.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_type_expr_with_subs(t, type_subs, var_bindings, out);
            }
            out.push(')');
        }
        TypeKind::Array { element, .. } => {
            // Size is a const expression; render as `Array[T]` to avoid
            // pulling in the expression formatter just for diagnostics.
            out.push_str("Array[");
            format_type_expr_with_subs(element, type_subs, var_bindings, out);
            out.push(']');
        }
        TypeKind::Pointer { is_mut, inner } => {
            out.push_str(if *is_mut { "*mut " } else { "*" });
            format_type_expr_with_subs(inner, type_subs, var_bindings, out);
        }
        TypeKind::FnType {
            params,
            return_type,
            effect_spec,
            is_once,
        } => {
            out.push_str(if *is_once { "OnceFn(" } else { "Fn(" });
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_type_expr_with_subs(p, type_subs, var_bindings, out);
            }
            out.push(')');
            if let Some(rt) = return_type {
                out.push_str(" -> ");
                format_type_expr_with_subs(rt, type_subs, var_bindings, out);
            }
            match effect_spec {
                Some(EffectSpec::Polymorphic) => out.push_str(" with _"),
                Some(EffectSpec::Specific(list)) => {
                    let resolved = resolve_effect_list_for_render(list, var_bindings);
                    out.push_str(" with [");
                    out.push_str(&resolved.join(", "));
                    out.push(']');
                }
                None => {}
            }
        }
        TypeKind::Ref(inner) => {
            out.push_str("ref ");
            format_type_expr_with_subs(inner, type_subs, var_bindings, out);
        }
        TypeKind::MutRef(inner) => {
            out.push_str("mut ref ");
            format_type_expr_with_subs(inner, type_subs, var_bindings, out);
        }
        TypeKind::MutSlice(element) => {
            out.push_str("mut Slice[");
            format_type_expr_with_subs(element, type_subs, var_bindings, out);
            out.push(']');
        }
        TypeKind::Weak(inner) => {
            out.push_str("weak ");
            format_type_expr_with_subs(inner, type_subs, var_bindings, out);
        }
        TypeKind::Unit => out.push_str("()"),
        TypeKind::Error => out.push_str("/* error */"),
    }
}

fn format_generic_args(
    args: &[GenericArg],
    type_subs: &HashMap<String, String>,
    var_bindings: &HashMap<String, HashSet<Effect>>,
    out: &mut String,
) {
    out.push('[');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        match a {
            GenericArg::Type(t) => format_type_expr_with_subs(t, type_subs, var_bindings, out),
            GenericArg::Const(_) => out.push_str("/* const */"),
        }
    }
    out.push(']');
}

/// Render an `EffectList` as a sorted vector of `verb(resource)` strings,
/// resolving any `Variable(E)` items via `var_bindings`. Pure / unresolved
/// produces an empty vector (the slot prints as `with []`).
pub(crate) fn resolve_effect_list_for_render(
    list: &EffectList,
    var_bindings: &HashMap<String, HashSet<Effect>>,
) -> Vec<String> {
    let mut effects: Vec<String> = Vec::new();
    for item in &list.items {
        match item {
            EffectItem::Verb(verb) => {
                for resource in &verb.resources {
                    effects.push(format!(
                        "{}({})",
                        verb_name(&verb.kind),
                        resource.path.join(".")
                    ));
                }
            }
            EffectItem::Group(name) => effects.push(format!("({name})")),
            EffectItem::Polymorphic => effects.push("_".to_string()),
            EffectItem::Variable(name) => {
                if let Some(set) = var_bindings.get(name) {
                    for e in set {
                        effects.push(format!("{}({})", verb_name(&e.verb), e.resource));
                    }
                } else {
                    // Unbound variable — render the variable name itself.
                    effects.push(name.clone());
                }
            }
        }
    }
    effects.sort();
    effects.dedup();
    effects
}

/// Tarjan's algorithm for finding strongly connected components.
/// Returns SCCs in reverse topological order; each SCC is a Vec of function names.
pub(crate) fn tarjan_scc(
    nodes: &HashSet<String>,
    graph: &HashMap<String, Vec<(String, Span)>>,
) -> Vec<Vec<String>> {
    struct TarjanState {
        index_counter: usize,
        stack: Vec<String>,
        on_stack: HashSet<String>,
        index: HashMap<String, usize>,
        lowlink: HashMap<String, usize>,
        result: Vec<Vec<String>>,
    }

    fn strongconnect(
        v: &str,
        graph: &HashMap<String, Vec<(String, Span)>>,
        state: &mut TarjanState,
    ) {
        let idx = state.index_counter;
        state.index_counter += 1;
        state.index.insert(v.to_string(), idx);
        state.lowlink.insert(v.to_string(), idx);
        state.stack.push(v.to_string());
        state.on_stack.insert(v.to_string());

        if let Some(edges) = graph.get(v) {
            for (w, _) in edges {
                if !state.index.contains_key(w) {
                    strongconnect(w, graph, state);
                    let w_low = state.lowlink[w];
                    let v_low = state.lowlink[v];
                    if w_low < v_low {
                        state.lowlink.insert(v.to_string(), w_low);
                    }
                } else if state.on_stack.contains(w) {
                    let w_idx = state.index[w];
                    let v_low = state.lowlink[v];
                    if w_idx < v_low {
                        state.lowlink.insert(v.to_string(), w_idx);
                    }
                }
            }
        }

        if state.lowlink[v] == state.index[v] {
            let mut scc = Vec::new();
            loop {
                let w = state.stack.pop().unwrap();
                state.on_stack.remove(&w);
                scc.push(w.clone());
                if w == v {
                    break;
                }
            }
            scc.sort(); // deterministic ordering
            state.result.push(scc);
        }
    }

    let mut state = TarjanState {
        index_counter: 0,
        stack: Vec::new(),
        on_stack: HashSet::new(),
        index: HashMap::new(),
        lowlink: HashMap::new(),
        result: Vec::new(),
    };

    // Process nodes in sorted order for determinism
    let mut sorted_nodes: Vec<&String> = nodes.iter().collect();
    sorted_nodes.sort();

    for node in sorted_nodes {
        if !state.index.contains_key(node.as_str()) {
            strongconnect(node, graph, &mut state);
        }
    }

    state.result
}

/// Check if two effect verbs on the SAME resource conflict.
pub(crate) fn effects_conflict(a: &EffectVerbKind, b: &EffectVerbKind) -> bool {
    use EffectVerbKind::*;
    match (a, b) {
        // reads + reads = safe
        (Reads, Reads) => false,
        // reads + writes = conflict
        (Reads, Writes) | (Writes, Reads) => true,
        // writes + writes = conflict
        (Writes, Writes) => true,
        // sends + sends = safe
        (Sends, Sends) => false,
        // sends + receives = safe (full-duplex I/O semantics)
        (Sends, Receives) | (Receives, Sends) => false,
        // receives + receives = safe
        (Receives, Receives) => false,
        // All other combinations: safe (allocates, panics are informational;
        // cross-category like reads+sends are independent concerns)
        _ => false,
    }
}
