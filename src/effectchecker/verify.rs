//! Public-fn and trait-impl declaration verification.
//!
//! Houses three verification passes:
//!
//! - `verify_declarations` — for each public function, verify the
//!   declared effect list is a sound superset of the inferred set;
//!   emits `MissingEffect` / `UnnecessaryEffect` diagnostics.
//! - `verify_impl_trait_ceilings` — for each impl method that
//!   implements a trait method, verify the impl's effect set is
//!   contained within the trait method's declared ceiling.
//! - `verify_trait_default_bodies` — when a trait method ships a
//!   default body, verify that body's inferred effects fit within
//!   the trait method's declared ceiling.
//!
//! Lives in a sibling `impl<'a> super::EffectChecker<'a>` block.

use crate::ast::*;
use crate::token::Span;

use super::{
    verb_name, DeclaredEffects, EffectError, EffectErrorKind, PublicEffectsPolicy, TracedEffect,
};

impl<'a> super::EffectChecker<'a> {
    // ── Verification ────────────────────────────────────────────

    pub(crate) fn verify_declarations(&mut self) {
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
    pub(crate) fn verify_impl_trait_ceilings(&mut self) {
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
    pub(crate) fn verify_trait_default_bodies(&mut self) {
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
}
