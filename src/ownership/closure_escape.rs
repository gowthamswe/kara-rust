//! Closure-escape ref-capture detection (round 12.35 — Closure
//! ownership Step 7).
//!
//! Houses `check_closure_ref_capture_escapes` (the per-function
//! entry) and the read-only walker family that powers it:
//!
//! - `collect_closure_let_bindings` /
//!   `collect_closure_let_bindings_in_expr` — build the
//!   `binding name → closure span` map for closures stored in
//!   single-name let bindings.
//! - `collect_escaping_closures` /
//!   `collect_escaping_closures_in_expr` /
//!   `collect_escape_target` — find every `return Some(closure_or_ident)`
//!   site (and the tail-expression form) that escapes a closure
//!   past the current function's frame.
//! - `fn_allows_ref_capture_escape` — opt-in attribute check.
//! - `collect_call_arg_escape_closures` — recognize closures passed
//!   as call args where the callee declares an escape contract.
//! - `walk_block_for_calls` / `walk_stmt_for_calls` /
//!   `walk_expr_for_calls` — generic call-site visitor used by the
//!   above to thread a `FnMut(&Expr, &[CallArg])` over the body.
//!
//! Lives in a sibling `impl<'a> super::OwnershipChecker<'a>` block.

use std::collections::HashMap;

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::typechecker::Type;

use super::{OwnershipError, OwnershipErrorKind, OwnershipMode};

impl<'a> super::OwnershipChecker<'a> {
    /// Round 12.35 — Closure ownership Step 7: detect ref-captured
    /// values that escape via `return`. Walks the function body once
    /// to: (1) collect a `closure_let_bindings` map from let-binding
    /// name → closure expression span (only `let pat = closure_expr;`
    /// forms with a single name); (2) find every escape site —
    /// `return Some(closure_or_ident)` statements anywhere in the body
    /// and the function-body's tail-expression form. For each escape
    /// whose underlying closure has at least one Ref/MutRef capture of
    /// a binding owned by the current function (i.e., `binding_types`
    /// for the captured name is not itself `Type::Ref` / `Type::MutRef`),
    /// emit `E0508` at the closure expression with a three-fix message.
    /// Captures whose source is itself a borrow (e.g., a `ref T`
    /// parameter) do not fire — the borrow source already extends to
    /// the caller's scope, so the closure's ref capture cannot outlive
    /// it from the current function's perspective.
    pub(crate) fn check_closure_ref_capture_escapes(&mut self, f: &Function) {
        let body = &f.body;
        let mut closure_let_bindings: HashMap<String, Vec<SpanKey>> = HashMap::new();
        Self::collect_closure_let_bindings(body, &mut closure_let_bindings);
        let mut escape_closures: Vec<SpanKey> = Vec::new();
        Self::collect_escaping_closures(body, &closure_let_bindings, &mut escape_closures);
        if let Some(tail) = &body.final_expr {
            Self::collect_escape_target(tail, &closure_let_bindings, &mut escape_closures);
        }
        // Round 12.39 — fn-arg pass conservative-fire. A closure
        // passed as a fn-arg to an Own-mode parameter slot may or
        // may not be stored beyond the call (the receiving function
        // could invoke-and-drop it synchronously, OR store it in a
        // long-lived cell, OR re-pass it elsewhere). Without inter-
        // procedural analysis we cannot tell, so we conservatively
        // treat every Own-mode Fn-slot pass as an escape. `ref Fn(...)`
        // / `mut ref Fn(...)` slots are skipped — the callee borrows
        // the closure for the duration of its call and cannot store
        // it beyond return. The opt-out is `#[allow(ref_capture_
        // escape)]` on the enclosing function: closures passed to
        // truly synchronous Own-mode Fn slots can be silenced
        // function-wise until callee-side annotation infrastructure
        // (`#[non_escaping]` on Fn parameter slots, or inter-
        // procedural body inspection for in-module callees) lands.
        if !Self::fn_allows_ref_capture_escape(f) {
            self.collect_call_arg_escape_closures(
                body,
                &closure_let_bindings,
                &mut escape_closures,
            );
        }
        for closure_key in escape_closures {
            let captures = match self.closure_captures.get(&closure_key) {
                Some(c) => c.clone(),
                None => continue,
            };
            let closure_span = match self.closure_spans.get(&closure_key).cloned() {
                Some(s) => s,
                None => continue,
            };
            for (cap_name, mode) in &captures {
                if !matches!(mode, OwnershipMode::Ref | OwnershipMode::MutRef) {
                    continue;
                }
                // Skip if the captured binding is itself a borrow —
                // its borrow source already extends to the caller's
                // scope, so escaping a ref-of-ref cannot outlive the
                // source from this function's perspective.
                if matches!(
                    self.binding_types.get(cap_name),
                    Some(Type::Ref(_)) | Some(Type::MutRef(_))
                ) {
                    continue;
                }
                let mode_str = match mode {
                    OwnershipMode::Ref => "ref",
                    OwnershipMode::MutRef => "mut ref",
                    OwnershipMode::Own => unreachable!(),
                };
                let fix = format!(
                    "consider one of: (a) clone `{cap_name}` inside the closure body so the capture becomes owned; (b) restructure so the closure stays inside the function (do not return it); (c) consume `{cap_name}` in the closure body (e.g., move it into a call) so the capture becomes `own` and RC fallback handles the sharing"
                );
                self.errors.push(OwnershipError {
                    message: format!(
                        "closure with `{mode_str}` capture of `{cap_name}` escapes its scope by being returned — the borrow of `{cap_name}` would outlive its source"
                    ),
                    span: closure_span.clone(),
                    kind: OwnershipErrorKind::RefCaptureEscapesScope,
                    suggestion: Some(fix),
                    replacement: None,
                    consume_span: None,
                });
            }
        }
    }

    /// Walk `block` recursively, registering each `let pat = expr;`
    /// form's binding names against the union of closure spans
    /// reachable from the RHS. Round 12.37 generalisation of the
    /// round-12.35 binding-name → closure-span map: the RHS may now
    /// be a direct closure (`let h = || cfg.x;`), a composite literal
    /// containing closures (`let holder = Holder { f: || cfg.x };`),
    /// a tuple of closures (`let pair = (|| cfg.x, || cfg.y);`), or
    /// an identifier referencing a previously-let-bound closure-
    /// carrying value (`let h2 = h;` — propagates `h`'s span set to
    /// `h2`). The RHS walk reuses `collect_escape_target` because the
    /// shapes of "what counts as a closure embedded in this
    /// expression" are exactly the same as for the escape-destination
    /// resolver — anywhere a closure surfaces in a return target also
    /// surfaces in a let RHS. Source-order processing of statements
    /// ensures that an identifier on the RHS resolves against an
    /// already-built map.
    fn collect_closure_let_bindings(block: &Block, out: &mut HashMap<String, Vec<SpanKey>>) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let { pattern, value, .. } => {
                    // First walk into the RHS for any nested let
                    // bindings (e.g., `let h = { let inner = ||...;
                    // inner };`), so identifier resolution inside
                    // `value` can see them.
                    Self::collect_closure_let_bindings_in_expr(value, out);
                    let mut spans: Vec<SpanKey> = Vec::new();
                    Self::collect_escape_target(value, out, &mut spans);
                    if !spans.is_empty() {
                        for name in pattern.binding_names() {
                            out.entry(name).or_default().extend(spans.iter().copied());
                        }
                    }
                }
                StmtKind::LetUninit { .. } => {}
                StmtKind::LetElse {
                    value, else_block, ..
                } => {
                    Self::collect_closure_let_bindings_in_expr(value, out);
                    Self::collect_closure_let_bindings(else_block, out);
                }
                StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                    Self::collect_closure_let_bindings(body, out);
                }
                StmtKind::Assign { value, .. } | StmtKind::CompoundAssign { value, .. } => {
                    Self::collect_closure_let_bindings_in_expr(value, out);
                }
                StmtKind::Expr(e) => {
                    Self::collect_closure_let_bindings_in_expr(e, out);
                }
            }
        }
        if let Some(tail) = &block.final_expr {
            Self::collect_closure_let_bindings_in_expr(tail, out);
        }
    }

    fn collect_closure_let_bindings_in_expr(expr: &Expr, out: &mut HashMap<String, Vec<SpanKey>>) {
        match &expr.kind {
            ExprKind::Block(b) => Self::collect_closure_let_bindings(b, out),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                Self::collect_closure_let_bindings_in_expr(condition, out);
                Self::collect_closure_let_bindings(then_block, out);
                if let Some(e) = else_branch {
                    Self::collect_closure_let_bindings_in_expr(e, out);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                Self::collect_closure_let_bindings_in_expr(value, out);
                Self::collect_closure_let_bindings(then_block, out);
                if let Some(e) = else_branch {
                    Self::collect_closure_let_bindings_in_expr(e, out);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                Self::collect_closure_let_bindings_in_expr(scrutinee, out);
                for arm in arms {
                    Self::collect_closure_let_bindings_in_expr(&arm.body, out);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                Self::collect_closure_let_bindings_in_expr(condition, out);
                Self::collect_closure_let_bindings(body, out);
            }
            ExprKind::WhileLet { value, body, .. } => {
                Self::collect_closure_let_bindings_in_expr(value, out);
                Self::collect_closure_let_bindings(body, out);
            }
            ExprKind::For { iterable, body, .. } => {
                Self::collect_closure_let_bindings_in_expr(iterable, out);
                Self::collect_closure_let_bindings(body, out);
            }
            ExprKind::Loop { body, .. } => {
                Self::collect_closure_let_bindings(body, out);
            }
            ExprKind::Par(b) | ExprKind::Seq(b) | ExprKind::Unsafe(b) | ExprKind::Try(b) => {
                Self::collect_closure_let_bindings(b, out);
            }
            ExprKind::Lock { body, .. } => Self::collect_closure_let_bindings(body, out),
            ExprKind::Providers { body, .. } => Self::collect_closure_let_bindings(body, out),
            // No closure-let registration descends into a closure body
            // — closures form a fresh scope; inner let-bound closures
            // belong to the inner scope's escape analysis, run
            // separately by `check_function` for that closure's own
            // outer function (which is this function — but the inner
            // closure's binding name is local to the inner closure
            // body and cannot be returned from this function).
            ExprKind::Closure { .. } => {}
            _ => {}
        }
    }

    /// Walk `block` recursively to find escape sites — every
    /// `return Some(target)` statement and the function-body tail-
    /// expression form. For each, route through `collect_escape_target`
    /// to resolve to a closure span if the target is a closure
    /// expression directly OR an identifier referencing a closure-let
    /// binding. Tail expressions that nest (the `then` / `else` of an
    /// `if`, match arms, block bodies) are followed transitively so
    /// `if cond { return || foo } else { || foo }` covers both arms.
    fn collect_escaping_closures(
        block: &Block,
        closure_lets: &HashMap<String, Vec<SpanKey>>,
        out: &mut Vec<SpanKey>,
    ) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let { value, .. } => {
                    Self::collect_escaping_closures_in_expr(value, closure_lets, out);
                }
                StmtKind::LetUninit { .. } => {}
                StmtKind::LetElse {
                    value, else_block, ..
                } => {
                    Self::collect_escaping_closures_in_expr(value, closure_lets, out);
                    Self::collect_escaping_closures(else_block, closure_lets, out);
                }
                StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                    Self::collect_escaping_closures(body, closure_lets, out);
                }
                StmtKind::Assign { target, value } => {
                    Self::collect_escaping_closures_in_expr(target, closure_lets, out);
                    Self::collect_escaping_closures_in_expr(value, closure_lets, out);
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    Self::collect_escaping_closures_in_expr(target, closure_lets, out);
                    Self::collect_escaping_closures_in_expr(value, closure_lets, out);
                }
                StmtKind::Expr(e) => {
                    Self::collect_escaping_closures_in_expr(e, closure_lets, out);
                }
            }
        }
        if let Some(tail) = &block.final_expr {
            Self::collect_escaping_closures_in_expr(tail, closure_lets, out);
        }
    }

    fn collect_escaping_closures_in_expr(
        expr: &Expr,
        closure_lets: &HashMap<String, Vec<SpanKey>>,
        out: &mut Vec<SpanKey>,
    ) {
        match &expr.kind {
            ExprKind::Return(Some(inner)) => {
                Self::collect_escape_target(inner, closure_lets, out);
            }
            ExprKind::Block(b) => Self::collect_escaping_closures(b, closure_lets, out),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                Self::collect_escaping_closures_in_expr(condition, closure_lets, out);
                Self::collect_escaping_closures(then_block, closure_lets, out);
                if let Some(e) = else_branch {
                    Self::collect_escaping_closures_in_expr(e, closure_lets, out);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                Self::collect_escaping_closures_in_expr(value, closure_lets, out);
                Self::collect_escaping_closures(then_block, closure_lets, out);
                if let Some(e) = else_branch {
                    Self::collect_escaping_closures_in_expr(e, closure_lets, out);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                Self::collect_escaping_closures_in_expr(scrutinee, closure_lets, out);
                for arm in arms {
                    Self::collect_escaping_closures_in_expr(&arm.body, closure_lets, out);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                Self::collect_escaping_closures_in_expr(condition, closure_lets, out);
                Self::collect_escaping_closures(body, closure_lets, out);
            }
            ExprKind::WhileLet { value, body, .. } => {
                Self::collect_escaping_closures_in_expr(value, closure_lets, out);
                Self::collect_escaping_closures(body, closure_lets, out);
            }
            ExprKind::For { iterable, body, .. } => {
                Self::collect_escaping_closures_in_expr(iterable, closure_lets, out);
                Self::collect_escaping_closures(body, closure_lets, out);
            }
            ExprKind::Loop { body, .. } => {
                Self::collect_escaping_closures(body, closure_lets, out);
            }
            ExprKind::Par(b) | ExprKind::Seq(b) | ExprKind::Unsafe(b) | ExprKind::Try(b) => {
                Self::collect_escaping_closures(b, closure_lets, out);
            }
            ExprKind::Lock { body, .. } => Self::collect_escaping_closures(body, closure_lets, out),
            ExprKind::Providers { body, .. } => {
                Self::collect_escaping_closures(body, closure_lets, out)
            }
            // Do not recurse into closure bodies — inner closures'
            // returns belong to the inner scope's body walk (the
            // enclosing fn-level analysis sees only the outer function's
            // returns).
            ExprKind::Closure { .. } => {}
            _ => {}
        }
    }

    /// Resolve an escape target expression to a closure span. The
    /// target may be: (a) a `Closure { .. }` expression directly, in
    /// which case its span is the closure span; (b) an `Identifier(n)`
    /// referencing a closure-let binding, in which case the let-RHS
    /// span is the closure span; (c) a nested if/match whose tail
    /// expressions are recursively resolved (the `if cond { || ... }
    /// else { other_closure_let }` shape produces two escape entries);
    /// (d) a composite literal (struct / tuple / array / vec / map /
    /// repeat) whose elements are recursively resolved — round 12.36
    /// extension covering the `return Holder { f: || cfg.value };`,
    /// `return (|| cfg.x, || cfg.y);`, `return [|| cfg.value];` shapes
    /// where the closure is a sub-expression of an escaping return.
    /// Anything else (function calls, field access, index, pipe) is
    /// silently ignored — those escape destinations require either
    /// inter-procedural analysis or projection-tracking and are
    /// deferred to a further follow-up.
    fn collect_escape_target(
        target: &Expr,
        closure_lets: &HashMap<String, Vec<SpanKey>>,
        out: &mut Vec<SpanKey>,
    ) {
        match &target.kind {
            ExprKind::Closure { .. } => {
                out.push(SpanKey::from_span(&target.span));
            }
            ExprKind::Identifier(name) => {
                if let Some(keys) = closure_lets.get(name) {
                    out.extend(keys.iter().copied());
                }
            }
            ExprKind::Block(b) => {
                if let Some(tail) = &b.final_expr {
                    Self::collect_escape_target(tail, closure_lets, out);
                }
            }
            ExprKind::If {
                then_block,
                else_branch,
                ..
            } => {
                if let Some(tail) = &then_block.final_expr {
                    Self::collect_escape_target(tail, closure_lets, out);
                }
                if let Some(e) = else_branch {
                    Self::collect_escape_target(e, closure_lets, out);
                }
            }
            ExprKind::IfLet {
                then_block,
                else_branch,
                ..
            } => {
                if let Some(tail) = &then_block.final_expr {
                    Self::collect_escape_target(tail, closure_lets, out);
                }
                if let Some(e) = else_branch {
                    Self::collect_escape_target(e, closure_lets, out);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    Self::collect_escape_target(&arm.body, closure_lets, out);
                }
            }
            // Round 12.36 — composite literal sub-cases. A closure that
            // sits inside a struct / tuple / array / vec / map / repeat
            // literal which is itself the operand of an escaping return
            // also escapes — the wrapping literal is constructed in the
            // current scope and immediately handed off to the caller.
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    Self::collect_escape_target(&f.value, closure_lets, out);
                }
                if let Some(s) = spread {
                    Self::collect_escape_target(s, closure_lets, out);
                }
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    Self::collect_escape_target(e, closure_lets, out);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    Self::collect_escape_target(e, closure_lets, out);
                }
            }
            ExprKind::RepeatLiteral { value, .. } => {
                // The `count` is an integer literal (compile-time), so
                // a closure can only sit in `value`. Recurse there
                // only.
                Self::collect_escape_target(value, closure_lets, out);
            }
            ExprKind::MapLiteral(pairs) => {
                for (k, v) in pairs {
                    Self::collect_escape_target(k, closure_lets, out);
                    Self::collect_escape_target(v, closure_lets, out);
                }
            }
            _ => {}
        }
    }

    /// Round 12.39 — function-level opt-out for the conservative
    /// fn-arg-pass escape check. `#[allow(ref_capture_escape)]` on
    /// the enclosing function suppresses E0508 emissions for sub-
    /// case (d) (closures with ref captures passed as Own-mode fn-
    /// args). Mirrors the `#[allow(rc_fallback)]` shape used
    /// elsewhere in this file. The other Step 7 sub-cases (return,
    /// composite-literal, let-bound-carrier escape) are NOT covered
    /// by this opt-out — those represent unambiguous escapes the
    /// programmer should always see.
    fn fn_allows_ref_capture_escape(f: &Function) -> bool {
        f.attributes.iter().any(|a| {
            a.is_bare("allow")
                && a.args.iter().any(|arg| {
                    if let Some(Expr {
                        kind: ExprKind::Identifier(name),
                        ..
                    }) = &arg.value
                    {
                        name == "ref_capture_escape"
                    } else {
                        false
                    }
                })
        })
    }

    /// Round 12.39 — walk the function body for `Call` expressions
    /// and, for each Own-mode argument position whose actual argument
    /// resolves through `collect_escape_target` to one or more
    /// closure spans, register those spans for the standard E0508
    /// firing. Borrow-mode positions (`ref Fn(...)` / `mut ref
    /// Fn(...)`) are skipped — the callee borrows the closure for
    /// the duration of the call and cannot store it beyond return.
    /// Method calls, indirect calls through function-typed bindings,
    /// and calls to functions absent from `callee_param_modes` (for
    /// which we have no per-position mode info) are skipped — the
    /// conservative-fire applies only where we have a known free-
    /// function signature with explicit parameter modes. This
    /// matches the `arg_is_borrow_position` lookup shape already
    /// used by `check_call`.
    fn collect_call_arg_escape_closures(
        &self,
        block: &Block,
        closure_lets: &HashMap<String, Vec<SpanKey>>,
        out: &mut Vec<SpanKey>,
    ) {
        Self::walk_block_for_calls(block, &mut |callee, args| {
            let modes = match self.callee_modes_for_call(callee) {
                Some(m) => m,
                None => return,
            };
            for (i, arg) in args.iter().enumerate() {
                let mode = match modes.get(i) {
                    Some(m) => m,
                    None => continue,
                };
                if !matches!(mode, OwnershipMode::Own) {
                    continue;
                }
                Self::collect_escape_target(&arg.value, closure_lets, out);
            }
        });
    }

    /// Walk a block recursively, invoking `visit` at every `Call`
    /// expression with the callee and the arg list. Used by round
    /// 12.39's fn-arg-pass scan; structurally similar to the existing
    /// escape walkers but visit-pattern-keyed instead of the
    /// closure-collection pattern.
    fn walk_block_for_calls(block: &Block, visit: &mut impl FnMut(&Expr, &[CallArg])) {
        for stmt in &block.stmts {
            Self::walk_stmt_for_calls(stmt, visit);
        }
        if let Some(tail) = &block.final_expr {
            Self::walk_expr_for_calls(tail, visit);
        }
    }

    fn walk_stmt_for_calls(stmt: &Stmt, visit: &mut impl FnMut(&Expr, &[CallArg])) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => Self::walk_expr_for_calls(value, visit),
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                Self::walk_expr_for_calls(value, visit);
                Self::walk_block_for_calls(else_block, visit);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                Self::walk_block_for_calls(body, visit);
            }
            StmtKind::Assign { target, value } => {
                Self::walk_expr_for_calls(target, visit);
                Self::walk_expr_for_calls(value, visit);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                Self::walk_expr_for_calls(target, visit);
                Self::walk_expr_for_calls(value, visit);
            }
            StmtKind::Expr(e) => Self::walk_expr_for_calls(e, visit),
        }
    }

    fn walk_expr_for_calls(expr: &Expr, visit: &mut impl FnMut(&Expr, &[CallArg])) {
        match &expr.kind {
            ExprKind::Call { callee, args } => {
                visit(callee, args);
                Self::walk_expr_for_calls(callee, visit);
                for arg in args {
                    Self::walk_expr_for_calls(&arg.value, visit);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                Self::walk_expr_for_calls(object, visit);
                for arg in args {
                    Self::walk_expr_for_calls(&arg.value, visit);
                }
            }
            ExprKind::Block(b) => Self::walk_block_for_calls(b, visit),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                Self::walk_expr_for_calls(condition, visit);
                Self::walk_block_for_calls(then_block, visit);
                if let Some(e) = else_branch {
                    Self::walk_expr_for_calls(e, visit);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                Self::walk_expr_for_calls(value, visit);
                Self::walk_block_for_calls(then_block, visit);
                if let Some(e) = else_branch {
                    Self::walk_expr_for_calls(e, visit);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                Self::walk_expr_for_calls(scrutinee, visit);
                for arm in arms {
                    Self::walk_expr_for_calls(&arm.body, visit);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                Self::walk_expr_for_calls(condition, visit);
                Self::walk_block_for_calls(body, visit);
            }
            ExprKind::WhileLet { value, body, .. } => {
                Self::walk_expr_for_calls(value, visit);
                Self::walk_block_for_calls(body, visit);
            }
            ExprKind::For { iterable, body, .. } => {
                Self::walk_expr_for_calls(iterable, visit);
                Self::walk_block_for_calls(body, visit);
            }
            ExprKind::Loop { body, .. } => Self::walk_block_for_calls(body, visit),
            ExprKind::Par(b) | ExprKind::Seq(b) | ExprKind::Unsafe(b) | ExprKind::Try(b) => {
                Self::walk_block_for_calls(b, visit);
            }
            ExprKind::Lock { body, .. } => Self::walk_block_for_calls(body, visit),
            ExprKind::Providers { body, .. } => Self::walk_block_for_calls(body, visit),
            ExprKind::Return(Some(inner))
            | ExprKind::Break {
                value: Some(inner), ..
            } => Self::walk_expr_for_calls(inner, visit),
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    Self::walk_expr_for_calls(e, visit);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    Self::walk_expr_for_calls(e, visit);
                }
            }
            ExprKind::RepeatLiteral { value, .. } => {
                Self::walk_expr_for_calls(value, visit);
            }
            ExprKind::MapLiteral(pairs) => {
                for (k, v) in pairs {
                    Self::walk_expr_for_calls(k, visit);
                    Self::walk_expr_for_calls(v, visit);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for fld in fields {
                    Self::walk_expr_for_calls(&fld.value, visit);
                }
                if let Some(s) = spread {
                    Self::walk_expr_for_calls(s, visit);
                }
            }
            ExprKind::Binary { left, right, .. } | ExprKind::NilCoalesce { left, right } => {
                Self::walk_expr_for_calls(left, visit);
                Self::walk_expr_for_calls(right, visit);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Question(operand) => {
                Self::walk_expr_for_calls(operand, visit);
            }
            ExprKind::OptionalChain { object, args, .. } => {
                Self::walk_expr_for_calls(object, visit);
                if let Some(args) = args {
                    for arg in args {
                        Self::walk_expr_for_calls(&arg.value, visit);
                    }
                }
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                Self::walk_expr_for_calls(object, visit);
            }
            ExprKind::Index { object, index } => {
                Self::walk_expr_for_calls(object, visit);
                Self::walk_expr_for_calls(index, visit);
            }
            ExprKind::Pipe { left, right } => {
                Self::walk_expr_for_calls(left, visit);
                Self::walk_expr_for_calls(right, visit);
            }
            // Closures form a fresh scope; their bodies' calls are
            // analyzed when we run check_function on them — wait,
            // actually closures don't get their own check_function
            // invocation today. Their bodies are walked as part of
            // the outer fn's check_block. Skip recursion here so
            // a closure bound to a let in the outer fn doesn't
            // double-process its body's calls — those calls already
            // execute in a different scope (the closure's invocation
            // frame), and conservative-fire on outer-fn calls
            // shouldn't see them.
            ExprKind::Closure { .. } => {}
            _ => {}
        }
    }
}
