//! Block / par-block / statement evaluation and cleanup.
//!
//! Houses `eval_block_inner` (sequential block body + drop/defer
//! cleanup stack), `eval_par_block` (par-block lowering — task
//! cluster fork/join), `eval_stmt_cf` (one-statement dispatch),
//! `dispatch_lowered_op` (rewriting typechecker-lowered operator
//! method calls back into binop/unary), and the cleanup helpers
//! `run_cleanup`, `fire_due_drops`, `observed_cancellation`, and
//! `signal_cancellation_if_error`.
//!
//! Lives in a sibling `impl<'a> super::Interpreter<'a>` block.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::ast::*;
use crate::token::Span;

use super::exec::{
    compute_block_last_use, push_drops_for_stmt, CleanupAction, ControlFlow, ErrDeferEntry,
    EvalResult, ExitPath,
};
use super::value::Value;
use super::Interpreter;

impl<'a> super::Interpreter<'a> {
    #[allow(clippy::result_large_err)]
    pub(crate) fn eval_block_inner(&mut self, block: &Block) -> EvalResult {
        self.env.push_scope();
        // Unified drop+defer cleanup stack — entries pushed in program-order
        // as control flow reaches each binding/defer statement, drained LIFO
        // at scope exit. Per design.md § Drop ordering within a branch:
        // destructors and `defer` blocks interleave in this single stack,
        // ordered by program-order of introduction. `errdefer` lives on a
        // separate phase-1 stack that drains first on error paths.
        let mut cleanup: Vec<CleanupAction> = Vec::new();
        let mut errdefers: Vec<ErrDeferEntry> = Vec::new();
        // Sub-step 3 (NLL placement): pre-compute each owned binding's
        // last-use statement index. After every successful statement,
        // any `Drop` slot whose binding's last use was that statement
        // fires immediately (and is removed from `cleanup`), instead
        // of waiting for scope exit. Bindings whose last-use is the
        // sentinel `stmts.len()` (referenced in `final_expr` or in a
        // defer/errdefer body) stay in `cleanup` and drain via the
        // unified LIFO at scope exit, preserving the program-order
        // interleave with Defers for that case.
        let last_use = compute_block_last_use(block);

        for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
            // `defer` / `errdefer` register their bodies at the moment
            // control flow reaches the statement — *not* at block start.
            // A defer below an early `return` is therefore never registered,
            // matching design.md (and Go/Zig semantics).
            match &stmt.kind {
                StmtKind::Defer { body } => {
                    cleanup.push(CleanupAction::Defer(body.clone()));
                    continue;
                }
                StmtKind::ErrDefer { binding, body } => {
                    errdefers.push(ErrDeferEntry {
                        binding: binding.clone(),
                        body: body.clone(),
                    });
                    continue;
                }
                _ => {}
            }
            // par {}-cancellation effect-boundary check. When this
            // interpreter is acting as a sibling branch and another
            // sibling has signalled fail-fast, raise Cancelled so the
            // active scope's errdefer phase fires with e = Cancelled.
            if self.observed_cancellation() {
                let cf = ControlFlow::Cancelled;
                let path = ExitPath::classify(&cf);
                self.run_cleanup(&cleanup, &errdefers, &path);
                self.env.pop_scope();
                return Err(cf);
            }
            let stmt_result = self.eval_stmt_cf(stmt);
            let cf_opt = match stmt_result {
                Ok(_) => self.pending_cf.take(),
                Err(cf) => Some(cf),
            };
            if let Some(cf) = cf_opt {
                let path = ExitPath::classify(&cf);
                // Notify sibling par-branches as soon as the error
                // path is detected, not after the branch finishes —
                // that way a still-running sibling can observe the
                // flag at its next between-statement check.
                self.signal_cancellation_if_error(&cf);
                self.run_cleanup(&cleanup, &errdefers, &path);
                self.env.pop_scope();
                return Err(cf);
            }
            // After a successful let-binding, push a Drop slot for each
            // name the pattern introduced.
            push_drops_for_stmt(stmt, &mut cleanup);
            // NLL placement: fire any Drop slot whose binding's last
            // use was this statement, then remove it from `cleanup`
            // so it does not fire again at scope exit. A binding that
            // is never read (last_use == its own let stmt_idx) drops
            // here too — that's the "let _ = expensive(); …" case
            // where NLL says the value dies at its declaration.
            self.fire_due_drops(&mut cleanup, &last_use, stmt_idx);
        }
        let result = if let Some(ref expr) = block.final_expr {
            if self.observed_cancellation() {
                let cf = ControlFlow::Cancelled;
                let path = ExitPath::classify(&cf);
                self.run_cleanup(&cleanup, &errdefers, &path);
                self.env.pop_scope();
                return Err(cf);
            }
            let v = self.eval_expr_inner(expr);
            if let Some(cf) = self.pending_cf.take() {
                let path = ExitPath::classify(&cf);
                self.signal_cancellation_if_error(&cf);
                self.run_cleanup(&cleanup, &errdefers, &path);
                self.env.pop_scope();
                return Err(cf);
            }
            v
        } else {
            Value::Unit
        };
        // Normal exit — drop+defer phase only.
        self.run_cleanup(&cleanup, &errdefers, &ExitPath::Normal);
        self.env.pop_scope();
        Ok(result)
    }

    /// Execute a `par {}` block with parallel execution.
    /// Each top-level statement in the block becomes a concurrent branch.
    /// Fail-fast: first error cancels all siblings.
    #[allow(clippy::result_large_err)]
    pub(crate) fn eval_par_block(&mut self, block: &Block) -> EvalResult {
        let stmts = &block.stmts;

        // Single or zero statements — no parallelism needed
        if stmts.len() <= 1 {
            return self.eval_block_inner(block);
        }

        // Snapshot current environment for all branches
        let env_snapshot = self.env.snapshot();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let program = self.program;
        let typecheck_result = self.typecheck_result;
        let sequential_mode = self.sequential_mode;
        let source_filename = &self.source_filename;
        let source_text = &self.source_text;
        let dbg_output_mode = self.dbg_output_mode;
        let task_id_counter = Arc::clone(&self.task_id_counter);
        let parent_captures_dbg = self.captured_dbg.is_some();
        // Pre-allocate task ids in source order so a given branch always
        // reports the same task_id regardless of OS scheduling. The
        // counter is a monotonic Arc shared across nested par blocks; we
        // claim a contiguous range here, then each branch reads its
        // pre-assigned slot below.
        let branch_task_ids: Vec<u64> = (0..stmts.len())
            .map(|_| task_id_counter.fetch_add(1, Ordering::Relaxed) + 1)
            .collect();

        // Collect results from each branch
        // Each branch result: (index, defined_vars, output_lines, dbg_lines, control_flow_or_value)
        type BranchResult = (
            usize,
            HashMap<String, Value>,
            Vec<String>,
            Vec<String>,
            Result<Value, ControlFlow>,
        );
        let results: Mutex<Vec<BranchResult>> = Mutex::new(Vec::new());

        std::thread::scope(|s| {
            for (i, stmt) in stmts.iter().enumerate() {
                let env_snap = &env_snapshot;
                let cancel = Arc::clone(&cancel_flag);
                let prog = &program;
                let tc = &typecheck_result;
                let results_ref = &results;
                let stmt_clone = stmt.clone();
                let task_id_counter = Arc::clone(&task_id_counter);
                let task_id = branch_task_ids[i];
                s.spawn(move || {
                    // Pre-start cancellation observation: a sibling already
                    // failed before this branch was scheduled. The branch
                    // never enters its body, so no errdefers are registered
                    // and no cleanup runs — push nothing.
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }

                    // Create a branch interpreter with the shared env snapshot
                    let mut branch_interp = Interpreter::new(prog, tc);
                    branch_interp.captured_output = Some(Vec::new());
                    branch_interp.sequential_mode = sequential_mode;
                    branch_interp.source_filename = source_filename.clone();
                    branch_interp.cancel_flag = Some(Arc::clone(&cancel));
                    branch_interp.source_text = source_text.clone();
                    branch_interp.dbg_output_mode = dbg_output_mode;
                    branch_interp.task_id_counter = Arc::clone(&task_id_counter);
                    // Task id is pre-assigned in source order above so
                    // dbg() output reports a stable id for a given
                    // branch regardless of OS scheduling. Counter
                    // starts at 1 (id 0 is the "no par" sentinel,
                    // never reported as an actual task tag).
                    branch_interp.current_task_id = Some(task_id);
                    if parent_captures_dbg {
                        branch_interp.captured_dbg = Some(Vec::new());
                    }

                    // Restore environment snapshot
                    for (k, v) in env_snap {
                        branch_interp.env.define(k.clone(), v.clone());
                    }
                    // Register top-level items so function calls work
                    branch_interp.register_items();

                    // Execute the statement
                    let result = branch_interp.eval_stmt_cf(&stmt_clone);
                    // Also check pending_cf
                    let cf_result = if let Some(cf) = branch_interp.pending_cf.take() {
                        Err(cf)
                    } else {
                        result.map(|_| Value::Unit)
                    };

                    // On error, set cancel flag for fail-fast
                    if cf_result.is_err() {
                        cancel.store(true, Ordering::Relaxed);
                    }

                    // Collect defined variables from this branch (top scope only)
                    let defined_vars = if let Some(scope) = branch_interp.env.scopes.last() {
                        scope.clone()
                    } else {
                        HashMap::new()
                    };

                    let output = branch_interp.captured_output.unwrap_or_default();
                    let dbg_lines = branch_interp.captured_dbg.unwrap_or_default();

                    results_ref.lock().unwrap().push((
                        i,
                        defined_vars,
                        output,
                        dbg_lines,
                        cf_result,
                    ));
                });
            }
        });

        // Sort results by source order (deterministic)
        let mut branch_results = results.into_inner().unwrap();
        branch_results.sort_by_key(|(i, _, _, _, _)| *i);

        // Merge results back into the parent interpreter
        // 1. Merge output in source order
        for (_, _, output, _, _) in &branch_results {
            for line in output {
                if let Some(ref mut cap) = self.captured_output {
                    cap.push(line.clone());
                } else {
                    print!("{}", line);
                }
            }
        }

        // 1b. Merge dbg lines in source order (test-only; only present
        // when the parent has an active capture buffer).
        if let Some(ref mut cap) = self.captured_dbg {
            for (_, _, _, dbg_lines, _) in &branch_results {
                for line in dbg_lines {
                    cap.push(line.clone());
                }
            }
        }

        // 2. Merge defined variables
        self.env.push_scope();
        for (_, vars, _, _, _) in &branch_results {
            for (name, val) in vars {
                // Skip prelude/function definitions
                if matches!(val, Value::Function { .. } | Value::EnumVariant { .. }) {
                    continue;
                }
                self.env.define(name.clone(), val.clone());
            }
        }

        // 3. Check for errors (fail-fast: first error in source order).
        // `ControlFlow::Cancelled` is silenced — a cancelled sibling's
        // cleanup already ran with `e = Cancelled`, but the originating
        // branch's real `Err` is what propagates as the scope's value.
        for (_, _, _, _, result) in branch_results {
            if let Err(cf) = result {
                if matches!(cf, ControlFlow::Cancelled) {
                    continue;
                }
                self.env.pop_scope();
                return Err(cf);
            }
        }

        // 4. Final expression (par blocks don't have a final_expr in current design)
        let result = if let Some(ref expr) = block.final_expr {
            let v = self.eval_expr_inner(expr);
            if let Some(cf) = self.pending_cf.take() {
                self.env.pop_scope();
                return Err(cf);
            }
            v
        } else {
            Value::Unit
        };
        self.env.pop_scope();
        Ok(result)
    }

    /// True iff this interpreter is acting as a `par {}` sibling branch
    /// and a peer has signalled fail-fast cancellation.
    fn observed_cancellation(&self) -> bool {
        self.cancel_flag
            .as_ref()
            .map(|f| f.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Set the shared `par {}` cancel flag (if any) when the active
    /// scope is unwinding on an error path. Cancellation is itself an
    /// error path but the store is idempotent.
    fn signal_cancellation_if_error(&self, cf: &ControlFlow) {
        let is_error_path = matches!(
            cf,
            ControlFlow::Return(Value::EnumVariant { variant, .. })
                if variant == "Err" || variant == "None"
        ) || matches!(
            cf,
            ControlFlow::RuntimeError | ControlFlow::ExitUnwind { .. } | ControlFlow::Cancelled
        );
        if is_error_path {
            if let Some(ref flag) = self.cancel_flag {
                flag.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Drain the unified drop+defer cleanup stack at scope exit per
    /// design.md § Drop ordering within a branch. Two phases:
    ///
    /// 1. `errdefer` phase (error paths only). Param-less `errdefer { ... }`
    ///    runs on every error path. `errdefer(e) { ... }` binds `e` to the
    ///    propagating `Err` payload (or `Cancelled` in cancelled siblings —
    ///    sub-step 4 wires that branch). `errdefer(e)` is skipped on panic
    ///    per the language rules.
    /// 2. drop+defer phase (always). Drains the unified stack LIFO so
    ///    `let x = ...; defer foo();` cleans up as `foo()` then `drop(x)`.
    ///    `Drop` actions record on `drop_trace`; once user-`impl Drop`
    ///    dispatch lands, observable side effects attach here without
    ///    changing the program-order LIFO position.
    fn run_cleanup(
        &mut self,
        cleanup: &[CleanupAction],
        errdefers: &[ErrDeferEntry],
        path: &ExitPath,
    ) {
        // Phase 1: errdefer. Reverse declaration order; param-less runs on
        // every error path, errdefer(e) binds the Err payload (skipped on
        // panic — only param-less fires there).
        if path.is_error() {
            for entry in errdefers.iter().rev() {
                match &entry.binding {
                    Some(name) => match path {
                        ExitPath::Err(payload) | ExitPath::Cancelled(payload) => {
                            self.env.push_scope();
                            self.env.define(name.clone(), payload.clone());
                            let _ = self.eval_block_inner(&entry.body);
                            self.env.pop_scope();
                        }
                        ExitPath::Panic | ExitPath::NoneProp | ExitPath::Normal => {
                            // errdefer(e) is skipped on panic and on bare
                            // None propagation (no payload to bind).
                        }
                    },
                    None => {
                        let _ = self.eval_block_inner(&entry.body);
                    }
                }
            }
        }
        // Phase 2: drop+defer interleaved LIFO.
        for action in cleanup.iter().rev() {
            match action {
                CleanupAction::Defer(body) => {
                    let _ = self.eval_block_inner(body);
                }
                CleanupAction::Drop { name } => {
                    // The drop itself is a no-op until user-`impl Drop`
                    // dispatch lands. The trace records the firing for
                    // sub-step 3 (NLL placement) test verification.
                    self.drop_trace.push(name.clone());
                }
            }
        }
    }

    /// Fire any `Drop` slot whose binding's last use was the just-
    /// finished statement, and remove it from `cleanup` so it does
    /// not fire again at scope exit. NLL placement per design.md §
    /// Drop ordering within a branch (sub-step 3). `Defer` slots
    /// always stay in `cleanup` and drain at scope exit. Walks
    /// `cleanup` front-to-back so program-order is preserved on
    /// in-place removal; the relative LIFO order of remaining
    /// entries is unchanged. Drop firings are recorded on
    /// `drop_trace` directly here (rather than via `run_cleanup`)
    /// so test traces include NLL and scope-exit firings in their
    /// actual program order.
    fn fire_due_drops(
        &mut self,
        cleanup: &mut Vec<CleanupAction>,
        last_use: &HashMap<String, usize>,
        stmt_idx: usize,
    ) {
        let mut i = 0;
        while i < cleanup.len() {
            let should_fire = match &cleanup[i] {
                CleanupAction::Drop { name } => last_use.get(name).copied() == Some(stmt_idx),
                CleanupAction::Defer(_) => false,
            };
            if should_fire {
                let action = cleanup.remove(i);
                if let CleanupAction::Drop { name } = action {
                    self.drop_trace.push(name);
                }
            } else {
                i += 1;
            }
        }
    }

    #[allow(clippy::result_large_err)]
    fn eval_stmt_cf(&mut self, stmt: &Stmt) -> EvalResult {
        match &stmt.kind {
            StmtKind::Let { pattern, value, .. } => {
                let val = self.eval_expr_inner(value);
                if let Some(cf) = self.pending_cf.take() {
                    return Err(cf);
                }
                self.bind_pattern(pattern, val);
            }
            StmtKind::LetUninit { name, .. } => {
                // Declare the binding with a sentinel `Unit` value. Static
                // definite-assignment analysis (in `OwnershipChecker`)
                // rejects any read before the first assignment, so a
                // well-typed program never observes this sentinel.
                self.env.define(name.clone(), Value::Unit);
            }
            StmtKind::LetElse {
                pattern,
                value,
                else_block,
                ..
            } => {
                let val = self.eval_expr_inner(value);
                if self.try_match_pattern(pattern, &val) {
                    self.bind_pattern(pattern, val);
                } else {
                    self.eval_block_inner(else_block)?;
                }
            }
            StmtKind::Defer { body } => {
                // Collect for later execution — we'll run these when we have
                // a proper scope-exit mechanism. For now, run inline as a
                // simplified approximation.
                let _ = body;
            }
            StmtKind::ErrDefer { body, .. } => {
                let _ = body;
            }
            StmtKind::Assign { target, value } => {
                let val = self.eval_expr_inner(value);
                match &target.kind {
                    ExprKind::Identifier(name) => {
                        self.env.set(name, val);
                    }
                    ExprKind::FieldAccess { object, field } => {
                        self.set_field(object, field, val);
                    }
                    ExprKind::Index { object, index } => {
                        self.set_index(object, index, val);
                    }
                    // `*r = v` — rebind `r` to `v` in the current scope.
                    // In the tree-walk interpreter, mut-ref params are local
                    // bindings; the call site writes back after the call (CICO).
                    ExprKind::Unary {
                        op: crate::ast::UnaryOp::Deref,
                        operand,
                    } => {
                        if let ExprKind::Identifier(name) = &operand.kind {
                            self.env.set(name, val);
                        }
                    }
                    _ => unreachable!(
                        "unsupported assignment target at {}:{}; should be caught by parser/typechecker",
                        stmt.span.line, stmt.span.column
                    ),
                }
            }
            StmtKind::CompoundAssign { target, op, value } => {
                let current = self.eval_expr_inner(target);
                let rhs = self.eval_expr_inner(value);
                let bin_op = match op {
                    CompoundOp::Add => BinOp::Add,
                    CompoundOp::Sub => BinOp::Sub,
                    CompoundOp::Mul => BinOp::Mul,
                    CompoundOp::Div => BinOp::Div,
                    CompoundOp::Mod => BinOp::Mod,
                    CompoundOp::BitAnd => BinOp::BitAnd,
                    CompoundOp::BitOr => BinOp::BitOr,
                    CompoundOp::BitXor => BinOp::BitXor,
                    CompoundOp::Shl => BinOp::Shl,
                    CompoundOp::Shr => BinOp::Shr,
                };
                let result = self.eval_binary(&bin_op, current, rhs, &stmt.span);
                if let ExprKind::Identifier(name) = &target.kind {
                    self.env.set(name, result);
                }
            }
            StmtKind::Expr(expr) => {
                self.eval_expr_inner(expr);
                // If a control flow signal was set during expression evaluation,
                // propagate it immediately
                if let Some(cf) = self.pending_cf.take() {
                    return Err(cf);
                }
            }
        }
        Ok(Value::Unit)
    }

    // ── Call evaluation ─────────────────────────────────────────

    /// Execute a lowered primitive operator call (e.g. `i64.add(a, b)`).
    /// Returns `Some(value)` if the method matches a known intrinsic; `None`
    /// otherwise (caller falls through to other dispatch).
    pub(crate) fn dispatch_lowered_op(
        &mut self,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Option<Value> {
        // Map lowered method name back to the corresponding BinOp / UnaryOp
        // and synthesize a Binary/Unary expression that eval_binary/eval_unary
        // already knows how to execute. Reuses all existing intrinsic logic
        // (overflow trapping, division by zero, string concat, etc.).
        let bin_op = match method {
            "add" => Some(BinOp::Add),
            "sub" => Some(BinOp::Sub),
            "mul" => Some(BinOp::Mul),
            "div" => Some(BinOp::Div),
            "rem" => Some(BinOp::Mod),
            "eq" => Some(BinOp::Eq),
            "ne" => Some(BinOp::NotEq),
            "lt" => Some(BinOp::Lt),
            "le" => Some(BinOp::LtEq),
            "gt" => Some(BinOp::Gt),
            "ge" => Some(BinOp::GtEq),
            "bitand" => Some(BinOp::BitAnd),
            "bitor" => Some(BinOp::BitOr),
            "bitxor" => Some(BinOp::BitXor),
            "shl" => Some(BinOp::Shl),
            "shr" => Some(BinOp::Shr),
            _ => None,
        };
        if let Some(op) = bin_op {
            if args.len() == 2 {
                let lhs = self.eval_expr_inner(&args[0].value);
                let rhs = self.eval_expr_inner(&args[1].value);
                return Some(self.eval_binary(&op, lhs, rhs, span));
            }
        }
        if method == "neg" && args.len() == 1 {
            let val = self.eval_expr_inner(&args[0].value);
            return Some(self.eval_unary(&UnaryOp::Neg, val, span));
        }
        if method == "not" && args.len() == 1 {
            // `not` covers both `!bool` (UnaryOp::Not) and `~int` (UnaryOp::BitNot).
            // Kāra disjointly types these, so the runtime value shape is unambiguous.
            let val = self.eval_expr_inner(&args[0].value);
            let op = match &val {
                Value::Bool(_) => UnaryOp::Not,
                _ => UnaryOp::BitNot,
            };
            return Some(self.eval_unary(&op, val, span));
        }
        None
    }
}
