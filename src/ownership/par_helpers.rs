//! Channel + par-block helpers used by the Rc → Arc promotion pass.
//!
//! Houses the small attribute / channel-type recognition helpers
//! (`has_attr`, `collect_channel_param_types`, `type_expr_root_name`,
//! `recognize_channel_new`, `resolve_receiver_is_sender`) plus the
//! big three-way par-block walker (`scan_block_for_par_uses`,
//! `scan_stmt_for_par_uses`, `scan_expr_for_par_uses`) that finds
//! all bindings referenced inside `par {…}` regions so they can be
//! promoted from Rc to Arc in Phase 2.
//!
//! Free functions (no `Self` reference); the `Phase 2` driver in
//! ownership.rs holds them under `pub(crate)` use-imports.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::resolver::SpanKey;

use super::OwnershipMode;

pub(crate) fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.is_bare(name))
}

/// Extract `Sender` / `Receiver` annotations from a function or method's
/// parameter list for the Phase 2 par-walker's `let_types` seed. Strips
/// outer `ref` / `mut ref` / `weak` wrappers and looks at the path's last
/// segment. Non-Sender/Receiver names are skipped — the walker only cares
/// about the channel boundary (Theme 2, wip-list2 2026-05-08); other type
/// annotations are not load-bearing for the Rc → Arc promotion decision.
pub(crate) fn collect_channel_param_types(params: &[Param]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in params {
        let Some(name) = p.name() else { continue };
        let Some(ty_name) = type_expr_root_name(&p.ty) else {
            continue;
        };
        if ty_name == "Sender" || ty_name == "Receiver" {
            out.push((name.to_string(), ty_name));
        }
    }
    out
}

/// Recover the root type name from a `TypeExpr`, stripping `ref`/`mut ref`/
/// `weak` wrappers. Returns the path's last segment for a `Path` type;
/// `None` for tuples, function types, or unresolved forms. Mirrors the
/// shape of `provider_escape::type_expr_name` — same purpose, same rule.
pub(crate) fn type_expr_root_name(ty: &TypeExpr) -> Option<String> {
    match &ty.kind {
        TypeKind::Path(p) => p.segments.last().cloned(),
        TypeKind::Ref(inner) | TypeKind::MutRef(inner) | TypeKind::Weak(inner) => {
            type_expr_root_name(inner)
        }
        _ => None,
    }
}

/// Recognize a `Channel.new()` call shape in a `let pat = value;` RHS.
/// The parser emits this either as `MethodCall { object: Identifier("Channel"),
/// method: "new" }` (the common case) or as `Call { callee: Path(["Channel",
/// "new"]) }` (the path-callee form). No-arg only — argued forms like a
/// hypothetical `Channel.bounded(n)` are not v1 channel-source forms today;
/// extend this predicate when they ship.
pub(crate) fn recognize_channel_new(value: &Expr) -> bool {
    match &value.kind {
        ExprKind::MethodCall { object, method, .. } => {
            method == "new" && matches!(&object.kind, ExprKind::Identifier(n) if n == "Channel")
        }
        ExprKind::Call { callee, .. } => matches!(
            &callee.kind,
            ExprKind::Path { segments, .. }
                if segments.len() == 2 && segments[0] == "Channel" && segments[1] == "new"
        ),
        _ => false,
    }
}

/// Decide whether a `MethodCall`'s receiver expression resolves to a
/// `Sender[T]`-typed binding. Only `tx.send(payload)` against a `Sender`
/// counts as a channel-send boundary for the par-walker. Two shapes are
/// recognized:
///
/// - **Identifier(`tx`)**: looked up in `let_types`. Returns true iff
///   `let_types[tx] == "Sender"`.
/// - **Chained `tx.clone().send(...)`**: receiver is a `MethodCall {
///   method: "clone", object: Identifier(tx) }`. We unwrap the clone
///   one level and consult `let_types[tx]`. Per round-8 escape detection
///   in `provider_escape.rs`, `Sender::clone` returns `Sender[T]`, so the
///   sent payload still flows across the channel boundary.
///
/// Other receiver shapes (struct field access, multi-level chains beyond
/// one `.clone()`) fall through unrecognized for v1 — over-approximation
/// at this gate is unsound, so the conservative default is "not a Sender."
pub(crate) fn resolve_receiver_is_sender(
    object: &Expr,
    let_types: &HashMap<String, String>,
) -> bool {
    match &object.kind {
        ExprKind::Identifier(name) => let_types.get(name).map(String::as_str) == Some("Sender"),
        ExprKind::MethodCall {
            object: inner,
            method,
            ..
        } if method == "clone" => {
            if let ExprKind::Identifier(name) = &inner.kind {
                let_types.get(name).map(String::as_str) == Some("Sender")
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Walk a block, recording which bindings from `candidates` are used
/// inside a `par {}` (Phase 2 live-range overlap, conservative form).
///
/// Round 12.34 (Step 6): also threads `closure_captures` (read-only) and
/// a mutable `closure_bindings` accumulator. The walk registers each
/// `let pat = closure_expr;` form into `closure_bindings` as it
/// encounters them; a subsequent par-region use of any registered
/// closure binding promotes its captures present in `candidates`. The
/// merged single-pass pattern is sound because forward source order is
/// preserved within each block — a closure binding is registered before
/// any later reference to it can be observed in par-region position.
pub(crate) fn scan_block_for_par_uses(
    block: &Block,
    inside_parallel_region: bool,
    candidates: &HashSet<String>,
    closure_captures: &HashMap<SpanKey, Vec<(String, OwnershipMode)>>,
    closure_bindings: &mut HashMap<String, Vec<String>>,
    let_types: &mut HashMap<String, String>,
    promoted: &mut HashSet<String>,
) {
    for stmt in &block.stmts {
        scan_stmt_for_par_uses(
            stmt,
            inside_parallel_region,
            candidates,
            closure_captures,
            closure_bindings,
            let_types,
            promoted,
        );
    }
    if let Some(ref expr) = block.final_expr {
        scan_expr_for_par_uses(
            expr,
            inside_parallel_region,
            candidates,
            closure_captures,
            closure_bindings,
            let_types,
            promoted,
        );
    }
}

pub(crate) fn scan_stmt_for_par_uses(
    stmt: &Stmt,
    inside_parallel_region: bool,
    candidates: &HashSet<String>,
    closure_captures: &HashMap<SpanKey, Vec<(String, OwnershipMode)>>,
    closure_bindings: &mut HashMap<String, Vec<String>>,
    let_types: &mut HashMap<String, String>,
    promoted: &mut HashSet<String>,
) {
    match &stmt.kind {
        StmtKind::Let {
            pattern, value, ty, ..
        } => {
            // Round 12.34 (Step 6): register `let pat = closure_expr;`
            // forms into `closure_bindings` so subsequent par-region uses
            // of the binding can promote each capture present in
            // `candidates`. Tuple/struct patterns over a single closure
            // value are uncommon (closures are not destructure-able by
            // shape today), but we mirror the round-12.20 once-callable
            // registration's pattern.binding_names() form for parity.
            if matches!(value.kind, ExprKind::Closure { .. }) {
                if let Some(captures) = closure_captures.get(&SpanKey::from_span(&value.span)) {
                    let names: Vec<String> = captures.iter().map(|(n, _)| n.clone()).collect();
                    for binding in pattern.binding_names() {
                        closure_bindings.insert(binding, names.clone());
                    }
                }
            }
            // Theme 2 (wip-list2, 2026-05-08): record `Sender` / `Receiver`
            // type annotations and `Channel.new()` destructures into
            // `let_types` so the channel-send boundary in the MethodCall
            // arm below can resolve `tx.send(...)` receivers. Two shapes:
            //   (a) explicit annotation: `let tx: Sender[T] = ...;` —
            //       record every leaf binding of `pattern` against the
            //       root type name. Plain bindings (`Binding(tx)`) and
            //       at-bindings (`AtBinding`) cover the v1 surface; tuple
            //       patterns over a single Sender value are rejected at
            //       parse, so the leaf-flatten via `binding_names()` is
            //       safe-by-construction.
            //   (b) tuple destructure of `Channel.new()`: only the exact
            //       2-leaf form `let (tx, rx) = Channel.new();` registers
            //       — index 0 → "Sender", index 1 → "Receiver". Other
            //       tuple shapes fall through (conservative).
            if let Some(ty_expr) = ty {
                if let Some(ty_name) = type_expr_root_name(ty_expr) {
                    if ty_name == "Sender" || ty_name == "Receiver" {
                        for binding in pattern.binding_names() {
                            let_types.insert(binding, ty_name.clone());
                        }
                    }
                }
            }
            if recognize_channel_new(value) {
                if let PatternKind::Tuple(elems) = &pattern.kind {
                    if elems.len() == 2 {
                        if let (PatternKind::Binding(tx), PatternKind::Binding(rx)) =
                            (&elems[0].kind, &elems[1].kind)
                        {
                            let_types.insert(tx.clone(), "Sender".to_string());
                            let_types.insert(rx.clone(), "Receiver".to_string());
                        }
                    }
                }
            }
            scan_expr_for_par_uses(
                value,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        StmtKind::LetUninit { .. } => {}
        StmtKind::LetElse {
            value, else_block, ..
        } => {
            scan_expr_for_par_uses(
                value,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_block_for_par_uses(
                else_block,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
            scan_block_for_par_uses(
                body,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        StmtKind::Assign { target, value } => {
            scan_expr_for_par_uses(
                target,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_expr_for_par_uses(
                value,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        StmtKind::CompoundAssign { target, value, .. } => {
            scan_expr_for_par_uses(
                target,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_expr_for_par_uses(
                value,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        StmtKind::Expr(expr) => {
            scan_expr_for_par_uses(
                expr,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
    }
}

pub(crate) fn scan_expr_for_par_uses(
    expr: &Expr,
    inside_parallel_region: bool,
    candidates: &HashSet<String>,
    closure_captures: &HashMap<SpanKey, Vec<(String, OwnershipMode)>>,
    closure_bindings: &mut HashMap<String, Vec<String>>,
    let_types: &mut HashMap<String, String>,
    promoted: &mut HashSet<String>,
) {
    match &expr.kind {
        // Round 12.34 (Step 6): a use of any name inside a parallel
        // region promotes the name itself if RC-marked, AND every
        // RC-marked capture of any closure bound to that name. The
        // captures-via-closure-binding propagation realises design.md §
        // Closures Rule 2's "live range of closure value = live range of
        // each capture for the escape sub-case" for the v1-realisable
        // parallel-region escape routes: `par { h(); }`, `par { f(h); }`,
        // and (Theme 2 of wip-list2, 2026-05-08) `tx.send(h)` where `tx`
        // is a `Sender[T]` — the channel-send boundary flips
        // `inside_parallel_region` for its argument subtree, mirroring
        // the same shape `par {}` uses. The `spawn(closure)` boundary
        // stays deferred to Phase 6.3 / v1.1 user syntax.
        ExprKind::Identifier(name) if inside_parallel_region => {
            if candidates.contains(name) {
                promoted.insert(name.clone());
            }
            if let Some(captures) = closure_bindings.get(name) {
                for cap in captures {
                    if candidates.contains(cap) {
                        promoted.insert(cap.clone());
                    }
                }
            }
        }
        ExprKind::Par(body) => {
            scan_block_for_par_uses(
                body,
                true,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Block(block)
        | ExprKind::Loop { body: block, .. }
        | ExprKind::LabeledBlock { body: block, .. }
        | ExprKind::Unsafe(block)
        | ExprKind::Try(block)
        | ExprKind::Seq(block)
        | ExprKind::Lock { body: block, .. } => {
            scan_block_for_par_uses(
                block,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
            scan_expr_for_par_uses(
                left,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_expr_for_par_uses(
                right,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Unary { operand, .. } => {
            scan_expr_for_par_uses(
                operand,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Call { callee, args } => {
            scan_expr_for_par_uses(
                callee,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            for arg in args {
                scan_expr_for_par_uses(
                    &arg.value,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } => {
            // Theme 2 (wip-list2, 2026-05-08): `tx.send(payload)` where
            // `tx` resolves to a `Sender[T]` flips the parallel-region
            // flag for the args subtree only. The receiver position is
            // NOT in the parallel region — it's the sender, not the
            // payload. Cloned-sender shape `tx.clone().send(h)` works
            // because `resolve_receiver_is_sender` unwraps one level of
            // `.clone()`.
            let send_boundary = method == "send" && resolve_receiver_is_sender(object, let_types);
            scan_expr_for_par_uses(
                object,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            for arg in args {
                scan_expr_for_par_uses(
                    &arg.value,
                    inside_parallel_region || send_boundary,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
            scan_expr_for_par_uses(
                object,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Index { object, index } => {
            scan_expr_for_par_uses(
                object,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_expr_for_par_uses(
                index,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            scan_expr_for_par_uses(
                condition,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_block_for_par_uses(
                then_block,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            if let Some(eb) = else_branch {
                scan_expr_for_par_uses(
                    eb,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::IfLet {
            value,
            then_block,
            else_branch,
            ..
        } => {
            scan_expr_for_par_uses(
                value,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_block_for_par_uses(
                then_block,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            if let Some(eb) = else_branch {
                scan_expr_for_par_uses(
                    eb,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            scan_expr_for_par_uses(
                scrutinee,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            for arm in arms {
                if let Some(g) = &arm.guard {
                    scan_expr_for_par_uses(
                        g,
                        inside_parallel_region,
                        candidates,
                        closure_captures,
                        closure_bindings,
                        let_types,
                        promoted,
                    );
                }
                scan_expr_for_par_uses(
                    &arm.body,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::While {
            condition, body, ..
        } => {
            scan_expr_for_par_uses(
                condition,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_block_for_par_uses(
                body,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::WhileLet { value, body, .. } => {
            scan_expr_for_par_uses(
                value,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_block_for_par_uses(
                body,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::For { iterable, body, .. } => {
            scan_expr_for_par_uses(
                iterable,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_block_for_par_uses(
                body,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Closure { body, .. }
        | ExprKind::Question(body)
        | ExprKind::OptionalChain { object: body, .. }
        | ExprKind::Cast { expr: body, .. } => {
            scan_expr_for_par_uses(
                body,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Return(Some(inner))
        | ExprKind::Break {
            value: Some(inner), ..
        } => {
            scan_expr_for_par_uses(
                inner,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::NilCoalesce { left, right } => {
            scan_expr_for_par_uses(
                left,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_expr_for_par_uses(
                right,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
            for e in exprs {
                scan_expr_for_par_uses(
                    e,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::RepeatLiteral { value, count, .. } => {
            scan_expr_for_par_uses(
                value,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
            scan_expr_for_par_uses(
                count,
                inside_parallel_region,
                candidates,
                closure_captures,
                closure_bindings,
                let_types,
                promoted,
            );
        }
        ExprKind::PrefixCollectionLiteral { items, .. } => {
            for e in items {
                scan_expr_for_par_uses(
                    e,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::StructLiteral { fields, spread, .. } => {
            for field in fields {
                scan_expr_for_par_uses(
                    &field.value,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
            if let Some(s) = spread {
                scan_expr_for_par_uses(
                    s,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::MapLiteral(entries) => {
            for (k, v) in entries {
                scan_expr_for_par_uses(
                    k,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
                scan_expr_for_par_uses(
                    v,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                scan_expr_for_par_uses(
                    s,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
            if let Some(e) = end {
                scan_expr_for_par_uses(
                    e,
                    inside_parallel_region,
                    candidates,
                    closure_captures,
                    closure_bindings,
                    let_types,
                    promoted,
                );
            }
        }
        // Leaves and others do not contribute uses.
        _ => {}
    }
}
