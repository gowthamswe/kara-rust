//! Rc → Arc promotion + RC fallback note emission + `@no_rc`
//! enforcement (Phases 2 / 3 / K2).
//!
//! Houses:
//!
//! - `emit_rc_fallback_notes` — Phase 3: emit one `RcFallbackNote`
//!   per RC binding, flavored "Rc" or "Arc" per Phase 2's outcome.
//! - `promote_rc_to_arc` — Phase 2: walk each function's body via
//!   `scan_block_for_par_uses` to find bindings live across a
//!   `par {}` region, and promote them.
//! - `promote_for_function` — per-function helper for `promote_rc_to_arc`.
//! - `enforce_no_rc_attrs` — K2: enforce `#[no_rc]` and `@no_rc`
//!   attributes on functions and impl methods, erroring on any
//!   RC trigger.
//!
//! Lives in a sibling `impl<'a> super::OwnershipChecker<'a>` block.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::token::Span;

use super::par_helpers::{collect_channel_param_types, has_attr, scan_block_for_par_uses};
use super::{OwnershipError, OwnershipErrorKind};

impl<'a> super::OwnershipChecker<'a> {
    // ── RC Fallback Notes (emitted after Phase 2) ────────────────

    /// Emit one `RcFallbackNote` per RC binding, with the flavor determined
    /// by Phase 2: bindings in `arc_values` get "shared (Arc) — promoted:
    /// value crosses a parallel region"; others get "shared (Rc) — value
    /// does not cross a parallel region".
    ///
    /// Slice-6 enrichment (line 353 phase-5 checklist disjoint-capture
    /// slice 6): when the RC binding was captured *whole* by a closure
    /// inside the same enclosing function, append the
    /// `WholeRootCaptureReason` explanation to the note message and
    /// replace the generic suggestion with a fix-it that names the
    /// rewrite (hoist the field access outside the stopping
    /// construct). Lets the user see *why* their natural sibling-path
    /// access forced an RC promotion — without the explanation, the
    /// N0503 note tells the user RC happened but not which construct
    /// in the closure body forced the whole-root capture choice.
    pub(crate) fn emit_rc_fallback_notes(&mut self) {
        let mut notes = Vec::new();
        for (fn_key, rc_map) in &self.rc_values {
            if self.suppressed_rc_fn_keys.contains(fn_key) {
                continue;
            }
            let arc_set = self.arc_values.get(fn_key);
            for (binding, entry) in rc_map {
                let is_arc = arc_set.is_some_and(|s| s.contains(binding));
                let flavor = if is_arc {
                    "shared (Arc) — promoted: value crosses a parallel region"
                } else {
                    "shared (Rc) — value does not cross a parallel region"
                };
                let base_message = format!(
                    "RC fallback inserted for '{}' ({}); {}; consume at line {}:{}, other use at line {}:{}",
                    entry.binding,
                    entry.trigger.label(),
                    flavor,
                    entry.consume_span.line,
                    entry.consume_span.column,
                    entry.other_use_span.line,
                    entry.other_use_span.column,
                );
                let (message, suggestion) =
                    match self.find_whole_root_closure_reason(fn_key, binding) {
                        Some((closure_span, reason)) => {
                            let reason_text = reason.describe(binding);
                            let enriched = format!(
                                "{} — closure at line {}:{} captured `{}` whole because {}",
                                base_message,
                                closure_span.line,
                                closure_span.column,
                                binding,
                                reason_text,
                            );
                            (enriched, Some(Self::slice6_fix_it_suggestion(&reason)))
                        }
                        None => (
                            base_message,
                            Some(
                                "restructure to a single ownership path, or accept the RC and silence with #[allow(rc_fallback)]"
                                    .to_string(),
                            ),
                        ),
                    };
                notes.push(OwnershipError {
                    message,
                    span: entry.other_use_span.clone(),
                    kind: OwnershipErrorKind::RcFallbackNote,
                    suggestion,
                    replacement: None,
                    consume_span: Some(entry.consume_span.clone()),
                });
            }
        }
        self.notes.extend(notes);
    }

    /// Find a closure inside function `fn_key` whose whole-root
    /// capture reasons name `binding`. Returns the closure span and
    /// its reason for that root — used by `emit_rc_fallback_notes`
    /// to enrich the N0503 note with the slice-6 explanation. When
    /// multiple closures in the same function whole-root capture the
    /// same binding, the first encountered wins; the user typically
    /// only needs one explanation to understand the pattern. Per
    /// design.md § Rule 2¼ Interaction with Rule 2½, the closure's
    /// capture reason is the spec-mandated "the body called method
    /// `…` on `…`" explanation that completes the N0503 note's
    /// `direct re-use after consume` framing.
    fn find_whole_root_closure_reason(
        &self,
        fn_key: &str,
        binding: &str,
    ) -> Option<(super::Span, super::WholeRootCaptureReason)> {
        for (closure_key, reasons) in &self.whole_root_capture_reasons {
            if self.closure_function.get(closure_key).map(String::as_str) != Some(fn_key) {
                continue;
            }
            if let Some(reason) = reasons.get(binding) {
                if let Some(span) = self.closure_spans.get(closure_key) {
                    return Some((span.clone(), reason.clone()));
                }
            }
        }
        None
    }

    /// Spec-mandated fix-it tail for slice 6: a one-liner steering
    /// the user toward the rewrite that breaks the whole-root
    /// capture. Method calls: hoist the field access outside the
    /// call. Index / Deref: hoist the indexed / dereffed value into
    /// a local. By-value pass: lift the projection out of the call
    /// arg. BareIdentifier has no rewrite — the closure body
    /// *intentionally* names the whole binding — so we fall back to
    /// the generic "accept the RC" framing.
    fn slice6_fix_it_suggestion(reason: &super::WholeRootCaptureReason) -> String {
        match reason {
            super::WholeRootCaptureReason::MethodCall { method_name, .. } => format!(
                "hoist the call outside the closure (assign `{}`'s result to a local before the closure) so the closure body captures only the fields it actually reads",
                method_name,
            ),
            super::WholeRootCaptureReason::Index { .. } => {
                "hoist the indexed element into a local before the closure so the closure body captures only the element, not the whole collection".to_string()
            }
            super::WholeRootCaptureReason::Deref { .. } => {
                "hoist the dereferenced value into a local before the closure so the closure body captures the pointee directly".to_string()
            }
            super::WholeRootCaptureReason::ByValuePass { .. } => {
                "if only specific fields are needed, project them into locals before the closure and pass those locals instead of the whole binding".to_string()
            }
            super::WholeRootCaptureReason::BareIdentifier => {
                "the closure body directly names the whole binding; accept the RC and silence with #[allow(rc_fallback)]".to_string()
            }
        }
    }

    // ── Phase 2: Rc → Arc Promotion ─────────────────────────────

    /// For each function with RC bindings, walk its body looking for any
    /// use of those bindings that lies inside a `par {}` block. Each
    /// such binding is promoted from Rc to Arc.
    ///
    /// Conservative: a binding whose live range overlaps any parallel
    /// region is Arc for its entire live range (one decision per value,
    /// matching design.md § Rc vs Arc — Two-Phase Algorithm).
    pub(crate) fn promote_rc_to_arc(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) => {
                    let params = collect_channel_param_types(&f.params);
                    self.promote_for_function(&f.name, None, &params, &f.body);
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for item in &imp.items {
                        if let ImplItem::Method(method) = item {
                            let params = collect_channel_param_types(&method.params);
                            self.promote_for_function(
                                &method.name,
                                Some(&type_name),
                                &params,
                                &method.body,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn promote_for_function(
        &mut self,
        fn_name: &str,
        impl_type: Option<&str>,
        params: &[(String, String)],
        body: &Block,
    ) {
        let fn_key = match impl_type {
            Some(t) => format!("{}.{}", t, fn_name),
            None => fn_name.to_string(),
        };
        let Some(rc_map) = self.rc_values.get(&fn_key) else {
            return;
        };
        let candidates: HashSet<String> = rc_map.keys().cloned().collect();
        if candidates.is_empty() {
            return;
        }
        let mut promoted: HashSet<String> = HashSet::new();
        // Round 12.34 (Step 6): per-function map from closure-binding name
        // to its capture names, populated as the par-walker traverses
        // `let pat = closure_expr;` forms. A subsequent par-region use of
        // the closure binding promotes each capture present in
        // `candidates` to Arc, per design.md § Closures Rule 2's
        // "live range of closure value = live range of each capture for
        // the escape sub-case". Sourced from `self.closure_captures`
        // (round 12.24); only the names are needed downstream.
        let mut closure_bindings: HashMap<String, Vec<String>> = HashMap::new();
        // Theme 2 (wip-list2, 2026-05-08): per-function `let_types` map
        // tracking each binding's structurally-recovered type name —
        // currently only `Sender` / `Receiver` for the channel-send
        // boundary. Seeded from the function's parameters and grown as
        // the walker traverses `let` forms with `Sender[T]` / `Receiver[T]`
        // annotations or `Channel.new()` destructures.
        let mut let_types: HashMap<String, String> = HashMap::new();
        for (name, type_name) in params {
            let_types.insert(name.clone(), type_name.clone());
        }
        scan_block_for_par_uses(
            body,
            false,
            &candidates,
            &self.closure_captures,
            &mut closure_bindings,
            &mut let_types,
            &mut promoted,
        );
        if !promoted.is_empty() {
            self.arc_values.insert(fn_key, promoted);
        }
    }

    // ── #[no_rc] / @no_rc Enforcement ──────────────────────────

    pub(crate) fn enforce_no_rc_attrs(&mut self) {
        // Collect strict-no-rc functions
        let mut strict_fns: Vec<(String, Span)> = Vec::new();
        let mut no_rc_types: HashSet<String> = HashSet::new();

        for item in &self.program.items {
            match item {
                Item::Function(f) if has_attr(&f.attributes, "no_rc") => {
                    strict_fns.push((f.name.clone(), f.span.clone()));
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for it in &imp.items {
                        if let ImplItem::Method(m) = it {
                            if has_attr(&m.attributes, "no_rc") {
                                strict_fns
                                    .push((format!("{}.{}", type_name, m.name), m.span.clone()));
                            }
                        }
                    }
                }
                Item::StructDef(s) if s.no_rc => {
                    no_rc_types.insert(s.name.clone());
                }
                _ => {}
            }
        }

        // #[no_rc] on a function: any RC binding is an error.
        for (fn_key, fn_span) in &strict_fns {
            if let Some(rc_map) = self.rc_values.get(fn_key) {
                for (binding, entry) in rc_map {
                    self.errors.push(OwnershipError {
                        message: format!(
                            "function '{}' is #[no_rc] but value '{}' would require RC fallback ({})",
                            fn_key,
                            binding,
                            entry.trigger.label(),
                        ),
                        span: entry.other_use_span.clone(),
                        kind: OwnershipErrorKind::NoRcViolation,
                        suggestion: Some(format!(
                            "restructure '{}' so that consume and reuse lie on a single ownership path, or remove #[no_rc]",
                            binding
                        )),
                        replacement: None,
                        consume_span: None,
                    });
                }
                let _ = fn_span; // span available if we want to attach a secondary later
            }
        }

        // @no_rc on a struct: any RC binding of that type is an error.
        for rc_map in self.rc_values.values() {
            for (binding, entry) in rc_map {
                let Some(ty) = &entry.type_name else { continue };
                if no_rc_types.contains(ty) {
                    self.errors.push(OwnershipError {
                        message: format!(
                            "type '{}' is declared @no_rc but value '{}' would require RC fallback ({})",
                            ty,
                            binding,
                            entry.trigger.label(),
                        ),
                        span: entry.other_use_span.clone(),
                        kind: OwnershipErrorKind::NoRcViolation,
                        suggestion: Some(format!(
                            "restructure to keep '{}' on a single ownership path, or drop @no_rc on '{}'",
                            binding, ty
                        )),
                        replacement: None,
                        consume_span: None,
                    });
                }
            }
        }
    }
}
