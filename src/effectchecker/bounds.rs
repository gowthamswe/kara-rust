//! Generics + trait bounds resolution, plus mutual recursion
//! detection.
//!
//! Houses the helpers that resolve trait-bounded generic dispatch
//! to canonical `Trait.method` ceiling keys:
//!
//! - `extract_callee_name` — recover a callable name from a call
//!   expression (function ident, `Type.method` path, or
//!   bare-method via `extract_trait_assoc_fn_keys`).
//! - `extract_trait_assoc_fn_keys` — redirect `T.method()` and
//!   bare `method()` to matching `Trait.method` ceilings.
//! - `trait_declares_no_self_method` — check whether a trait
//!   declares a method with no `self` parameter.
//! - `fn_generic_bounds`, `impl_method_bounds`, `trait_method_bounds`
//!   — per-fn-shape generic bound extractors.
//! - `build_fn_bounds_index` — per-function bound table keyed by
//!   function name → param name → bounds.
//! - `build_fn_effect_var_positions` — per-function effect-var
//!   position table.
//! - `detect_mutual_recursion_groups` — Tarjan SCC over the call
//!   graph to find mutually-recursive function clusters for
//!   `EffectInferenceCycle` diagnostics.
//!
//! Lives in a sibling `impl<'a> super::EffectChecker<'a>` block.

use std::collections::{HashMap, HashSet};

use crate::ast::*;

use super::{
    effect_var_names_in_type, tarjan_scc, verb_name, MutualRecursionGroup, ResolvedEffect,
};

impl<'a> super::EffectChecker<'a> {
    pub(crate) fn extract_callee_name(&self, callee: &Expr) -> Option<String> {
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
    pub(crate) fn extract_trait_assoc_fn_keys(
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
    pub(crate) fn trait_declares_no_self_method(
        &self,
        trait_name: &str,
        method_name: &str,
    ) -> bool {
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
    pub(crate) fn fn_generic_bounds(f: &Function) -> HashMap<String, Vec<TraitBound>> {
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
    pub(crate) fn impl_method_bounds(
        imp: &ImplBlock,
        method: &Function,
    ) -> HashMap<String, Vec<TraitBound>> {
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
    pub(crate) fn trait_method_bounds(
        t: &TraitDef,
        method: &TraitMethod,
    ) -> HashMap<String, Vec<TraitBound>> {
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
    pub(crate) fn build_fn_bounds_index(
        &self,
    ) -> HashMap<String, HashMap<String, Vec<TraitBound>>> {
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
    pub(crate) fn build_fn_effect_var_positions(
        &self,
    ) -> HashMap<String, HashMap<String, Vec<usize>>> {
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
    pub(crate) fn detect_mutual_recursion_groups(&self) -> Vec<MutualRecursionGroup> {
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
}
