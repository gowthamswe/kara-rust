//! Borrow tracking, slice-source attribution, and call-site /
//! method-call analysis.
//!
//! Houses:
//!
//! - Call-arg formal mode lookups: `callee_modes_for_call`,
//!   `arg_is_borrow_position`, `arg_formal_slice_kind`,
//!   `arg_formal_ref_borrow_kind`.
//! - Place-expression root + slice-source attribution:
//!   `place_expr_root`, `record_slice_creation`,
//!   `slice_creation_source`.
//! - Active-borrow stack management (Slice 2 conflict detection):
//!   `push_active_borrow`, `classify_borrow_conflict`,
//!   `drain_borrows_at_depth`, `snapshot_active_borrow_lens`,
//!   `restore_active_borrows_to_snapshot`, `check_move_of_borrowed`.
//! - Method-call receiver mode lookups:
//!   `method_call_consumes_receiver`, `method_self_borrow_kind`,
//!   `method_call_receiver_is_mut_ref`.
//!
//! Lives in a sibling `impl<'a> super::OwnershipChecker<'a>` block.

use std::collections::HashMap;

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;

use super::{
    borrow_kind_display, slice_conflict_message, stdlib_method_self_borrow_kind, ActiveBorrow,
    BorrowConflict, BorrowKind, OwnershipError, OwnershipErrorKind, OwnershipMode, PlaceExpr,
    Projection, SliceConflictShape,
};

impl<'a> super::OwnershipChecker<'a> {
    /// Look up the callee's parameter modes for a free-function or static-
    /// method `Call` expression. Returns `None` for callees we can't name
    /// (function-typed values, complex expressions); those fall back to
    /// the prior conservative consume-everything behavior.
    pub(crate) fn callee_modes_for_call(&self, callee: &Expr) -> Option<&Vec<OwnershipMode>> {
        let key = match &callee.kind {
            ExprKind::Identifier(name) => name.clone(),
            ExprKind::Path { segments, .. } => segments.join("."),
            _ => return None,
        };
        self.callee_param_modes.get(&key)
    }

    /// Whether the argument at `arg_index` of `callee` is a borrow position
    /// (param declared `ref T` / `mut ref T` / `mut Slice[T]`). Args at
    /// borrow positions are *read*, not consumed, regardless of the
    /// `mut_marker` flag (which is itself only legal on `MutRef` slots).
    pub(crate) fn arg_is_borrow_position(&self, callee: &Expr, arg_index: usize) -> bool {
        self.callee_modes_for_call(callee)
            .and_then(|modes| modes.get(arg_index))
            .is_some_and(|m| matches!(m, OwnershipMode::Ref | OwnershipMode::MutRef))
    }

    /// Returns `Some(mutable)` if the formal at `arg_index` of `callee` is a
    /// slice type (`Slice[T]` or `mut Slice[T]`); `None` for non-slice
    /// formals or unresolvable callees. Drives Slice 1's call-arg coercion
    /// site detection.
    pub(crate) fn arg_formal_slice_kind(&self, callee: &Expr, arg_index: usize) -> Option<bool> {
        let key = match &callee.kind {
            ExprKind::Identifier(name) => name.clone(),
            ExprKind::Path { segments, .. } => segments.join("."),
            _ => return None,
        };
        self.callee_param_slice_kind
            .get(&key)
            .and_then(|kinds| kinds.get(arg_index).copied().flatten())
    }

    /// Slice 2 follow-up — return the call-arg-side borrow kind for a
    /// non-slice ref formal at `arg_index`. `Ref T` formals push
    /// `BorrowKind::ImmRef`; `MutRef T` formals push `BorrowKind::MutRef`.
    /// Slice formals (`Slice[T]` / `mut Slice[T]`) return `None` here so
    /// the existing slice creation hook owns those — the slice push
    /// already routes through the conflict matrix as `ImmSlice` / `MutSlice`.
    /// Owned formals and unresolvable callees also return `None`.
    pub(crate) fn arg_formal_ref_borrow_kind(
        &self,
        callee: &Expr,
        arg_index: usize,
    ) -> Option<BorrowKind> {
        if self.arg_formal_slice_kind(callee, arg_index).is_some() {
            return None;
        }
        let modes = self.callee_modes_for_call(callee)?;
        match modes.get(arg_index)? {
            OwnershipMode::Ref => Some(BorrowKind::ImmRef),
            OwnershipMode::MutRef => Some(BorrowKind::MutRef),
            OwnershipMode::Own => None,
        }
    }

    /// Resolve the root binding of a place expression at a slice creation
    /// site. Walks identifier / field / index / tuple-index / `.as_slice` /
    /// `.as_slice_mut` chains down to a root binding; returns `None` for
    /// expressions that don't start at a named binding (function-call
    /// results, struct / tuple / collection literals, etc.). For chains that
    /// pass through a slice binding (`s2 = s1[0..3]` where `s1` is itself a
    /// slice into `v`), the lookup walks transitively through
    /// `slice_binding_sources` so the returned root is the original storage
    /// (`v`), not the intermediate slice.
    pub(crate) fn place_expr_root(&self, expr: &Expr) -> Option<PlaceExpr> {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                if let Some((parent, _)) = self.slice_binding_sources.get(name) {
                    Some(parent.clone())
                } else {
                    Some(PlaceExpr {
                        root: name.clone(),
                        projections: Vec::new(),
                    })
                }
            }
            ExprKind::FieldAccess { object, field, .. } => {
                let mut p = self.place_expr_root(object)?;
                p.projections.push(Projection::Field(field.clone()));
                Some(p)
            }
            ExprKind::Index { object, index } => {
                let mut p = self.place_expr_root(object)?;
                let proj = if matches!(&index.kind, ExprKind::Range { .. }) {
                    Projection::Range
                } else {
                    Projection::Index
                };
                p.projections.push(proj);
                Some(p)
            }
            ExprKind::TupleIndex { object, .. } => {
                let mut p = self.place_expr_root(object)?;
                p.projections.push(Projection::Index);
                Some(p)
            }
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } if (method == "as_slice" || method == "as_slice_mut") && args.is_empty() => {
                self.place_expr_root(object)
            }
            _ => None,
        }
    }

    /// Record a slice creation site if the source resolves to a rooted
    /// place. Called from each of the four slice creation hook points:
    /// `.as_slice()` / `.as_slice_mut()`, range-indexing, call-arg
    /// coercion, and let-binding-rhs coercion. Idempotent — recording the
    /// same span twice is a no-op (later writes overwrite with the same
    /// value). Slice 2: also pushes an `ActiveBorrow` so the conflict
    /// matrix sees this slice when later borrows are added.
    pub(crate) fn record_slice_creation(
        &mut self,
        slice_span: &Span,
        source: &Expr,
        mutable: bool,
    ) {
        if let Some(place) = self.place_expr_root(source) {
            let key = SpanKey::from_span(slice_span);
            if let std::collections::hash_map::Entry::Vacant(e) =
                self.slice_borrow_sources.entry(key)
            {
                e.insert((place.clone(), mutable));
                let kind = if mutable {
                    BorrowKind::MutSlice
                } else {
                    BorrowKind::ImmSlice
                };
                self.push_active_borrow(kind, place, slice_span.clone());
            }
        }
    }

    /// If `expr` is a direct slice creation form (`.as_slice()` /
    /// `.as_slice_mut()` MethodCall, or `Index` with a `Range` index),
    /// return the source expression and the resulting slice's mutability.
    /// Used by the let-binding-rhs escape detector.
    pub(crate) fn slice_creation_source(expr: &Expr) -> Option<(&Expr, bool)> {
        match &expr.kind {
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } if (method == "as_slice" || method == "as_slice_mut") && args.is_empty() => {
                Some((object.as_ref(), method == "as_slice_mut"))
            }
            ExprKind::Index { object, index } if matches!(&index.kind, ExprKind::Range { .. }) => {
                Some((object.as_ref(), false))
            }
            _ => None,
        }
    }

    /// Slice 2 — push an active borrow into `active_borrows[source.root]`,
    /// scanning the existing entries first to detect slice-vs-slice and
    /// slice-vs-ref conflicts. Conflicts emit `SliceBorrowConflict` (same
    /// shape: A imm+mut, B mut+mut) or `CrossBorrowConflict` (slice + ref
    /// of same root) with the existing borrow's span as the secondary
    /// label. The new borrow is recorded regardless — we keep both so a
    /// later third operation can still detect against either.
    pub(crate) fn push_active_borrow(&mut self, kind: BorrowKind, source: PlaceExpr, span: Span) {
        // Scan existing borrows on the same root for conflicts.
        if let Some(existing) = self.active_borrows.get(&source.root) {
            for prior in existing {
                let conflict = self.classify_borrow_conflict(&prior.kind, &kind);
                match conflict {
                    BorrowConflict::SliceShape(shape) => {
                        self.errors.push(OwnershipError {
                            message: format!(
                                "{}: existing borrow at line {}:{}",
                                slice_conflict_message(&shape, &source.root),
                                prior.span.line,
                                prior.span.column
                            ),
                            span: span.clone(),
                            kind: OwnershipErrorKind::SliceBorrowConflict { shape },
                            suggestion: Some(
                                "drop the prior borrow before creating a new one (or restructure so they don't overlap)"
                                    .to_string(),
                            ),
                            replacement: None,
                            consume_span: Some(prior.span.clone()),
                        });
                    }
                    BorrowConflict::CrossForm => {
                        self.errors.push(OwnershipError {
                            message: format!(
                                "`{}` cannot be borrowed as `{}` because it is also borrowed as `{}` at line {}:{}",
                                source.root,
                                borrow_kind_display(&kind),
                                borrow_kind_display(&prior.kind),
                                prior.span.line,
                                prior.span.column
                            ),
                            span: span.clone(),
                            kind: OwnershipErrorKind::CrossBorrowConflict,
                            suggestion: Some(
                                "drop the slice borrow before mutating the source (or restructure so they don't overlap)"
                                    .to_string(),
                            ),
                            replacement: None,
                            consume_span: Some(prior.span.clone()),
                        });
                    }
                    BorrowConflict::None => {}
                }
            }
        }
        self.active_borrows
            .entry(source.root.clone())
            .or_default()
            .push(ActiveBorrow {
                kind,
                source,
                span,
                scope_depth: self.current_scope_depth,
            });
    }

    /// Slice 2 — classify the conflict shape between an existing borrow
    /// and a newly-pushed one. Symmetric in the slice-vs-slice cases (A
    /// fires whether existing is imm or new is imm). Cross-form pairs
    /// (slice + ref) route through `CrossBorrowConflict` rather than
    /// `SliceBorrowConflict`.
    #[allow(clippy::unused_self)]
    pub(crate) fn classify_borrow_conflict(
        &self,
        existing: &BorrowKind,
        new: &BorrowKind,
    ) -> BorrowConflict {
        match (existing.is_slice(), new.is_slice()) {
            (true, true) => match (existing.is_mut(), new.is_mut()) {
                (false, false) => BorrowConflict::None, // two imm slices — OK
                (true, true) => BorrowConflict::SliceShape(SliceConflictShape::MutSliceVsMutSlice),
                _ => BorrowConflict::SliceShape(SliceConflictShape::ImmSliceVsMutSlice),
            },
            (true, false) | (false, true) => {
                if existing.is_mut() || new.is_mut() {
                    BorrowConflict::CrossForm
                } else {
                    // Two immutable borrows of any form coexist — read-only.
                    BorrowConflict::None
                }
            }
            (false, false) => BorrowConflict::None,
        }
    }

    /// Slice 2 — drain any active borrows whose `scope_depth` exceeds the
    /// current scope depth. Called at block exit (after the in-block walk
    /// completes, before the depth decrements). Drop-of-borrowed detection
    /// rides this drain: a draining slice borrow whose source root is
    /// itself going out of scope here AND was bound at a shallower scope
    /// indicates the slice outlives its source storage.
    pub(crate) fn drain_borrows_at_depth(&mut self, exit_depth: usize) {
        let mut to_emit: Vec<(PlaceExpr, Span, Span)> = Vec::new();
        for (root, borrows) in self.active_borrows.iter_mut() {
            // For each draining slice, check whether its source root is
            // also dropping at this scope. The source's binding scope is
            // tracked separately so we know if the source's storage goes
            // away here.
            let source_dropping_now = self
                .binding_scope_depth
                .get(root)
                .is_some_and(|&depth| depth >= exit_depth);
            borrows.retain(|b| {
                if b.scope_depth >= exit_depth {
                    if source_dropping_now && b.kind.is_slice() {
                        // Slice's binding scope (where the slice value
                        // lives, populated at let time) is shallower
                        // than the source's? Then the slice will live
                        // past the source — shape D. We use
                        // `slice_binding_scope_depth` indexed by the
                        // root to flag this; if not present, conservative
                        // fall-through to drain without emitting.
                        if let Some(&slice_depth) =
                            self.slice_binding_scope_depth.get(&b.source.root)
                        {
                            if slice_depth < exit_depth {
                                to_emit.push((b.source.clone(), b.span.clone(), b.span.clone()));
                            }
                        }
                    }
                    false // drain
                } else {
                    true // keep
                }
            });
        }
        // Drop empty entries so the map stays clean.
        self.active_borrows.retain(|_, v| !v.is_empty());
        for (place, span, secondary) in to_emit {
            self.errors.push(OwnershipError {
                message: format!(
                    "slice into `{}` outlives its source: source dropped at end of scope while slice borrow is still live",
                    place.root,
                ),
                span,
                kind: OwnershipErrorKind::SliceBorrowConflict {
                    shape: SliceConflictShape::DropOfBorrowed,
                },
                suggestion: Some(
                    "extend the source binding's scope to outlive the slice, or restructure so the slice does not escape"
                        .to_string(),
                ),
                replacement: None,
                consume_span: Some(secondary),
            });
        }
    }

    /// Slice 2 — snapshot active-borrow per-root counts before walking a
    /// `Call` or `MethodCall`. Use with `restore_active_borrows_to_snapshot`
    /// after the args walk to drop the call-arg-coerced slice borrows
    /// (they are call-statement-scoped per the slice plan's sub-step (g)
    /// — the slice value lives only for the call's duration). This still
    /// lets the conflict matrix fire mid-call (the push side-effect emits
    /// the diagnostic before the drain), so persistent slice + transient
    /// coerced slice still conflicts. Sequential calls do not stack up.
    pub(crate) fn snapshot_active_borrow_lens(&self) -> HashMap<String, usize> {
        self.active_borrows
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect()
    }

    pub(crate) fn restore_active_borrows_to_snapshot(&mut self, snapshot: &HashMap<String, usize>) {
        let roots: Vec<String> = self.active_borrows.keys().cloned().collect();
        for root in roots {
            let target = snapshot.get(&root).copied().unwrap_or(0);
            if let Some(borrows) = self.active_borrows.get_mut(&root) {
                if borrows.len() > target {
                    borrows.truncate(target);
                }
            }
        }
        self.active_borrows.retain(|_, v| !v.is_empty());
    }

    /// Slice 2 — at every consume that would transition `name` to
    /// `Moved`, check whether `name` has any live slice borrows. If so,
    /// emit shape C (move-of-borrowed) before the move proceeds. Returns
    /// `true` iff a conflict was emitted (caller may use this to suppress
    /// the consume — but v1 keeps the consume regardless so downstream
    /// state stays consistent).
    pub(crate) fn check_move_of_borrowed(&mut self, name: &str, move_span: &Span) -> bool {
        let Some(borrows) = self.active_borrows.get(name) else {
            return false;
        };
        if borrows.is_empty() {
            return false;
        }
        // Use the first live borrow as the secondary span — multiple
        // borrows would each fire, but for v1 we keep the diagnostic
        // count to one per move.
        let prior = borrows[0].clone();
        self.errors.push(OwnershipError {
            message: format!(
                "cannot move `{}` while a slice borrow into it is still live (borrowed at line {}:{})",
                name, prior.span.line, prior.span.column
            ),
            span: move_span.clone(),
            kind: OwnershipErrorKind::SliceBorrowConflict {
                shape: SliceConflictShape::MoveOfBorrowed,
            },
            suggestion: Some(
                "drop the slice borrow before moving the source, or restructure so they don't overlap"
                    .to_string(),
            ),
            replacement: None,
            consume_span: Some(prior.span),
        });
        true
    }

    /// Resolve the method's receiver mode for a `MethodCall` expression.
    /// Returns `true` iff the receiver should be consumed (declared
    /// `bare self`). Reads the typechecker's method-callee resolution to
    /// pick the canonical `Type.method` key, then looks up the declared
    /// `SelfParam` from the impl-block / trait declaration.
    ///
    /// Falls back to `false` (read-only receiver, the prior behavior) when
    /// the lookup misses — typecheck errors upstream, methods on stdlib
    /// types whose impls are not in user code, etc. This is a conservative
    /// default: if we can't prove the receiver is consumed, we assume it
    /// isn't.
    pub(crate) fn method_call_consumes_receiver(&self, method_call: &Expr) -> bool {
        let key = match self
            .typecheck_result
            .method_callee_types
            .get(&SpanKey::from_span(&method_call.span))
        {
            Some(k) => k,
            None => return false,
        };
        matches!(self.method_self_modes.get(key), Some(SelfParam::Owned))
    }

    /// Slice 2 — the receiver-side `BorrowKind` for a `MethodCall`. Drives
    /// the call-statement-scoped ref-side push that Slice 2's sub-step (g)
    /// gates on. Returns `None` for static methods, bare-self consumes
    /// (no borrow), and unresolved methods. Falls through to a small
    /// table of stdlib method receiver modes when the user-impl lookup
    /// misses — `Vec.push` / `Map.insert` etc. don't have user-side
    /// `impl` blocks, so without the table cross-borrow detection would
    /// silently miss for the most common case (`let _s = v.as_slice();
    /// v.push(99);`).
    pub(crate) fn method_self_borrow_kind(&self, method_call: &Expr) -> Option<BorrowKind> {
        let key = self
            .typecheck_result
            .method_callee_types
            .get(&SpanKey::from_span(&method_call.span))?;
        if let Some(self_param) = self.method_self_modes.get(key) {
            return match self_param {
                SelfParam::Owned => None,
                SelfParam::Ref => Some(BorrowKind::ImmRef),
                SelfParam::MutRef => Some(BorrowKind::MutRef),
            };
        }
        stdlib_method_self_borrow_kind(key)
    }

    /// Whether the resolved method's receiver is `mut ref self`. Used by the
    /// trigger 3 detection: a `mut ref self` receiver is a "container" in the
    /// design.md § Part 4 trigger 3 sense — it outlives the call, so an
    /// owned arg consumed into it stays alive on a path parallel to any
    /// subsequent outer use of the source binding.
    pub(crate) fn method_call_receiver_is_mut_ref(&self, method_call: &Expr) -> bool {
        let key = match self
            .typecheck_result
            .method_callee_types
            .get(&SpanKey::from_span(&method_call.span))
        {
            Some(k) => k,
            None => return false,
        };
        matches!(self.method_self_modes.get(key), Some(SelfParam::MutRef))
    }
}
