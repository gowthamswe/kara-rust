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
    program: &'a Program,
    /// Public-function effect declaration policy (see `PublicEffectsPolicy`).
    public_effects_policy: PublicEffectsPolicy,
    /// Expanded effect groups: group name → EffectSet.
    expanded_groups: HashMap<String, EffectSet>,
    /// Active compile profile — determines which effects are forbidden at extern sites.
    profile: CompileProfile,
    /// Transparent effect verb names.
    transparent_effects: HashSet<String>,
    /// Declared effects per function name.
    declared_effects: HashMap<String, DeclaredEffects>,
    /// Inferred effects per function name.
    inferred_effects: HashMap<String, EffectSet>,
    /// Whether each function is public.
    function_visibility: HashMap<String, bool>,
    /// Function spans for error reporting.
    function_spans: HashMap<String, Span>,
    /// Functions and their AST bodies (for inference).
    function_bodies: HashMap<String, Function>,
    /// Impl method bodies: "TypeName.method" → Function
    method_bodies: HashMap<String, Function>,
    /// Functions that call polymorphic (`with _`) callees.
    calls_polymorphic: HashSet<String>,
    /// Functions that explicitly declare `with _` (anonymous polymorphism)
    /// — distinct from `with E` (named) declarations. The viral rule fires
    /// only for `with _` callees: `with E` is resolved at the call site
    /// against concrete bindings, so it does not "leak" through callers
    /// that lack a `with _` of their own.
    fn_uses_with_underscore: HashSet<String>,
    /// Per-function generic-parameter bounds index: function key (e.g.
    /// `"sort"` or `"Wrapper.default"`) → param name → bounds. Populated
    /// once before inference; consulted by `extract_trait_assoc_fn_keys`
    /// to redirect `T.method()` and bare `method()` calls to the matching
    /// `Trait.method` ceiling key.
    fn_bounds_index: HashMap<String, HashMap<String, Vec<TraitBound>>>,
    /// Per-function effect-variable position index: function key → effect
    /// variable name → list of parameter indices whose `Fn(...) with E`
    /// type references that variable. Populated after `collect_function_info`;
    /// consulted by `check_with_e_unification` to enforce single-assignment
    /// of `E` across multiple `with E` slots in one signature
    /// (design.md:315–317). A variable that appears at only one position
    /// adds no constraint beyond the existing `with _` polymorphic behavior;
    /// a variable at 2+ positions requires every closure argument's effect
    /// set to agree, with a conflict diagnostic otherwise.
    fn_effect_var_positions: HashMap<String, HashMap<String, Vec<usize>>>,
    /// Method-call → resolved `Type.method` key, populated by the typechecker
    /// (`TypeCheckResult.method_callee_types`). Used by `MethodCall` arms in
    /// `collect_calls_in_expr` (function-reference arg propagation), in
    /// `check_with_e_in_expr` (`with E` unification), and in
    /// `check_subtyping_in_expr_owned` (Fn-slot subtyping) to resolve the
    /// callee precisely instead of falling back to per-method-name heuristics.
    /// Empty when constructed via the unparameterised `new` family.
    method_callee_types: HashMap<SpanKey, String>,
    /// Per-call-site type-parameter substitutions, populated by the
    /// typechecker (`TypeCheckResult.call_type_subs`). Maps a call-expression
    /// span to a `param_name → resolved_type_name` table — concrete entries
    /// (`"Vec[i64]"`) come from the typechecker's solver. Round 10.3 step 7
    /// reads this to render a fully-monomorphized callee signature in E0404
    /// diagnostics, so the user sees `Fn(i64) -> ()` instead of `Fn(T) -> ()`
    /// when a generic call's effect-subtyping check fails. Empty when
    /// constructed via the unparameterised `new` family.
    call_type_subs: HashMap<SpanKey, HashMap<String, String>>,
    errors: Vec<EffectError>,
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
                Item::ExternFunction(e) => {
                    // ABI-keyed default effect set (trust-not-verify: extern has no body).
                    // `extern "C"` → {blocks}; `extern "C-unwind"` → {blocks, panics}.
                    // `@noblock` removes blocks from the default (e.g. a pure-CPU C++ fn).
                    let builtin_span = Span {
                        line: 0,
                        column: 0,
                        offset: 0,
                        length: 0,
                    };
                    let has_noblock = e.attributes.iter().any(|a| a.name == "noblock");
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
                            if let Some(forbidden_reason) =
                                self.profile_forbids(&te.effect, &e.name, &e.abi)
                            {
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
                    if let Some(DeclaredEffects::Explicit(ref set)) =
                        self.declared_effects.get(&e.name)
                    {
                        self.inferred_effects.insert(e.name.clone(), set.clone());
                    } else {
                        self.inferred_effects
                            .insert(e.name.clone(), EffectSet::new());
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

    // ── Phase B: Inference ──────────────────────────────────────

    fn infer_effects(&mut self) {
        // Initialize all functions with empty effect sets (except externs, already done)
        for name in self.function_bodies.keys().cloned().collect::<Vec<_>>() {
            self.inferred_effects.entry(name).or_default();
        }
        for name in self.method_bodies.keys().cloned().collect::<Vec<_>>() {
            self.inferred_effects.entry(name).or_default();
        }

        // Build the call graph and decompose into SCCs (Tarjan's algorithm).
        // tarjan_scc returns SCCs in reverse topological order — leaves first,
        // roots last — so processing them in order propagates callee effects
        // into callers before callers are processed.
        let call_graph = self.build_call_graph();
        let all_fn_names: HashSet<String> = self
            .function_bodies
            .keys()
            .chain(self.method_bodies.keys())
            .cloned()
            .collect();
        let sccs = tarjan_scc(&all_fn_names, &call_graph);

        for scc in &sccs {
            if scc.len() == 1 {
                // Non-recursive: one pass is sufficient because all callees
                // (in earlier SCCs) are already fully resolved.
                let name = &scc[0];
                let body = self
                    .function_bodies
                    .get(name)
                    .map(|f| f.body.clone())
                    .or_else(|| self.method_bodies.get(name).map(|f| f.body.clone()));
                if let Some(body) = body {
                    self.infer_function_effects(name, &body);
                }
            } else {
                // Mutually-recursive SCC: iterate until convergence.
                // Effects are monotone, so at most |SCC| passes are needed
                // (each pass can propagate effects one hop around the cycle).
                for _ in 0..=scc.len() {
                    let mut changed = false;
                    for name in scc {
                        let body = self
                            .function_bodies
                            .get(name)
                            .map(|f| f.body.clone())
                            .or_else(|| self.method_bodies.get(name).map(|f| f.body.clone()));
                        if let Some(body) = body {
                            if self.infer_function_effects(name, &body) {
                                changed = true;
                            }
                        }
                    }
                    if !changed {
                        break;
                    }
                }
            }
        }
    }

    /// After the SCC inference pass, update `declared_effects` for private trait
    /// methods that have no explicit ceiling to the union of all impl inferred
    /// effects.  This makes the formal ceiling correct (spec compliance) and lets
    /// `verify_impl_trait_ceilings` use a real set instead of skipping with `None`.
    ///
    /// Public traits and methods that already carry an explicit `with` clause are
    /// left untouched — their declared ceilings are authoritative.
    fn infer_private_trait_ceilings(&mut self) {
        // Build (trait_name, method_name) → [impl_type_name] from all impl blocks.
        let mut trait_impl_types: HashMap<(String, String), Vec<String>> = HashMap::new();
        for item in &self.program.items {
            let imp = match item {
                Item::ImplBlock(i) => i,
                _ => continue,
            };
            let trait_name = match &imp.trait_name {
                Some(p) => p.segments.last().cloned().unwrap_or_default(),
                None => continue,
            };
            let type_name = match &imp.target_type.kind {
                TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                _ => continue,
            };
            for impl_item in &imp.items {
                let method = match impl_item {
                    ImplItem::Method(m) => m,
                    ImplItem::AssocType(_) => continue,
                };
                trait_impl_types
                    .entry((trait_name.clone(), method.name.clone()))
                    .or_default()
                    .push(type_name.clone());
            }
        }

        // For each private trait method with no explicit ceiling, union impl effects.
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            let t = match item {
                Item::TraitDef(t) => t,
                _ => continue,
            };
            if t.is_pub {
                continue; // Public traits always use declared ceilings.
            }
            for trait_item in &t.items {
                let m = match trait_item {
                    TraitItem::Method(m) => m,
                    TraitItem::AssocType(_) => continue,
                };
                let trait_key = format!("{}.{}", t.name, m.name);
                // Only infer for methods with no explicit ceiling (None).
                if !matches!(
                    self.declared_effects.get(&trait_key),
                    Some(DeclaredEffects::None) | None
                ) {
                    continue;
                }
                let impl_types = trait_impl_types
                    .get(&(t.name.clone(), m.name.clone()))
                    .cloned()
                    .unwrap_or_default();
                let mut ceiling = EffectSet::new();
                for type_name in &impl_types {
                    let impl_key = format!("{}.{}", type_name, m.name);
                    if let Some(impl_set) = self.inferred_effects.get(&impl_key) {
                        for te in &impl_set.effects {
                            if !ceiling.contains(&te.effect) {
                                ceiling.add(te.effect.clone(), te.origin.clone());
                            }
                        }
                    }
                }
                if !ceiling.is_empty() {
                    self.declared_effects
                        .insert(trait_key, DeclaredEffects::Explicit(ceiling));
                }
            }
        }
    }

    /// Build a call graph restricted to user-defined functions and methods.
    /// Edges to builtins and external callees are omitted since they have no
    /// bodies to infer from (their effects are seeded directly into
    /// `inferred_effects` during initialization).
    fn build_call_graph(&self) -> HashMap<String, Vec<(String, Span)>> {
        let all_fn_names: HashSet<String> = self
            .function_bodies
            .keys()
            .chain(self.method_bodies.keys())
            .cloned()
            .collect();
        let mut graph: HashMap<String, Vec<(String, Span)>> = HashMap::new();
        let empty_bounds: HashMap<String, Vec<TraitBound>> = HashMap::new();
        for (name, func) in &self.function_bodies {
            let bounds = self.fn_bounds_index.get(name).unwrap_or(&empty_bounds);
            let relevant = self
                .collect_calls_in_block(&func.body, bounds)
                .into_iter()
                .filter(|(callee, _)| all_fn_names.contains(callee))
                .collect();
            graph.insert(name.clone(), relevant);
        }
        for (name, func) in &self.method_bodies {
            let bounds = self.fn_bounds_index.get(name).unwrap_or(&empty_bounds);
            let relevant = self
                .collect_calls_in_block(&func.body, bounds)
                .into_iter()
                .filter(|(callee, _)| all_fn_names.contains(callee))
                .collect();
            graph.insert(name.clone(), relevant);
        }
        graph
    }

    /// Walk a function body, find all calls, and add callee effects.
    /// Returns true if any new effects were added.
    fn infer_function_effects(&mut self, fn_name: &str, body: &Block) -> bool {
        let empty_bounds: HashMap<String, Vec<TraitBound>> = HashMap::new();
        let bounds = self
            .fn_bounds_index
            .get(fn_name)
            .cloned()
            .unwrap_or(empty_bounds);
        let calls = self.collect_calls_in_block(body, &bounds);
        let mut new_effects = Vec::new();

        for (callee_name, call_span) in &calls {
            // Propagate the polymorphic marker only for callees that use
            // `with _` (anonymous polymorphism). A callee that declares
            // only `with E` (named) resolves its effect variable at the
            // call site against concrete bindings, so it does not "leak"
            // through callers that lack a `with _` of their own.
            let callee_is_poly = self.fn_uses_with_underscore.contains(callee_name)
                || self.calls_polymorphic.contains(callee_name);
            if callee_is_poly {
                self.calls_polymorphic.insert(fn_name.to_string());
            }

            // Look up callee's effects
            let callee_effects = self.get_callee_effects(callee_name);
            for effect in callee_effects {
                new_effects.push((
                    effect.clone(),
                    EffectOrigin::Callee {
                        fn_name: callee_name.clone(),
                        span: call_span.clone(),
                    },
                ));
            }
        }

        let mut changed = false;
        let current = self
            .inferred_effects
            .entry(fn_name.to_string())
            .or_default();
        for (effect, origin) in new_effects {
            if !current.contains(&effect) {
                current.add(effect, origin);
                changed = true;
            }
        }
        changed
    }

    /// Get the effects of a callee function.
    /// For public functions: use declared effects (inference firewall).
    /// For private functions: use inferred effects.
    /// For polymorphic (`with _`) functions: use inferred effects (transparent —
    /// the function's own internal effects are contributed to the caller).
    /// Note: effects from closure arguments are already propagated because
    /// `collect_calls_in_expr` walks into closure bodies at the call site.
    fn get_callee_effects(&self, callee_name: &str) -> Vec<Effect> {
        let is_pub = self
            .function_visibility
            .get(callee_name)
            .copied()
            .unwrap_or(false);

        if is_pub {
            // Use declared effects
            match self.declared_effects.get(callee_name) {
                Some(DeclaredEffects::Explicit(set)) => set.effect_set().into_iter().collect(),
                Some(DeclaredEffects::Polymorphic) => {
                    // `with _` — transparent: use the callee's inferred effects so its
                    // own fixed effects (allocations, I/O, etc.) propagate to the caller.
                    // Closure-argument effects are handled separately by the caller's
                    // body scan (collect_calls_in_expr walks into closure bodies).
                    match self.inferred_effects.get(callee_name) {
                        Some(set) => set.effect_set().into_iter().collect(),
                        None => Vec::new(),
                    }
                }
                Some(DeclaredEffects::PolymorphicWithFixed(fixed)) => {
                    // Mixed declaration (e.g. `with reads(X) + _`): return the fixed
                    // effects plus the callee's inferred effects. Closure-argument effects
                    // are propagated by the caller's body scan.
                    let mut effects: HashSet<Effect> = fixed.effect_set();
                    if let Some(inferred) = self.inferred_effects.get(callee_name) {
                        effects.extend(inferred.effect_set());
                    }
                    effects.into_iter().collect()
                }
                Some(DeclaredEffects::None) | None => Vec::new(), // pure
            }
        } else {
            // Use inferred effects
            match self.inferred_effects.get(callee_name) {
                Some(set) => set.effect_set().into_iter().collect(),
                None => Vec::new(),
            }
        }
    }

    /// Empty bounds map for callers that don't have generic-param context.
    /// Threaded through `collect_calls_in_*` so they can redirect typeparam
    /// dispatch (`T.method()`, bare `method()` against a bound) to the
    /// matching `Trait.method` ceiling key.
    fn collect_calls_in_block(
        &self,
        block: &Block,
        bounds: &HashMap<String, Vec<TraitBound>>,
    ) -> Vec<(String, Span)> {
        let mut calls = Vec::new();
        for stmt in &block.stmts {
            self.collect_calls_in_stmt(stmt, &mut calls, bounds);
        }
        if let Some(ref expr) = block.final_expr {
            self.collect_calls_in_expr(expr, &mut calls, bounds);
        }
        calls
    }

    fn collect_calls_in_stmt(
        &self,
        stmt: &Stmt,
        calls: &mut Vec<(String, Span)>,
        bounds: &HashMap<String, Vec<TraitBound>>,
    ) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => self.collect_calls_in_expr(value, calls, bounds),
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.collect_calls_in_expr(value, calls, bounds);
                let block_calls = self.collect_calls_in_block(else_block, bounds);
                calls.extend(block_calls);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                let block_calls = self.collect_calls_in_block(body, bounds);
                calls.extend(block_calls);
            }
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                self.collect_calls_in_expr(target, calls, bounds);
                self.collect_calls_in_expr(value, calls, bounds);
            }
            StmtKind::Expr(expr) => self.collect_calls_in_expr(expr, calls, bounds),
        }
    }

    /// True if `name` is a `with _` (polymorphic) callee.
    fn is_polymorphic_callee(&self, name: &str) -> bool {
        matches!(
            self.declared_effects.get(name),
            Some(DeclaredEffects::Polymorphic | DeclaredEffects::PolymorphicWithFixed(_))
        )
    }

    /// True if `name` is a user-defined function (not a local variable).
    fn is_user_function(&self, name: &str) -> bool {
        self.function_bodies.contains_key(name) || self.method_bodies.contains_key(name)
    }

    fn collect_calls_in_expr(
        &self,
        expr: &Expr,
        calls: &mut Vec<(String, Span)>,
        bounds: &HashMap<String, Vec<TraitBound>>,
    ) {
        match &expr.kind {
            ExprKind::Call { callee, args } => {
                // Trait-assoc-fn redirect: when the callee dispatches through
                // a typeparam-bound trait, push the resolved `Trait.method`
                // key(s) so the trait's declared ceiling propagates to the
                // caller. Concrete-type dispatch (`Wrapper.method()`) and
                // ordinary free-function calls still use `extract_callee_name`.
                let trait_keys = self.extract_trait_assoc_fn_keys(callee, bounds);
                let callee_name: Option<String> = if !trait_keys.is_empty() {
                    for key in &trait_keys {
                        calls.push((key.clone(), expr.span.clone()));
                    }
                    trait_keys.into_iter().next()
                } else {
                    let n = self.extract_callee_name(callee);
                    if let Some(ref name) = n {
                        calls.push((name.clone(), expr.span.clone()));
                    }
                    n
                };
                self.collect_calls_in_expr(callee, calls, bounds);
                for arg in args {
                    self.collect_calls_in_expr(&arg.value, calls, bounds);
                }
                // When calling a `with _` function (or a private function that
                // transitively calls one), named-function-reference args contribute
                // their effects directly (per-call-site resolution).
                // Inline closure args are already handled by the recursive walk above.
                if let Some(ref cname) = callee_name {
                    if self.is_polymorphic_callee(cname)
                        || self.calls_polymorphic.contains(cname.as_str())
                    {
                        for arg in args {
                            if let ExprKind::Identifier(arg_name) = &arg.value.kind {
                                if self.is_user_function(arg_name) {
                                    calls.push((arg_name.clone(), arg.value.span.clone()));
                                }
                            }
                        }
                    }
                }
            }
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                // `Self.method()` inside a trait default body dispatches
                // through the trait's supertraits; redirect to the matching
                // `Trait.method` ceiling key. Bounds for Self are populated
                // by `trait_method_bounds` (supertraits as Self bounds).
                if matches!(object.kind, ExprKind::SelfType) {
                    if let Some(bs) = bounds.get("Self") {
                        for b in bs {
                            if let Some(t) = b.path.last() {
                                if self.trait_declares_no_self_method(t, method) {
                                    calls.push((format!("{}.{}", t, method), expr.span.clone()));
                                }
                            }
                        }
                    }
                }
                // For method calls, we'd need type info to know the exact method.
                // For now, search all impl methods with matching name.
                for key in self.method_bodies.keys() {
                    if key.ends_with(&format!(".{}", method)) {
                        calls.push((key.clone(), expr.span.clone()));
                    }
                }
                // unwrap() and expect() always panic on None/Err (F-057).
                if method == "unwrap" {
                    calls.push(("__builtin_unwrap".to_string(), expr.span.clone()));
                } else if method == "expect" {
                    calls.push(("__builtin_expect".to_string(), expr.span.clone()));
                }
                // Lowercase stdlib module aliases routed through `MethodCall`
                // syntax (`env.set(...)`, etc.). The parser produces a
                // `MethodCall` with `object = Identifier("env")` because the
                // module name is value-class — `Env.set(...)` would parse as
                // `Call(Path([Env, set]))` and route through `extract_callee_name`
                // above, but the lowercase form does not. Mirror the
                // typechecker's `infer_method_call` lowercase-module branch
                // (line ~8725) and the interpreter's `eval_method_call` alias
                // map by emitting the capitalized `Env.<method>` call key here
                // so seeded `inferred_effects` flow to the caller.
                if let ExprKind::Identifier(mod_name) = &object.kind {
                    if mod_name == "env" {
                        calls.push((format!("Env.{}", method), expr.span.clone()));
                    }
                }
                // Stdlib methods whose effects are pre-seeded in inferred_effects.
                // Matched by method name (conservatively — no receiver type info here).
                // Conservative over-approximation is acceptable; false negatives are not.
                const STDLIB_METHOD_MAP: &[(&str, &str)] = &[
                    ("push", "Vec.push"),
                    ("push_str", "String.push_str"),
                    ("insert", "Map.insert"),
                    ("insert", "SortedSet.insert"),
                    ("insert", "Set.insert"),
                    ("try_insert", "Map.try_insert"),
                    ("entry", "Map.entry"),
                    ("extend", "Map.extend"),
                    ("merge", "Map.merge"),
                    ("keys", "Map.keys"),
                    ("values", "Map.values"),
                    ("entries", "Map.entries"),
                    ("clone", "Map.clone"),
                    ("clone", "Set.clone"),
                    ("union", "Set.union"),
                    ("intersection", "Set.intersection"),
                    ("difference", "Set.difference"),
                    ("send", "Sender.send"),
                    ("recv", "Receiver.recv"),
                    ("chunk_by", "Iterator.chunk_by"),
                    ("chunks", "Iterator.chunks"),
                    ("windows", "Iterator.windows"),
                ];
                for &(method_name, qualified) in STDLIB_METHOD_MAP {
                    if method.as_str() == method_name {
                        calls.push((qualified.to_string(), expr.span.clone()));
                    }
                }
                // Function-reference argument propagation, mirror of the
                // `Call` branch above. When the resolved `Type.method` callee
                // is `with _`-polymorphic (or transitively calls a `with _`
                // function), every named-fn argument contributes its effects
                // directly so the caller's inferred set covers the closure
                // performed via the function reference. Without this, a public
                // method like `Runner.run_each(write_log)` would infer no
                // effects through `write_log` even though `run_each` calls
                // it with `with E`.
                if let Some(callee_key) = self.resolve_method_callee_key(&expr.span) {
                    if self.is_polymorphic_callee(&callee_key)
                        || self.calls_polymorphic.contains(callee_key.as_str())
                    {
                        for arg in args {
                            if let ExprKind::Identifier(arg_name) = &arg.value.kind {
                                if self.is_user_function(arg_name) {
                                    calls.push((arg_name.clone(), arg.value.span.clone()));
                                }
                            }
                        }
                    }
                }
                self.collect_calls_in_expr(object, calls, bounds);
                for arg in args {
                    self.collect_calls_in_expr(&arg.value, calls, bounds);
                }
            }
            ExprKind::Binary { left, op, right } => {
                self.collect_calls_in_expr(left, calls, bounds);
                self.collect_calls_in_expr(right, calls, bounds);
                if matches!(op, BinOp::Div | BinOp::Mod) {
                    calls.push(("__builtin_div_rem".to_string(), expr.span.clone()));
                }
            }
            ExprKind::Pipe { left, right } => {
                self.collect_calls_in_expr(left, calls, bounds);
                self.collect_calls_in_expr(right, calls, bounds);
            }
            ExprKind::Unary { operand, .. } => {
                self.collect_calls_in_expr(operand, calls, bounds);
            }
            ExprKind::Block(block) => {
                for stmt in &block.stmts {
                    self.collect_calls_in_stmt(stmt, calls, bounds);
                }
                if let Some(ref e) = block.final_expr {
                    self.collect_calls_in_expr(e, calls, bounds);
                }
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.collect_calls_in_expr(condition, calls, bounds);
                let block_calls = self.collect_calls_in_block(then_block, bounds);
                calls.extend(block_calls);
                if let Some(ref e) = else_branch {
                    self.collect_calls_in_expr(e, calls, bounds);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.collect_calls_in_expr(value, calls, bounds);
                let block_calls = self.collect_calls_in_block(then_block, bounds);
                calls.extend(block_calls);
                if let Some(ref e) = else_branch {
                    self.collect_calls_in_expr(e, calls, bounds);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.collect_calls_in_expr(scrutinee, calls, bounds);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.collect_calls_in_expr(guard, calls, bounds);
                    }
                    self.collect_calls_in_expr(&arm.body, calls, bounds);
                }
            }
            ExprKind::While {
                condition, body, ..
            }
            | ExprKind::For {
                iterable: condition,
                body,
                ..
            } => {
                self.collect_calls_in_expr(condition, calls, bounds);
                let block_calls = self.collect_calls_in_block(body, bounds);
                calls.extend(block_calls);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.collect_calls_in_expr(value, calls, bounds);
                let block_calls = self.collect_calls_in_block(body, bounds);
                calls.extend(block_calls);
            }
            ExprKind::Loop { body, .. }
            | ExprKind::Unsafe(body)
            | ExprKind::Try(body)
            | ExprKind::Seq(body)
            | ExprKind::Par(body) => {
                let block_calls = self.collect_calls_in_block(body, bounds);
                calls.extend(block_calls);
            }
            ExprKind::LabeledBlock { body, .. } => {
                let block_calls = self.collect_calls_in_block(body, bounds);
                calls.extend(block_calls);
            }
            ExprKind::Lock { body, .. } => {
                let block_calls = self.collect_calls_in_block(body, bounds);
                calls.extend(block_calls);
            }
            ExprKind::Closure { body, .. } => {
                self.collect_calls_in_expr(body, calls, bounds);
            }
            ExprKind::Return(Some(inner)) | ExprKind::Question(inner) => {
                self.collect_calls_in_expr(inner, calls, bounds);
            }
            ExprKind::Break {
                value: Some(inner), ..
            } => {
                self.collect_calls_in_expr(inner, calls, bounds);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.collect_calls_in_expr(object, calls, bounds);
            }
            ExprKind::Index { object, index } => {
                self.collect_calls_in_expr(object, calls, bounds);
                self.collect_calls_in_expr(index, calls, bounds);
                // Indexing with [] calls Index::index which has panics effect
                // (can panic on out-of-bounds). Use .get() for fallible access.
                calls.push(("__builtin_index".to_string(), expr.span.clone()));
            }
            ExprKind::Tuple(exprs) => {
                for e in exprs {
                    self.collect_calls_in_expr(e, calls, bounds);
                }
            }
            ExprKind::MapLiteral(entries) => {
                for (key, val) in entries {
                    self.collect_calls_in_expr(key, calls, bounds);
                    self.collect_calls_in_expr(val, calls, bounds);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.collect_calls_in_expr(&f.value, calls, bounds);
                }
                if let Some(ref s) = spread {
                    self.collect_calls_in_expr(s, calls, bounds);
                }
            }
            ExprKind::Cast { expr: inner, .. } => {
                self.collect_calls_in_expr(inner, calls, bounds);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.collect_calls_in_expr(s, calls, bounds);
                }
                if let Some(e) = end {
                    self.collect_calls_in_expr(e, calls, bounds);
                }
            }
            ExprKind::Path { segments, .. } => {
                // A path like Foo::bar used as a value — could be a function reference
                if segments.len() == 2 {
                    let key = format!("{}.{}", segments[0], segments[1]);
                    if self.function_bodies.contains_key(&key)
                        || self.method_bodies.contains_key(&key)
                    {
                        calls.push((key, expr.span.clone()));
                    }
                }
            }
            ExprKind::OptionalChain { object, args, .. } => {
                self.collect_calls_in_expr(object, calls, bounds);
                if let Some(args) = args {
                    for arg in args {
                        self.collect_calls_in_expr(&arg.value, calls, bounds);
                    }
                }
            }
            ExprKind::NilCoalesce { left, right } => {
                self.collect_calls_in_expr(left, calls, bounds);
                self.collect_calls_in_expr(right, calls, bounds);
            }
            ExprKind::ArrayLiteral(elements) => {
                for e in elements {
                    self.collect_calls_in_expr(e, calls, bounds);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.collect_calls_in_expr(value, calls, bounds);
                self.collect_calls_in_expr(count, calls, bounds);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.collect_calls_in_expr(e, calls, bounds);
                }
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.collect_calls_in_expr(&b.value, calls, bounds);
                }
                let block_calls = self.collect_calls_in_block(body, bounds);
                calls.extend(block_calls);
            }
            // Leaf expressions — no calls to collect
            ExprKind::Identifier(_)
            | ExprKind::SelfValue
            | ExprKind::SelfType
            | ExprKind::Integer(_, _)
            | ExprKind::Float(_, _)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_)
            | ExprKind::InterpolatedStringLit(_)
            | ExprKind::Bool(_)
            | ExprKind::Continue { .. }
            | ExprKind::Return(None)
            | ExprKind::Break { value: None, .. }
            | ExprKind::PipePlaceholder
            | ExprKind::Error => {}
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

    // ── Verification ────────────────────────────────────────────

    fn verify_declarations(&mut self) {
        let fn_names: Vec<String> = self.function_bodies.keys().cloned().collect();
        for name in &fn_names {
            let is_pub = self.function_visibility.get(name).copied().unwrap_or(false);
            if !is_pub {
                continue;
            } // Private functions don't need declarations

            let declared = self.declared_effects.get(name);
            let inferred = self.inferred_effects.get(name);
            let span = self.function_spans.get(name).cloned().unwrap_or(Span {
                line: 0,
                column: 0,
                offset: 0,
                length: 0,
            });

            match declared {
                Some(DeclaredEffects::Polymorphic) => {
                    // `with _` (anonymous, viral): body may carry any effects —
                    // the wildcard absorbs whatever the closure brings, plus any
                    // concrete operations the body performs. Skip verification.
                    //
                    // `with E` (named, precise): body's concrete effects must
                    // come from E only (i.e. via the polymorphic parameter).
                    // The single shared E across a polymorphic SCC means any
                    // concrete leak in one member propagates to all via the
                    // fixed-point and surfaces here for every leaking member.
                    if self.fn_uses_with_underscore.contains(name) {
                        continue;
                    }
                    if let Some(inferred_set) = inferred {
                        for te in &inferred_set.effects {
                            if self.is_transparent_verb(&te.effect.verb) {
                                continue;
                            }
                            // Effects whose origin is a polymorphic callee
                            // (declared `with _` / `with E`, or transitively
                            // poly via `calls_polymorphic`) are contributed
                            // through E, not as concrete body leaks. Trait
                            // dispatch through a typeparam bound (e.g.,
                            // `T.method()` where `T: Processor` and
                            // `Processor.method` is `with _`) routes through
                            // this branch — the design.md `run[T: Processor,
                            // with E]` example would otherwise false-positive.
                            if self.effect_came_via_polymorphic_callee(te, name) {
                                continue;
                            }
                            let origin_msg = self.format_effect_origin(name, &te.effect);
                            self.errors.push(EffectError {
                                message: format!(
                                    "public function '{}' is declared `with E` (purely \
                                     polymorphic) but performs {}({}){}; add it to the \
                                     declaration as `with E {}({})` or remove the call",
                                    name,
                                    verb_name(&te.effect.verb),
                                    te.effect.resource,
                                    origin_msg,
                                    verb_name(&te.effect.verb),
                                    te.effect.resource,
                                ),
                                span: span.clone(),
                                kind: EffectErrorKind::MissingEffectDeclaration,
                                subtype_trace: None,
                            });
                        }
                    }
                    continue;
                }
                Some(DeclaredEffects::PolymorphicWithFixed(fixed)) => {
                    // `with _ + fixed`: any `_` makes the declaration viral —
                    // body may carry effects beyond `fixed`. Skip.
                    //
                    // `with E + fixed`: body's concrete effects must be ⊆ fixed
                    // (E is symbolic and resolves at the call site; only the
                    // fixed part licenses concrete body effects).
                    if self.fn_uses_with_underscore.contains(name) {
                        continue;
                    }
                    let fixed_set = fixed.effect_set();
                    if let Some(inferred_set) = inferred {
                        for te in &inferred_set.effects {
                            if self.is_transparent_verb(&te.effect.verb) {
                                continue;
                            }
                            // Same poly-origin filter as the pure `with E`
                            // arm above — effects propagated through a
                            // polymorphic callee belong to E, not to the
                            // fixed part of the declaration.
                            if self.effect_came_via_polymorphic_callee(te, name) {
                                continue;
                            }
                            if !fixed_set.contains(&te.effect) {
                                let origin_msg = self.format_effect_origin(name, &te.effect);
                                self.errors.push(EffectError {
                                    message: format!(
                                        "public function '{}' performs {}({}){} but it is not \
                                         in the fixed part of its `with E ...` declaration; \
                                         add {}({}) to the declaration",
                                        name,
                                        verb_name(&te.effect.verb),
                                        te.effect.resource,
                                        origin_msg,
                                        verb_name(&te.effect.verb),
                                        te.effect.resource,
                                    ),
                                    span: span.clone(),
                                    kind: EffectErrorKind::MissingEffectDeclaration,
                                    subtype_trace: None,
                                });
                            }
                        }
                    }
                    continue;
                }
                Some(DeclaredEffects::Explicit(declared_set)) => {
                    if let Some(inferred_set) = inferred {
                        let declared_effects = declared_set.effect_set();
                        let inferred_effects = inferred_set.effect_set();

                        // Check for missing declarations
                        for effect in &inferred_effects {
                            // Skip transparent effects
                            if self.is_transparent_verb(&effect.verb) {
                                continue;
                            }
                            if !declared_effects.contains(effect) {
                                let origin_msg = self.format_effect_origin(name, effect);
                                self.errors.push(EffectError {
                                    message: format!(
                                        "public function '{}' performs {}({}) but does not declare it{}",
                                        name,
                                        verb_name(&effect.verb),
                                        effect.resource,
                                        origin_msg,
                                    ),
                                    span: span.clone(),
                                    kind: EffectErrorKind::MissingEffectDeclaration,
                                    subtype_trace: None,
                                });
                            }
                        }

                        // Check for over-declarations
                        for effect in &declared_effects {
                            if self.is_transparent_verb(&effect.verb) {
                                continue;
                            }
                            if !inferred_effects.contains(effect) {
                                self.errors.push(EffectError {
                                    message: format!(
                                        "public function '{}' declares {}({}) but does not perform it",
                                        name,
                                        verb_name(&effect.verb),
                                        effect.resource,
                                    ),
                                    span: span.clone(),
                                    kind: EffectErrorKind::OverDeclaredEffect,
                                    subtype_trace: None,
                                });
                            }
                        }
                    }
                }
                Some(DeclaredEffects::None) | None => {
                    // Under `public_effects = "inferred"`, a pub fn may omit the
                    // declaration entirely — effects are inferred from the body
                    // the same way private functions are treated. If the author
                    // does write an explicit `with ...` clause, the other match
                    // arms above still verify it; this arm only governs the
                    // no-declaration case.
                    //
                    // Intentionally NOT `continue`-ing here, even under Inferred
                    // policy. The `with _` viral rule checked below applies regardless
                    // of policy: calling a polymorphic callee always requires `with _`.
                    if self.public_effects_policy != PublicEffectsPolicy::Inferred {
                        // Under Declared policy, require explicit effect annotations.
                        if let Some(inferred_set) = inferred {
                            let non_transparent: Vec<&TracedEffect> = inferred_set
                                .effects
                                .iter()
                                .filter(|e| !self.is_transparent_verb(&e.effect.verb))
                                .collect();
                            if !non_transparent.is_empty() {
                                let effects_list: Vec<String> = non_transparent
                                    .iter()
                                    .map(|e| {
                                        format!(
                                            "{}({})",
                                            verb_name(&e.effect.verb),
                                            e.effect.resource
                                        )
                                    })
                                    .collect();
                                self.errors.push(EffectError {
                                    message: format!(
                                        "public function '{}' performs effects [{}] but has no \
                                         effect declaration. Add: {} to the function signature",
                                        name,
                                        effects_list.join(", "),
                                        effects_list.join(", "),
                                    ),
                                    span: span.clone(),
                                    kind: EffectErrorKind::MissingEffectDeclaration,
                                    subtype_trace: None,
                                });
                            }
                        }
                    }
                }
            }
            // For any public fn not already declaring `with _` (those arms `continue`d),
            // require `with _` if it calls a polymorphic callee — regardless of whether
            // it has explicit effects, no declaration, or is under Inferred policy.
            if self.calls_polymorphic.contains(name) {
                self.errors.push(EffectError {
                    message: format!(
                        "public function '{}' calls a polymorphic (`with _`) function but does \
                         not declare `with _`. Add `with _` to propagate closure effects.",
                        name,
                    ),
                    span: span.clone(),
                    kind: EffectErrorKind::MissingEffectDeclaration,
                    subtype_trace: None,
                });
            }
        }
    }

    /// For every `impl Trait for Type` block, verify that each impl method's
    /// inferred effect set is a subset of the trait method's declared ceiling.
    ///
    /// - `DeclaredEffects::Explicit(set)` ceiling → inferred must be ⊆ set.
    /// - `Polymorphic` / `PolymorphicWithFixed` / `None` / missing key → no check
    ///   (wildcard or unbound ceiling means impls are free).
    fn verify_impl_trait_ceilings(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        let mut new_errors: Vec<EffectError> = Vec::new();

        for item in &items {
            let imp = match item {
                Item::ImplBlock(imp) => imp,
                _ => continue,
            };
            // Only `impl Trait for Type` — inherent impls have no trait ceiling.
            let trait_name = match &imp.trait_name {
                Some(path) => path.segments.last().cloned().unwrap_or_default(),
                None => continue,
            };
            let type_name = match &imp.target_type.kind {
                TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                _ => continue,
            };

            for impl_item in &imp.items {
                let method = match impl_item {
                    ImplItem::Method(m) => m,
                    ImplItem::AssocType(_) => continue,
                };

                let impl_key = format!("{}.{}", type_name, method.name);
                let trait_key = format!("{}.{}", trait_name, method.name);

                // Look up the trait method's declared ceiling.
                let ceiling_set = match self.declared_effects.get(&trait_key) {
                    Some(DeclaredEffects::Explicit(set)) => set.effect_set(),
                    // Polymorphic, PolymorphicWithFixed, None, or unknown trait → free.
                    _ => continue,
                };

                let inferred = match self.inferred_effects.get(&impl_key) {
                    Some(s) => s.clone(),
                    None => continue,
                };

                let mut ceiling_display: Vec<String> = ceiling_set
                    .iter()
                    .filter(|e| !self.is_transparent_verb(&e.verb))
                    .map(|e| format!("{}({})", verb_name(&e.verb), e.resource))
                    .collect();
                ceiling_display.sort();
                let ceiling_str = if ceiling_display.is_empty() {
                    "pure (no effects)".to_string()
                } else {
                    format!("[{}]", ceiling_display.join(", "))
                };

                for te in &inferred.effects {
                    if self.is_transparent_verb(&te.effect.verb) {
                        continue;
                    }
                    if !ceiling_set.contains(&te.effect) {
                        new_errors.push(EffectError {
                            message: format!(
                                "impl method '{}.{}' performs {}({}) but trait '{}.{}' \
                                 ceiling is {}; narrow the impl or widen the trait ceiling",
                                type_name,
                                method.name,
                                verb_name(&te.effect.verb),
                                te.effect.resource,
                                trait_name,
                                method.name,
                                ceiling_str,
                            ),
                            span: method.span.clone(),
                            kind: EffectErrorKind::ImplExceedsTraitCeiling,
                            subtype_trace: None,
                        });
                    }
                }
            }
        }
        self.errors.extend(new_errors);
    }

    /// For every trait method that has a default body, verify that the body's
    /// inferred effect set is a subset of the method's declared ceiling.
    ///
    /// The trait author cannot smuggle effects into callers by hiding them in a
    /// default body without declaring them on the method's `with` clause.
    ///
    /// - `DeclaredEffects::Explicit(ceiling)` → inferred must be ⊆ ceiling.
    /// - `Polymorphic` / `PolymorphicWithFixed` / `None` / missing key → no check.
    fn verify_trait_default_bodies(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        let mut new_errors: Vec<EffectError> = Vec::new();

        for item in &items {
            let t = match item {
                Item::TraitDef(t) => t,
                _ => continue,
            };
            for trait_item in &t.items {
                let m = match trait_item {
                    TraitItem::Method(m) => m,
                    TraitItem::AssocType(_) => continue,
                };
                if m.body.is_none() {
                    continue;
                }

                let key = format!("{}.{}", t.name, m.name);

                let ceiling_set = match self.declared_effects.get(&key) {
                    Some(DeclaredEffects::Explicit(set)) => set.effect_set(),
                    _ => continue,
                };

                let inferred = match self.inferred_effects.get(&key) {
                    Some(s) => s.clone(),
                    None => continue,
                };

                let mut ceiling_display: Vec<String> = ceiling_set
                    .iter()
                    .filter(|e| !self.is_transparent_verb(&e.verb))
                    .map(|e| format!("{}({})", verb_name(&e.verb), e.resource))
                    .collect();
                ceiling_display.sort();
                let ceiling_str = if ceiling_display.is_empty() {
                    "pure (no effects)".to_string()
                } else {
                    format!("[{}]", ceiling_display.join(", "))
                };

                for te in &inferred.effects {
                    if self.is_transparent_verb(&te.effect.verb) {
                        continue;
                    }
                    if !ceiling_set.contains(&te.effect) {
                        new_errors.push(EffectError {
                            message: format!(
                                "default body of '{}.{}' performs {}({}) but the method \
                                 ceiling is {}; declare the effect on the method or remove \
                                 it from the default body",
                                t.name,
                                m.name,
                                verb_name(&te.effect.verb),
                                te.effect.resource,
                                ceiling_str,
                            ),
                            span: m.span.clone(),
                            kind: EffectErrorKind::TraitDefaultExceedsCeiling,
                            subtype_trace: None,
                        });
                    }
                }
            }
        }
        self.errors.extend(new_errors);
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

    /// Walk every function and method body and check that function-valued arguments
    /// passed to `Fn` slots satisfy A ⊆ B, where A is the argument's effect set
    /// and B is the slot's declared effect set.
    ///
    /// - `with _` (Polymorphic) → always passes.
    /// - `with [effects...]` (Specific) → B = declared effects.
    /// - unannotated → B = ∅ (pure); any effect on the argument is an E0404.
    fn check_call_site_subtyping(&mut self) {
        let bodies: Vec<Block> = self
            .function_bodies
            .values()
            .map(|f| f.body.clone())
            .chain(self.method_bodies.values().map(|f| f.body.clone()))
            .collect();
        for body in bodies {
            self.check_subtyping_in_block_owned(body);
        }
    }

    fn check_subtyping_in_block_owned(&mut self, block: Block) {
        for stmt in block.stmts {
            self.check_subtyping_in_stmt_owned(stmt);
        }
        if let Some(expr) = block.final_expr {
            self.check_subtyping_in_expr_owned(*expr);
        }
    }

    fn check_subtyping_in_stmt_owned(&mut self, stmt: Stmt) {
        match stmt.kind {
            StmtKind::Let { value, .. } => self.check_subtyping_in_expr_owned(value),
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.check_subtyping_in_expr_owned(value);
                self.check_subtyping_in_block_owned(else_block);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.check_subtyping_in_block_owned(body);
            }
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                self.check_subtyping_in_expr_owned(target);
                self.check_subtyping_in_expr_owned(value);
            }
            StmtKind::Expr(expr) => self.check_subtyping_in_expr_owned(expr),
        }
    }

    /// Per-argument Fn-slot subtyping check, shared between `Call` and
    /// `MethodCall` arms of `check_subtyping_in_expr_owned`. Resolves the
    /// callee's parameter list (via `function_bodies` or `method_bodies`)
    /// and emits `EffectSubtypeViolation` for any function-valued argument
    /// whose effect set exceeds its slot's declared effects.
    ///
    /// `args` indices align with `params` indices for both call shapes:
    /// method-call `args` exclude the receiver, and `method_bodies[k].params`
    /// excludes `self_param` (it is stored separately).
    ///
    /// `call_span` is the span of the call expression itself (not the args).
    /// Used to look up `call_type_subs` so the diagnostic can render a fully
    /// monomorphized callee signature when the call is generic.
    fn check_call_args_subtyping(&mut self, callee_name: &str, args: &[CallArg], call_span: &Span) {
        let params = self
            .function_bodies
            .get(callee_name)
            .map(|f| f.params.clone())
            .or_else(|| {
                self.method_bodies
                    .get(callee_name)
                    .map(|f| f.params.clone())
            });
        let Some(params) = params else {
            return;
        };
        let return_type = self
            .function_bodies
            .get(callee_name)
            .map(|f| f.return_type.clone())
            .or_else(|| {
                self.method_bodies
                    .get(callee_name)
                    .map(|f| f.return_type.clone())
            })
            .flatten();
        // Per-call bindings for `with E` slots: each named effect variable is
        // resolved to the union of effects supplied at every parameter
        // position that references it. A slot typed `Fn(...) with E` is then
        // checked against this concrete set rather than the empty set.
        // Round 9's unification check separately diagnoses disagreement
        // between positions.
        let var_bindings = self.compute_call_var_bindings(callee_name, args);
        // Look up type-parameter substitutions for this call (Round 10.3
        // step 7). Empty when the callee is non-generic or the typechecker
        // didn't run with `with_call_type_subs` wired in.
        let type_subs = self
            .call_type_subs
            .get(&SpanKey::from_span(call_span))
            .cloned()
            .unwrap_or_default();
        for (i, call_arg) in args.iter().enumerate() {
            let Some(param) = params.get(i) else {
                continue;
            };
            let slot_effects = match &param.ty.kind {
                TypeKind::FnType {
                    effect_spec: Some(EffectSpec::Polymorphic),
                    ..
                } => continue,
                TypeKind::FnType {
                    effect_spec: Some(EffectSpec::Specific(list)),
                    ..
                } => self.resolve_effect_list_to_set(list, Some(&var_bindings)),
                TypeKind::FnType {
                    effect_spec: None, ..
                } => HashSet::new(),
                _ => continue,
            };
            let arg_effects = self.get_arg_effects(&call_arg.value);
            let arg_span = call_arg.value.span.clone();

            // Pre-compute trace fields shared across all E0404 errors for
            // this argument position (slot / argument / offending sets).
            let slot_str: Vec<String> = slot_effects
                .iter()
                .map(|e| format!("{}({})", verb_name(&e.verb), e.resource))
                .collect();
            let arg_str: Vec<String> = arg_effects
                .effects
                .iter()
                .filter(|te| !self.is_transparent_verb(&te.effect.verb))
                .map(|te| format!("{}({})", verb_name(&te.effect.verb), te.effect.resource))
                .collect();
            let offending_str: Vec<String> = arg_effects
                .effects
                .iter()
                .filter(|te| {
                    !self.is_transparent_verb(&te.effect.verb) && !slot_effects.contains(&te.effect)
                })
                .map(|te| format!("{}({})", verb_name(&te.effect.verb), te.effect.resource))
                .collect();

            // Render the monomorphized callee signature (Round 10.3 step 7).
            // Only emitted when the callee has at least one type parameter
            // for which a substitution is known — otherwise it would just be
            // a verbose echo of the source.
            let monomorphized = if type_subs.is_empty() && var_bindings.is_empty() {
                None
            } else {
                Some(format_monomorphized_signature(
                    callee_name,
                    &params,
                    return_type.as_ref(),
                    &type_subs,
                    &var_bindings,
                ))
            };

            for te in &arg_effects.effects {
                let is_transparent = self.is_transparent_verb(&te.effect.verb);
                if !slot_effects.contains(&te.effect) && !is_transparent {
                    let effect_str =
                        format!("{}({})", verb_name(&te.effect.verb), te.effect.resource);
                    let mut message = format!(
                        "argument {} has effect {} not declared in slot [{}]",
                        i + 1,
                        effect_str,
                        if slot_str.is_empty() {
                            "pure".to_string()
                        } else {
                            slot_str.join(", ")
                        },
                    );
                    if let Some(ref sig) = monomorphized {
                        message.push_str(&format!("; callee: {sig}"));
                    }
                    self.errors.push(EffectError {
                        message,
                        span: arg_span.clone(),
                        kind: EffectErrorKind::EffectSubtypeViolation,
                        subtype_trace: Some(EffectSubtypeTrace {
                            slot_effects: slot_str.clone(),
                            argument_effects: arg_str.clone(),
                            offending_effects: offending_str.clone(),
                            monomorphized_signature: monomorphized.clone(),
                        }),
                    });
                }
            }
        }
    }

    fn check_subtyping_in_expr_owned(&mut self, expr: Expr) {
        match expr.kind {
            ExprKind::Call { callee, args } => {
                if let Some(cname) = self.extract_callee_name(&callee) {
                    self.check_call_args_subtyping(&cname, &args, &expr.span);
                }
                // Recurse into callee and args
                self.check_subtyping_in_expr_owned(*callee);
                for arg in args {
                    self.check_subtyping_in_expr_owned(arg.value);
                }
            }
            ExprKind::Block(block) => self.check_subtyping_in_block_owned(block),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.check_subtyping_in_expr_owned(*condition);
                self.check_subtyping_in_block_owned(then_block);
                if let Some(e) = else_branch {
                    self.check_subtyping_in_expr_owned(*e);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.check_subtyping_in_expr_owned(*value);
                self.check_subtyping_in_block_owned(then_block);
                if let Some(e) = else_branch {
                    self.check_subtyping_in_expr_owned(*e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_subtyping_in_expr_owned(*scrutinee);
                for arm in arms {
                    if let Some(g) = arm.guard {
                        self.check_subtyping_in_expr_owned(g);
                    }
                    self.check_subtyping_in_expr_owned(arm.body);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.check_subtyping_in_expr_owned(*condition);
                self.check_subtyping_in_block_owned(body);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.check_subtyping_in_expr_owned(*value);
                self.check_subtyping_in_block_owned(body);
            }
            ExprKind::For { iterable, body, .. } => {
                self.check_subtyping_in_expr_owned(*iterable);
                self.check_subtyping_in_block_owned(body);
            }
            ExprKind::Loop { body, .. }
            | ExprKind::Unsafe(body)
            | ExprKind::Try(body)
            | ExprKind::Seq(body)
            | ExprKind::Par(body) => {
                self.check_subtyping_in_block_owned(body);
            }
            ExprKind::LabeledBlock { body, .. } => self.check_subtyping_in_block_owned(body),
            ExprKind::Lock { body, .. } => self.check_subtyping_in_block_owned(body),
            ExprKind::Closure { body, .. } => self.check_subtyping_in_expr_owned(*body),
            ExprKind::MethodCall { object, args, .. } => {
                // Mirror the `Call` branch: resolve to `Type.method` via the
                // typechecker side-table and run the same per-arg Fn-slot
                // subtyping check. Without this, an effectful closure could
                // satisfy a method's pure `Fn()` slot whenever the enclosing
                // caller declared the effects.
                if let Some(callee_key) = self.resolve_method_callee_key(&expr.span) {
                    self.check_call_args_subtyping(&callee_key, &args, &expr.span);
                }
                self.check_subtyping_in_expr_owned(*object);
                for arg in args {
                    self.check_subtyping_in_expr_owned(arg.value);
                }
            }
            ExprKind::Binary { left, right, .. } => {
                self.check_subtyping_in_expr_owned(*left);
                self.check_subtyping_in_expr_owned(*right);
            }
            ExprKind::Pipe { left, right } => {
                self.check_subtyping_in_expr_owned(*left);
                self.check_subtyping_in_expr_owned(*right);
            }
            ExprKind::Unary { operand, .. } => self.check_subtyping_in_expr_owned(*operand),
            ExprKind::Return(Some(e)) | ExprKind::Question(e) => {
                self.check_subtyping_in_expr_owned(*e)
            }
            ExprKind::Break { value: Some(e), .. } => self.check_subtyping_in_expr_owned(*e),
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.check_subtyping_in_expr_owned(*object)
            }
            ExprKind::Index { object, index } => {
                self.check_subtyping_in_expr_owned(*object);
                self.check_subtyping_in_expr_owned(*index);
            }
            ExprKind::Tuple(exprs) => {
                for e in exprs {
                    self.check_subtyping_in_expr_owned(e);
                }
            }
            ExprKind::ArrayLiteral(elems) => {
                for e in elems {
                    self.check_subtyping_in_expr_owned(e);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.check_subtyping_in_expr_owned(*value);
                self.check_subtyping_in_expr_owned(*count);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.check_subtyping_in_expr_owned(e);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.check_subtyping_in_expr_owned(f.value);
                }
                if let Some(s) = spread {
                    self.check_subtyping_in_expr_owned(*s);
                }
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.check_subtyping_in_expr_owned(k);
                    self.check_subtyping_in_expr_owned(v);
                }
            }
            ExprKind::Cast { expr: inner, .. } => self.check_subtyping_in_expr_owned(*inner),
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.check_subtyping_in_expr_owned(*s);
                }
                if let Some(e) = end {
                    self.check_subtyping_in_expr_owned(*e);
                }
            }
            ExprKind::NilCoalesce { left, right } => {
                self.check_subtyping_in_expr_owned(*left);
                self.check_subtyping_in_expr_owned(*right);
            }
            ExprKind::OptionalChain { object, args, .. } => {
                self.check_subtyping_in_expr_owned(*object);
                if let Some(args) = args {
                    for a in args {
                        self.check_subtyping_in_expr_owned(a.value);
                    }
                }
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.check_subtyping_in_expr_owned(b.value);
                }
                self.check_subtyping_in_block_owned(body);
            }
            ExprKind::InterpolatedStringLit(parts) => {
                for p in parts {
                    if let ParsedInterpolationPart::Expr(e) = p {
                        self.check_subtyping_in_expr_owned(*e);
                    }
                }
            }
            // Leaf expressions — nothing to recurse into
            ExprKind::Identifier(_)
            | ExprKind::Path { .. }
            | ExprKind::SelfValue
            | ExprKind::SelfType
            | ExprKind::Integer(_, _)
            | ExprKind::Float(_, _)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_)
            | ExprKind::Bool(_)
            | ExprKind::Continue { .. }
            | ExprKind::Return(None)
            | ExprKind::Break { value: None, .. }
            | ExprKind::PipePlaceholder
            | ExprKind::Error => {}
        }
    }

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
fn effects_contain_with_underscore(effects: &Option<EffectList>) -> bool {
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
fn effect_var_names_in_type(ty: &TypeExpr) -> Vec<String> {
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

fn verb_name(verb: &EffectVerbKind) -> String {
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
fn format_monomorphized_signature(
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
fn resolve_effect_list_for_render(
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
fn tarjan_scc(
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
fn effects_conflict(a: &EffectVerbKind, b: &EffectVerbKind) -> bool {
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
