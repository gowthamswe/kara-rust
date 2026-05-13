// src/concurrency.rs

//! Concurrency analysis pass for the Kāra language.
//!
//! Analyzes function bodies to identify which statements can safely run in
//! parallel by building a dual-analysis dependency graph:
//! 1. **Data dependency**: if statement B reads a variable that A defines, B depends on A
//! 2. **Effect conflict**: if A and B have conflicting effects on the same resource, they
//!    must serialize
//!
//! Only when BOTH analyses find no dependency can statements be parallelized.

use crate::ast::*;
use crate::effectchecker::{DeclaredEffects, EffectCheckResult};
use std::collections::{HashMap, HashSet};

/// `true` iff this statement contains a `return`, `break`, or
/// `continue` that escapes a directly-nested expression's control flow
/// — i.e., that would, at codegen time, emit a `ret X` (or branch to a
/// loop's exit edge) bypassing the statement's "fall through" exit.
/// Used by `find_parallel_groups` to keep such statements out of
/// par groups; a par branch is lowered to a standalone `void` LLVM
/// function and an embedded `return X` from the original body would
/// produce `ret <T> X` inside the void branch and fail LLVM module
/// verification.
fn stmt_has_early_exit(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Let { value, .. }
        | StmtKind::Assign { value, .. }
        | StmtKind::CompoundAssign { value, .. }
        | StmtKind::Expr(value) => expr_has_early_exit(value),
        StmtKind::LetElse {
            value, else_block, ..
        } => expr_has_early_exit(value) || block_has_early_exit(else_block),
        StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => block_has_early_exit(body),
        StmtKind::LetUninit { .. } => false,
    }
}

fn block_has_early_exit(block: &Block) -> bool {
    block.stmts.iter().any(stmt_has_early_exit)
        || block
            .final_expr
            .as_ref()
            .is_some_and(|e| expr_has_early_exit(e))
}

fn expr_has_early_exit(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Return(_) => true,
        ExprKind::Break { .. } => true,
        ExprKind::Continue { .. } => true,
        ExprKind::Block(b) => block_has_early_exit(b),
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            expr_has_early_exit(condition)
                || block_has_early_exit(then_block)
                || else_branch.as_ref().is_some_and(|e| expr_has_early_exit(e))
        }
        ExprKind::IfLet {
            value,
            then_block,
            else_branch,
            ..
        } => {
            expr_has_early_exit(value)
                || block_has_early_exit(then_block)
                || else_branch.as_ref().is_some_and(|e| expr_has_early_exit(e))
        }
        ExprKind::Match { scrutinee, arms } => {
            expr_has_early_exit(scrutinee) || arms.iter().any(|a| expr_has_early_exit(&a.body))
        }
        ExprKind::While {
            condition, body, ..
        } => expr_has_early_exit(condition) || block_has_early_exit(body),
        ExprKind::For { iterable, body, .. } => {
            expr_has_early_exit(iterable) || block_has_early_exit(body)
        }
        ExprKind::Loop { body, .. } => block_has_early_exit(body),
        ExprKind::Binary { left, right, .. }
        | ExprKind::Pipe { left, right }
        | ExprKind::NilCoalesce { left, right } => {
            expr_has_early_exit(left) || expr_has_early_exit(right)
        }
        ExprKind::Unary { operand, .. } => expr_has_early_exit(operand),
        ExprKind::Call { callee, args } => {
            expr_has_early_exit(callee) || args.iter().any(|a| expr_has_early_exit(&a.value))
        }
        ExprKind::MethodCall { object, args, .. } => {
            expr_has_early_exit(object) || args.iter().any(|a| expr_has_early_exit(&a.value))
        }
        ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
            expr_has_early_exit(object)
        }
        ExprKind::Index { object, index } => {
            expr_has_early_exit(object) || expr_has_early_exit(index)
        }
        ExprKind::Tuple(elems) => elems.iter().any(expr_has_early_exit),
        _ => false,
    }
}

// ── Result Types ───────────────────────────────────────────────

/// The full result of concurrency analysis across all functions.
#[derive(Debug, Clone)]
pub struct ConcurrencyAnalysis {
    /// Per-function parallelization decisions.
    pub function_decisions: HashMap<String, FunctionConcurrency>,
}

/// Parallelization analysis for a single function.
#[derive(Debug, Clone)]
pub struct FunctionConcurrency {
    /// Groups of statement indices that can run in parallel.
    pub parallel_groups: Vec<ParallelGroup>,
    /// Total statements analyzed.
    pub total_statements: usize,
}

/// A set of statements that can safely run in parallel.
#[derive(Debug, Clone)]
pub struct ParallelGroup {
    /// Indices of statements in this parallel group.
    pub statement_indices: Vec<usize>,
    /// Why these can be parallelized.
    pub reason: String,
    /// True if the group is too cheap to justify thread dispatch
    /// (pure arithmetic, simple variable access, no I/O or function calls with effects).
    /// Codegen should run trivial groups inline instead of spawning tasks.
    pub is_trivial: bool,
}

// ── Internal: Per-statement metadata ───────────────────────────

/// Metadata extracted from a single statement for dependency analysis.
#[derive(Debug, Clone)]
struct StmtInfo {
    /// Variables defined (written) by this statement.
    defines: HashSet<String>,
    /// Variables read by this statement.
    reads: HashSet<String>,
    /// Effects produced by this statement (from called functions).
    effects: Vec<StmtEffect>,
    /// Whether this statement (transitively) calls a function with polymorphic
    /// effects (`with _`). Its effects are unknown at analysis time, so it must
    /// serialize conservatively against any other stmt with visible effects.
    calls_polymorphic: bool,
    /// Whether this statement is inside a seq {} block.
    is_seq: bool,
    /// Whether this statement may exit the enclosing function abnormally
    /// (an `if` body / loop body / match arm reachable through this stmt
    /// contains `return`, `break`, or `continue`). Such statements
    /// cannot share a parallel group with siblings — par branches are
    /// emitted as standalone `void` LLVM functions and a raw `ret X`
    /// from inside the branch produces invalid IR ("return instr that
    /// returns non-void in Function of void return type").
    has_early_exit: bool,
}

/// An effect associated with a statement.
#[derive(Debug, Clone)]
struct StmtEffect {
    verb: EffectVerbKind,
    resource: String,
}

// ── Checker ────────────────────────────────────────────────────

pub struct ConcurrencyChecker<'a> {
    program: &'a Program,
    effects: &'a EffectCheckResult,
    /// Function bodies collected from the program, keyed by function name.
    function_bodies: HashMap<String, &'a Function>,
    /// Impl method bodies: "TypeName.method" -> &Function.
    method_bodies: HashMap<String, &'a Function>,
}

impl<'a> ConcurrencyChecker<'a> {
    pub fn new(program: &'a Program, effects: &'a EffectCheckResult) -> Self {
        let mut checker = ConcurrencyChecker {
            program,
            effects,
            function_bodies: HashMap::new(),
            method_bodies: HashMap::new(),
        };
        checker.collect_functions();
        checker
    }

    fn collect_functions(&mut self) {
        for item in &self.program.items {
            match item {
                Item::Function(f) => {
                    self.function_bodies.insert(f.name.clone(), f);
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => match p.segments.last().cloned() {
                            Some(name) => name,
                            None => continue,
                        },
                        _ => continue,
                    };
                    for item in &imp.items {
                        if let ImplItem::Method(method) = item {
                            let key = format!("{}.{}", type_name, method.name);
                            self.method_bodies.insert(key, method);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn analyze(self) -> ConcurrencyAnalysis {
        let mut decisions = HashMap::new();

        for item in &self.program.items {
            if let Item::Function(f) = item {
                let fc = self.analyze_function(f);
                decisions.insert(f.name.clone(), fc);
            }
        }

        // Also analyze impl methods
        for item in &self.program.items {
            if let Item::ImplBlock(imp) = item {
                let type_name = match &imp.target_type.kind {
                    TypeKind::Path(p) => match p.segments.last().cloned() {
                        Some(name) => name,
                        None => continue,
                    },
                    _ => continue,
                };
                for impl_item in &imp.items {
                    if let ImplItem::Method(method) = impl_item {
                        let key = format!("{}.{}", type_name, method.name);
                        let fc = self.analyze_function(method);
                        decisions.insert(key, fc);
                    }
                }
            }
        }

        ConcurrencyAnalysis {
            function_decisions: decisions,
        }
    }

    fn analyze_function(&self, func: &Function) -> FunctionConcurrency {
        let stmts = &func.body.stmts;
        let total_statements = stmts.len();

        if total_statements == 0 {
            return FunctionConcurrency {
                parallel_groups: Vec::new(),
                total_statements: 0,
            };
        }

        // Step 1: Extract metadata for each statement
        let stmt_infos: Vec<StmtInfo> = stmts.iter().map(|s| self.analyze_stmt(s, false)).collect();

        // Step 2: Build dependency graph (adjacency list of conflicts)
        // dep_edges[i] contains all j where i depends on j (or they must serialize)
        let mut has_edge = vec![vec![false; total_statements]; total_statements];

        for i in 0..total_statements {
            for j in 0..i {
                if self.statements_conflict(&stmt_infos[j], &stmt_infos[i]) {
                    has_edge[i][j] = true;
                    has_edge[j][i] = true;
                }
            }
        }

        // Step 3: Find maximal independent sets (greedy graph coloring approach)
        // We group statements that have no edges between them.
        let parallel_groups = self.find_parallel_groups(&stmt_infos, &has_edge, total_statements);

        FunctionConcurrency {
            parallel_groups,
            total_statements,
        }
    }

    /// Analyze a single statement to extract defines, reads, and effects.
    fn analyze_stmt(&self, stmt: &Stmt, is_seq: bool) -> StmtInfo {
        let mut info = StmtInfo {
            defines: HashSet::new(),
            reads: HashSet::new(),
            effects: Vec::new(),
            calls_polymorphic: false,
            is_seq,
            has_early_exit: stmt_has_early_exit(stmt),
        };

        match &stmt.kind {
            StmtKind::Let { pattern, value, .. } => {
                // The pattern defines variables
                self.collect_pattern_bindings(pattern, &mut info.defines);
                // The value expression may read variables and call functions
                self.collect_expr_reads(value, &mut info.reads);
                self.collect_expr_effects(value, &mut info);
            }
            StmtKind::LetUninit { name, .. } => {
                info.defines.insert(name.clone());
            }
            StmtKind::LetElse {
                pattern,
                value,
                else_block,
                ..
            } => {
                self.collect_pattern_bindings(pattern, &mut info.defines);
                self.collect_expr_reads(value, &mut info.reads);
                self.collect_expr_effects(value, &mut info);
                self.collect_block_reads(else_block, &mut info.reads);
                self.collect_block_effects(else_block, &mut info);
                self.collect_block_inner_writes(else_block, &mut info.defines);
            }
            StmtKind::Assign { target, value } => {
                // The target is being written to
                self.collect_assign_target_defines(target, &mut info.defines);
                // But the target may also read (e.g. array[idx] = val reads idx)
                self.collect_assign_target_reads(target, &mut info.reads);
                self.collect_expr_reads(value, &mut info.reads);
                self.collect_expr_effects(value, &mut info);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                self.collect_assign_target_defines(target, &mut info.defines);
                self.collect_assign_target_reads(target, &mut info.reads);
                // Compound assign also reads the target
                self.collect_expr_reads(target, &mut info.reads);
                self.collect_expr_reads(value, &mut info.reads);
                self.collect_expr_effects(value, &mut info);
            }
            StmtKind::Expr(expr) => {
                self.collect_expr_reads(expr, &mut info.reads);
                self.collect_expr_effects(expr, &mut info);
                // Nested Assigns (e.g. inside a `for v in nums.iter() {
                // if v > cap { cap = v; } }`) write to outer-scope
                // names — record them in `info.defines` so subsequent
                // stmts that read those names create a data dependency
                // and serialize against this stmt. Without this, a
                // for-loop body's `cap = v` is invisible to
                // `statements_conflict` and the analyzer groups stmts
                // that should be sequential.
                self.collect_expr_inner_writes(expr, &mut info.defines);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.collect_block_reads(body, &mut info.reads);
                self.collect_block_effects(body, &mut info);
                self.collect_block_inner_writes(body, &mut info.defines);
            }
        }

        info
    }

    /// Check if two statements conflict (have a dependency requiring serialization).
    fn statements_conflict(&self, a: &StmtInfo, b: &StmtInfo) -> bool {
        // If either is in a seq block, force serialization
        if a.is_seq || b.is_seq {
            return true;
        }

        // Data dependency: B reads something A defines, or A reads something B defines
        if !a.defines.is_disjoint(&b.reads) || !b.defines.is_disjoint(&a.reads) {
            return true;
        }

        // Write-write conflict on same variable
        if !a.defines.is_disjoint(&b.defines) {
            return true;
        }

        // Polymorphic calls have unknown effects at analysis time — conflict
        // with any other stmt that has effect activity.
        if a.calls_polymorphic && (b.calls_polymorphic || !b.effects.is_empty()) {
            return true;
        }
        if b.calls_polymorphic && !a.effects.is_empty() {
            return true;
        }

        // Effect conflict
        self.effects_conflict(&a.effects, &b.effects)
    }

    /// Check if two sets of effects have a conflict.
    fn effects_conflict(&self, a_effects: &[StmtEffect], b_effects: &[StmtEffect]) -> bool {
        for a in a_effects {
            for b in b_effects {
                if self.two_effects_conflict(a, b) {
                    return true;
                }
            }
        }
        false
    }

    /// Check if two individual effects conflict.
    ///
    /// Conflict rules:
    /// - Same resource:
    ///   - reads + reads = NO conflict
    ///   - reads + writes = CONFLICT
    ///   - writes + writes = CONFLICT
    ///   - sends + sends = CONFLICT
    ///   - receives + receives = CONFLICT
    ///   - allocates + allocates = CONFLICT (same resource)
    ///   - panics + panics = CONFLICT
    ///   - blocks + blocks = CONFLICT
    ///   - suspends + suspends = CONFLICT
    ///   - Cross-category (e.g. reads + sends) = NO conflict even on same resource
    /// - Different resources = NO conflict regardless of verbs
    fn two_effects_conflict(&self, a: &StmtEffect, b: &StmtEffect) -> bool {
        // Different resources never conflict
        if a.resource != b.resource {
            return false;
        }

        // Same resource: check verb categories
        use EffectVerbKind::*;

        // Group 1: reads/writes — same category
        // Group 2: sends/receives — same category
        // Group 3: allocates — self-conflict
        // Group 4: panics — self-conflict
        // Group 5: blocks — self-conflict
        // Group 6: suspends — self-conflict
        // Cross-group: no conflict

        match (&a.verb, &b.verb) {
            // reads + reads = safe
            (Reads, Reads) => false,
            // reads + writes or writes + reads = CONFLICT
            (Reads, Writes) | (Writes, Reads) => true,
            // writes + writes = CONFLICT
            (Writes, Writes) => true,

            // sends + sends = CONFLICT
            (Sends, Sends) => true,
            // receives + receives = CONFLICT
            (Receives, Receives) => true,
            // sends + receives = safe (same resource, different direction)
            (Sends, Receives) | (Receives, Sends) => false,

            // Self-conflicts for singleton verbs
            (Allocates, Allocates) => true,
            (Panics, Panics) => true,
            (Blocks, Blocks) => true,
            (Suspends, Suspends) => true,

            // User-defined verbs: conflict if same verb on same resource
            (UserDefined(va), UserDefined(vb)) => va == vb,

            // Cross-category: no conflict
            _ => false,
        }
    }

    /// Find groups of statements that can run in parallel.
    /// Uses a greedy approach: walk statements in order, grouping consecutive
    /// independent statements.
    fn find_parallel_groups(
        &self,
        infos: &[StmtInfo],
        has_edge: &[Vec<bool>],
        n: usize,
    ) -> Vec<ParallelGroup> {
        let mut groups: Vec<ParallelGroup> = Vec::new();
        let mut assigned = vec![false; n];

        // For each unassigned statement, try to build a maximal parallel group
        for start in 0..n {
            if assigned[start] {
                continue;
            }

            // A statement that may exit the function early (contains `return`,
            // `break`, or `continue`) cannot share a par group with any
            // sibling — the par branch's `void` LLVM signature can't carry
            // the inner `ret X` and module verification fails.
            if infos[start].has_early_exit {
                assigned[start] = true;
                continue;
            }

            let mut group_indices = vec![start];
            assigned[start] = true;

            // Try to add subsequent unassigned statements to this group.
            //
            // **Contiguous-only invariant.** A parallel group must be a
            // contiguous run of statements: code before the group runs
            // sequentially, the group fans out at one point through
            // `karac_par_run`, then code after the group runs
            // sequentially. Non-contiguous groups violate this — they
            // imply two interleaved fan-outs that the single-fan-out
            // runtime cannot express, and the codegen's
            // `i = max_idx + 1` step would skip past the second
            // group's stmts entirely. So when a candidate isn't
            // independent of the in-progress group, we **break**, not
            // continue — the group ends here and any later eligible
            // candidate becomes the seed of its own group.
            for candidate in (start + 1)..n {
                if assigned[candidate] {
                    break;
                }

                // Same rule applied to candidates: an early-exit stmt
                // ends the par group at its sibling boundary.
                if infos[candidate].has_early_exit {
                    break;
                }

                // Check if candidate is independent of ALL statements already in the group
                let independent = group_indices.iter().all(|&g| !has_edge[candidate][g]);

                if independent {
                    group_indices.push(candidate);
                    assigned[candidate] = true;
                } else {
                    break;
                }
            }

            // Only emit groups with more than 1 statement (parallelism requires >= 2)
            if group_indices.len() > 1 {
                let reason = self.describe_group_reason(infos, &group_indices);
                // A group is trivial if all statements are pure (no effects).
                // Trivial groups aren't worth the overhead of thread dispatch.
                let is_trivial = group_indices
                    .iter()
                    .all(|&i| infos[i].effects.is_empty() && !infos[i].calls_polymorphic);
                groups.push(ParallelGroup {
                    statement_indices: group_indices,
                    reason,
                    is_trivial,
                });
            }
        }

        groups
    }

    /// Generate a human-readable reason for why a group of statements can be parallelized.
    fn describe_group_reason(&self, infos: &[StmtInfo], indices: &[usize]) -> String {
        let all_pure = indices.iter().all(|&i| infos[i].effects.is_empty());
        if all_pure {
            return "pure computations".to_string();
        }

        // Check if they all read different resources
        let mut all_resources: Vec<&str> = Vec::new();
        let mut has_reads_only = true;
        for &i in indices {
            for eff in &infos[i].effects {
                if !matches!(eff.verb, EffectVerbKind::Reads) {
                    has_reads_only = false;
                }
                all_resources.push(&eff.resource);
            }
        }

        if has_reads_only {
            // Check if same or different resources
            let unique: HashSet<&&str> = all_resources.iter().collect();
            if unique.len() > 1 {
                return "independent reads on different resources".to_string();
            }
            return "concurrent reads on same resource".to_string();
        }

        // Check if effects are on different resources
        let unique_resources: HashSet<&str> = all_resources.iter().copied().collect();
        if unique_resources.len() == all_resources.len() && unique_resources.len() > 1 {
            return "independent effects on different resources".to_string();
        }

        "no data or effect dependencies".to_string()
    }

    // ── Variable extraction helpers ────────────────────────────

    fn collect_pattern_bindings(&self, pattern: &Pattern, defines: &mut HashSet<String>) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                defines.insert(name.clone());
            }
            PatternKind::AtBinding { name, pattern } => {
                defines.insert(name.clone());
                self.collect_pattern_bindings(pattern, defines);
            }
            PatternKind::Struct { fields, .. } => {
                for f in fields {
                    if let Some(ref p) = f.pattern {
                        self.collect_pattern_bindings(p, defines);
                    } else {
                        // Shorthand field: `Foo { x }` — the field name is the binding
                        defines.insert(f.name.clone());
                    }
                }
            }
            PatternKind::TupleVariant { patterns, .. } | PatternKind::Tuple(patterns) => {
                for p in patterns {
                    self.collect_pattern_bindings(p, defines);
                }
            }
            PatternKind::Or(patterns) => {
                for p in patterns {
                    self.collect_pattern_bindings(p, defines);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for p in prefix.iter().chain(suffix.iter()) {
                    self.collect_pattern_bindings(p, defines);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    defines.insert(name.clone());
                }
            }
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
        }
    }

    fn collect_assign_target_defines(&self, expr: &Expr, defines: &mut HashSet<String>) {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                defines.insert(name.clone());
            }
            ExprKind::FieldAccess { object, .. } => {
                // a.field = ... defines the root variable
                self.collect_assign_target_defines(object, defines);
            }
            ExprKind::Index { object, .. } => {
                self.collect_assign_target_defines(object, defines);
            }
            _ => {}
        }
    }

    fn collect_assign_target_reads(&self, expr: &Expr, reads: &mut HashSet<String>) {
        match &expr.kind {
            ExprKind::FieldAccess { object, .. } => {
                self.collect_assign_target_reads(object, reads);
            }
            ExprKind::Index { object, index } => {
                self.collect_assign_target_reads(object, reads);
                self.collect_expr_reads(index, reads);
            }
            _ => {}
        }
    }

    // ── Expression read collection ─────────────────────────────

    fn collect_expr_reads(&self, expr: &Expr, reads: &mut HashSet<String>) {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                reads.insert(name.clone());
            }
            ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
                self.collect_expr_reads(left, reads);
                self.collect_expr_reads(right, reads);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.collect_expr_reads(left, reads);
                self.collect_expr_reads(right, reads);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Question(operand) => {
                self.collect_expr_reads(operand, reads);
            }
            ExprKind::Call { callee, args } => {
                self.collect_expr_reads(callee, reads);
                for arg in args {
                    self.collect_expr_reads(&arg.value, reads);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.collect_expr_reads(object, reads);
                for arg in args {
                    self.collect_expr_reads(&arg.value, reads);
                }
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.collect_expr_reads(object, reads);
            }
            ExprKind::Index { object, index } => {
                self.collect_expr_reads(object, reads);
                self.collect_expr_reads(index, reads);
            }
            ExprKind::OptionalChain { object, args, .. } => {
                self.collect_expr_reads(object, reads);
                if let Some(args) = args {
                    for arg in args {
                        self.collect_expr_reads(&arg.value, reads);
                    }
                }
            }
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Try(block)
            | ExprKind::Seq(block)
            | ExprKind::Par(block) => {
                self.collect_block_reads(block, reads);
            }
            ExprKind::Lock { body, .. } => {
                self.collect_block_reads(body, reads);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.collect_expr_reads(condition, reads);
                self.collect_block_reads(then_block, reads);
                if let Some(e) = else_branch {
                    self.collect_expr_reads(e, reads);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.collect_expr_reads(value, reads);
                self.collect_block_reads(then_block, reads);
                if let Some(e) = else_branch {
                    self.collect_expr_reads(e, reads);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.collect_expr_reads(scrutinee, reads);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.collect_expr_reads(guard, reads);
                    }
                    self.collect_expr_reads(&arm.body, reads);
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
                self.collect_expr_reads(condition, reads);
                self.collect_block_reads(body, reads);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.collect_expr_reads(value, reads);
                self.collect_block_reads(body, reads);
            }
            ExprKind::Loop { body, .. } => {
                self.collect_block_reads(body, reads);
            }
            ExprKind::LabeledBlock { body, .. } => {
                self.collect_block_reads(body, reads);
            }
            ExprKind::Closure { body, .. } => {
                self.collect_expr_reads(body, reads);
            }
            ExprKind::Return(Some(inner)) => {
                self.collect_expr_reads(inner, reads);
            }
            ExprKind::Break {
                value: Some(inner), ..
            } => {
                self.collect_expr_reads(inner, reads);
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    self.collect_expr_reads(e, reads);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.collect_expr_reads(value, reads);
                self.collect_expr_reads(count, reads);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.collect_expr_reads(e, reads);
                }
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.collect_expr_reads(k, reads);
                    self.collect_expr_reads(v, reads);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.collect_expr_reads(&f.value, reads);
                }
                if let Some(s) = spread {
                    self.collect_expr_reads(s, reads);
                }
            }
            ExprKind::Cast { expr: inner, .. } => {
                self.collect_expr_reads(inner, reads);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.collect_expr_reads(s, reads);
                }
                if let Some(e) = end {
                    self.collect_expr_reads(e, reads);
                }
            }
            ExprKind::Path { segments, .. } => {
                // A path like Mod::val — the first segment could be a variable
                if let Some(first) = segments.first() {
                    reads.insert(first.clone());
                }
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.collect_expr_reads(&b.value, reads);
                }
                self.collect_block_reads(body, reads);
            }
            ExprKind::InterpolatedStringLit(parts) => {
                for part in parts {
                    if let ParsedInterpolationPart::Expr(inner) = part {
                        self.collect_expr_reads(inner, reads);
                    }
                }
            }
            // Leaf expressions that don't read variables
            ExprKind::Integer(_, _)
            | ExprKind::Float(_, _)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_)
            | ExprKind::Bool(_)
            | ExprKind::SelfValue
            | ExprKind::SelfType
            | ExprKind::Continue { .. }
            | ExprKind::Return(None)
            | ExprKind::Break { value: None, .. }
            | ExprKind::PipePlaceholder
            | ExprKind::Error => {}
        }
    }

    /// Walk an expression's nested blocks and record any outer-scope
    /// names written via `Assign` / `CompoundAssign` into `writes`.
    /// Critical for the auto-parallelizer's data-dependency reasoning:
    /// a `for v in coll { if v > m { m = v; } }` expression-statement
    /// must record `m` as a write so subsequent stmts that read `m`
    /// serialize against it. Local variables shadowed inside nested
    /// blocks (introduced by `let`) are intentionally still recorded
    /// here — the conflict check at the call site uses
    /// `Set::intersect` over a flat name set, so non-disjoint local
    /// shadowing of the same name produces an over-serialization that
    /// is correct (and conservative) rather than incorrect.
    fn collect_expr_inner_writes(&self, expr: &Expr, writes: &mut HashSet<String>) {
        match &expr.kind {
            ExprKind::Block(block) | ExprKind::Seq(block) => {
                self.collect_block_inner_writes(block, writes);
            }
            ExprKind::If {
                then_block,
                else_branch,
                ..
            } => {
                self.collect_block_inner_writes(then_block, writes);
                if let Some(e) = else_branch {
                    self.collect_expr_inner_writes(e, writes);
                }
            }
            ExprKind::IfLet {
                then_block,
                else_branch,
                ..
            } => {
                self.collect_block_inner_writes(then_block, writes);
                if let Some(e) = else_branch {
                    self.collect_expr_inner_writes(e, writes);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.collect_expr_inner_writes(&arm.body, writes);
                }
            }
            ExprKind::While { body, .. } => self.collect_block_inner_writes(body, writes),
            ExprKind::Loop { body, .. } => self.collect_block_inner_writes(body, writes),
            ExprKind::For { body, .. } => self.collect_block_inner_writes(body, writes),
            ExprKind::Unsafe(block) | ExprKind::Par(block) => {
                self.collect_block_inner_writes(block, writes);
            }
            _ => {}
        }
    }

    /// Walk a block's statements and record any outer-scope names
    /// written via `Assign` / `CompoundAssign` (plus inner writes of
    /// nested expressions). Companion to `collect_expr_inner_writes`.
    fn collect_block_inner_writes(&self, block: &Block, writes: &mut HashSet<String>) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Assign { target, .. } | StmtKind::CompoundAssign { target, .. } => {
                    self.collect_assign_target_defines(target, writes);
                }
                StmtKind::Expr(e) => self.collect_expr_inner_writes(e, writes),
                StmtKind::Let { value, .. } => self.collect_expr_inner_writes(value, writes),
                StmtKind::LetElse {
                    value, else_block, ..
                } => {
                    self.collect_expr_inner_writes(value, writes);
                    self.collect_block_inner_writes(else_block, writes);
                }
                StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                    self.collect_block_inner_writes(body, writes);
                }
                _ => {}
            }
        }
        if let Some(e) = &block.final_expr {
            self.collect_expr_inner_writes(e, writes);
        }
    }

    fn collect_block_reads(&self, block: &Block, reads: &mut HashSet<String>) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let { value, .. } => self.collect_expr_reads(value, reads),
                StmtKind::LetUninit { .. } => {}
                StmtKind::LetElse {
                    value, else_block, ..
                } => {
                    self.collect_expr_reads(value, reads);
                    self.collect_block_reads(else_block, reads);
                }
                StmtKind::Assign { target, value } => {
                    self.collect_expr_reads(target, reads);
                    self.collect_expr_reads(value, reads);
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    self.collect_expr_reads(target, reads);
                    self.collect_expr_reads(value, reads);
                }
                StmtKind::Expr(e) => self.collect_expr_reads(e, reads),
                StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                    self.collect_block_reads(body, reads);
                }
            }
        }
        if let Some(e) = &block.final_expr {
            self.collect_expr_reads(e, reads);
        }
    }

    // ── Effect collection from expressions ─────────────────────

    fn collect_expr_effects(&self, expr: &Expr, info: &mut StmtInfo) {
        match &expr.kind {
            ExprKind::Call { callee, args } => {
                // Look up callee effects
                if let Some(name) = self.extract_callee_name(callee) {
                    self.add_function_effects(&name, info);
                }
                self.collect_expr_effects(callee, info);
                for arg in args {
                    self.collect_expr_effects(&arg.value, info);
                }
            }
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                // Try all matching method names (same strategy as effectchecker)
                for key in self.method_bodies.keys() {
                    if key.ends_with(&format!(".{}", method)) {
                        self.add_function_effects(key, info);
                    }
                }
                // Also try bare method name
                self.add_function_effects(method, info);
                self.collect_expr_effects(object, info);
                for arg in args {
                    self.collect_expr_effects(&arg.value, info);
                }
            }
            ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
                self.collect_expr_effects(left, info);
                self.collect_expr_effects(right, info);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.collect_expr_effects(left, info);
                self.collect_expr_effects(right, info);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Question(operand) => {
                self.collect_expr_effects(operand, info);
            }
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Try(block)
            | ExprKind::Seq(block)
            | ExprKind::Par(block) => {
                self.collect_block_effects(block, info);
            }
            ExprKind::Lock { body, .. } => {
                self.collect_block_effects(body, info);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.collect_expr_effects(condition, info);
                self.collect_block_effects(then_block, info);
                if let Some(e) = else_branch {
                    self.collect_expr_effects(e, info);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.collect_expr_effects(value, info);
                self.collect_block_effects(then_block, info);
                if let Some(e) = else_branch {
                    self.collect_expr_effects(e, info);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.collect_expr_effects(scrutinee, info);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.collect_expr_effects(guard, info);
                    }
                    self.collect_expr_effects(&arm.body, info);
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
                self.collect_expr_effects(condition, info);
                self.collect_block_effects(body, info);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.collect_expr_effects(value, info);
                self.collect_block_effects(body, info);
            }
            ExprKind::Loop { body, .. } => {
                self.collect_block_effects(body, info);
            }
            ExprKind::LabeledBlock { body, .. } => {
                self.collect_block_effects(body, info);
            }
            ExprKind::Closure { body, .. } => {
                self.collect_expr_effects(body, info);
            }
            ExprKind::Return(Some(inner)) => {
                self.collect_expr_effects(inner, info);
            }
            ExprKind::Break {
                value: Some(inner), ..
            } => {
                self.collect_expr_effects(inner, info);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.collect_expr_effects(object, info);
            }
            ExprKind::Index { object, index } => {
                self.collect_expr_effects(object, info);
                self.collect_expr_effects(index, info);
            }
            ExprKind::OptionalChain { object, args, .. } => {
                self.collect_expr_effects(object, info);
                if let Some(args) = args {
                    for arg in args {
                        self.collect_expr_effects(&arg.value, info);
                    }
                }
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    self.collect_expr_effects(e, info);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.collect_expr_effects(value, info);
                self.collect_expr_effects(count, info);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.collect_expr_effects(e, info);
                }
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.collect_expr_effects(k, info);
                    self.collect_expr_effects(v, info);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.collect_expr_effects(&f.value, info);
                }
                if let Some(s) = spread {
                    self.collect_expr_effects(s, info);
                }
            }
            ExprKind::Cast { expr: inner, .. } => {
                self.collect_expr_effects(inner, info);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.collect_expr_effects(s, info);
                }
                if let Some(e) = end {
                    self.collect_expr_effects(e, info);
                }
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.collect_expr_effects(&b.value, info);
                }
                self.collect_block_effects(body, info);
            }
            ExprKind::InterpolatedStringLit(parts) => {
                for part in parts {
                    if let ParsedInterpolationPart::Expr(inner) = part {
                        self.collect_expr_effects(inner, info);
                    }
                }
            }
            // Leaf expressions — no effects
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

    fn collect_block_effects(&self, block: &Block, info: &mut StmtInfo) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let { value, .. } => self.collect_expr_effects(value, info),
                StmtKind::LetUninit { .. } => {}
                StmtKind::LetElse {
                    value, else_block, ..
                } => {
                    self.collect_expr_effects(value, info);
                    self.collect_block_effects(else_block, info);
                }
                StmtKind::Assign { target, value } => {
                    self.collect_expr_effects(target, info);
                    self.collect_expr_effects(value, info);
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    self.collect_expr_effects(target, info);
                    self.collect_expr_effects(value, info);
                }
                StmtKind::Expr(e) => self.collect_expr_effects(e, info),
                StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                    self.collect_block_effects(body, info);
                }
            }
        }
        if let Some(e) = &block.final_expr {
            self.collect_expr_effects(e, info);
        }
    }

    /// Look up a function's inferred effects and add them to the effect list.
    /// Also sets `info.calls_polymorphic` if the callee's declared effects
    /// include `with _` — in which case the inferred set alone doesn't describe
    /// what the callee may actually do at runtime.
    fn add_function_effects(&self, name: &str, info: &mut StmtInfo) {
        if let Some(effect_set) = self.effects.inferred_effects.get(name) {
            for te in &effect_set.effects {
                info.effects.push(StmtEffect {
                    verb: te.effect.verb.clone(),
                    resource: te.effect.resource.clone(),
                });
            }
        }
        if matches!(
            self.effects.declared_effects.get(name),
            Some(DeclaredEffects::Polymorphic | DeclaredEffects::PolymorphicWithFixed(_))
        ) {
            info.calls_polymorphic = true;
        }
    }

    /// Extract a callee name from a call expression.
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
}
