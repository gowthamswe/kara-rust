//! Control-flow graph construction for Kāra function bodies.
//!
//! Per design.md § Part 4 RC Dataflow Specification: the RC fallback pass
//! requires a structured CFG with a dominator tree. This module lowers a
//! function body's AST into a basic-block CFG, recording the use-sites
//! (read or consume) of each binding within each block.
//!
//! Kāra has no `goto`; every control-flow construct (`if`, `match`,
//! `while`, `for`, `loop`, `break`, `continue`, `return`, `?`) lowers to
//! a structured, reducible CFG. The graph this module produces is the
//! input to:
//!  - the dominator tree pass (`src/dominator.rs`), and
//!  - the formal RC-condition check `∃C∃U with U ≠ C, ¬dom(C,U) ∧ ¬dom(U,C)`
//!    that replaces the linear forward state machine in `src/ownership.rs`.
//!
//! ## Status
//!
//! Round 12.7 ships the foundational CFG builder with use-site collection,
//! exercised in unit tests. Integration into `ownership.rs` is staged in a
//! subsequent round — the existing linear forward state machine continues
//! to drive RC fallback decisions until the new pass is wired through.

use crate::ast::{Block, Expr, ExprKind, Pattern, PatternKind, Stmt, StmtKind};
use crate::resolver::SpanKey;
use crate::token::Span;
use std::collections::{HashMap, HashSet};

pub type BlockId = usize;

/// A use-site of a binding within a basic block.
#[derive(Debug, Clone)]
pub struct UseSite {
    pub binding: String,
    pub kind: UseKind,
    pub span: Span,
    /// Reason the use site was tagged `Consume`, populated by the
    /// classifier. Read sites carry `ConsumeOrigin::Direct` (it is
    /// never inspected for non-Consume kinds). Drives flavor labeling
    /// on `RcWitness` so the predicate-driven RC fallback path can
    /// emit `RcEntry` records with the correct `RcTrigger`.
    pub consume_origin: ConsumeOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseKind {
    Read,
    Consume,
    /// Marker site for `name = expr;` where the LHS is a bare identifier
    /// (round 12.19). Acts as a *kill* in the predicate's per-binding
    /// dataflow: a prior Consume of the same binding is rebound by this
    /// reassignment, so subsequent uses are not use-after-move. The CFG
    /// records the marker at the assign-target's span; predicate
    /// consumers iterate consumes/uses ignoring this kind, then check
    /// whether a Reassign sits "between" any (C, U) pair before
    /// declaring a witness. Field/index assigns (`obj.f = ...`) do NOT
    /// emit this kind — those are partial mutations of the root, not
    /// rebindings of the binding.
    Reassign,
}

/// Why a Consume use-site was emitted. Defaulted to `Direct`; set to
/// `ClosureCapture` for capture-position consumes inside a closure
/// body, and to `ContainerStore` for owned (no-`mut`-marker) args of a
/// `mut ref self` method call (the trigger-3 sink-arg shape from round
/// 12.12). The classifier tags consumes during the AST walk; the CFG
/// builder threads the tag through `UseSite` so the predicate evaluator
/// can attach it to each `RcWitness` without a second AST pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeOrigin {
    Direct,
    ClosureCapture,
    ContainerStore,
}

/// Bundled output of `crate::use_classifier::classify_function_body`.
///
/// `kinds` tags each binding-use leaf as `Read` or `Consume` (drives the
/// formal RC predicate's consume condition). `sink_arg_spans` lists the
/// argument-expression spans that the CFG must lower into a sibling sink
/// block of the call site rather than inline — round 12.12's structural
/// fix for trigger-3 (container store with subsequent use). The sink
/// pattern mirrors round 12.11's closure-body lowering: capture-position
/// consumes become dominance-incomparable with subsequent outer uses.
#[derive(Debug, Clone, Default)]
pub struct Classification {
    pub kinds: HashMap<SpanKey, UseKind>,
    pub sink_arg_spans: HashSet<SpanKey>,
    /// Per-Consume-span origin tag (round 12.14). Sparse: spans absent
    /// from the map default to `ConsumeOrigin::Direct`. Populated by
    /// the use classifier when a Consume identifier-leaf is recorded
    /// inside a closure body (`ClosureCapture`) or as the
    /// owned-arg of a `mut ref self` method call (`ContainerStore`).
    pub consume_origins: HashMap<SpanKey, ConsumeOrigin>,
}

/// A basic block in the CFG. Statements are not stored — only the use
/// sites in source order, which is all the dataflow pass needs.
#[derive(Debug, Clone, Default)]
pub struct BasicBlock {
    pub id: BlockId,
    pub uses: Vec<UseSite>,
    pub successors: Vec<BlockId>,
}

/// The control-flow graph for one function body.
#[derive(Debug, Clone)]
pub struct Cfg {
    pub blocks: Vec<BasicBlock>,
    pub entry: BlockId,
    pub exit: BlockId,
}

impl Cfg {
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id]
    }

    /// Iterate predecessors of `id` by scanning successor lists. Cheap
    /// enough for the small functions that appear in real code; if a hot
    /// path emerges, cache predecessors in a parallel Vec.
    pub fn predecessors(&self, id: BlockId) -> Vec<BlockId> {
        self.blocks
            .iter()
            .filter(|b| b.successors.contains(&id))
            .map(|b| b.id)
            .collect()
    }

    /// Total number of blocks (including the synthetic exit block).
    pub fn num_blocks(&self) -> usize {
        self.blocks.len()
    }
}

/// Build a CFG from a function body. The function entry is `cfg.entry`;
/// every `return` (and the falls-through-end-of-body case) flows to the
/// synthetic `cfg.exit` block. All `UseSite`s are recorded with
/// `UseKind::Read`; use `build_cfg_with_classification` to drive
/// per-use-position consume tagging from a classifier.
pub fn build_cfg(body: &Block) -> Cfg {
    build_cfg_with_classification(body, &Classification::default())
}

/// Build a CFG with a `Classification` driving (a) the `UseKind` of each
/// recorded `UseSite` and (b) the placement of selected method-call args
/// into sibling sink blocks. Spans absent from `classification.kinds`
/// default to `UseKind::Read`; spans absent from
/// `classification.sink_arg_spans` are lowered inline. The classifier is
/// produced by `crate::use_classifier::classify_function_body`.
pub fn build_cfg_with_classification<'a>(
    body: &'a Block,
    classification: &'a Classification,
) -> Cfg {
    let mut builder = CfgBuilder::new(classification);
    let entry = builder.new_block();
    let exit = builder.new_block();

    let cur = builder.lower_block(body, entry, exit, &[]);
    builder.add_edge(cur, exit);

    Cfg {
        blocks: builder.blocks,
        entry,
        exit,
    }
}

/// State carried while we walk a loop body — tracks where `break` and
/// `continue` should jump within the enclosing loop.
#[derive(Debug, Clone)]
struct LoopFrame {
    label: Option<String>,
    /// Edge target for `break` / `break <label>`.
    break_target: BlockId,
    /// Edge target for `continue` / `continue <label>`.
    continue_target: BlockId,
    /// `defer_stack.len()` snapshot taken right before the loop body's
    /// scope frame was pushed. `break` / `continue` from inside the
    /// loop unwind every defer frame at index `>= defer_depth`,
    /// emitting each frame's `defer` bodies (LIFO) into the exit-edge
    /// path before flowing to the loop's `break_target` /
    /// `continue_target`. Frames at index `< defer_depth` belong to
    /// scopes outside this loop and are not unwound by break /
    /// continue (only by `return` / `?`-error).
    defer_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferKind {
    /// Plain `defer { ... }` — fires on every scope exit (success or
    /// error). Per design.md § Drop ordering item 2 the defer body's
    /// uses must be visible to the predicate at *exit* edges, not at
    /// the lexical declaration site.
    Defer,
    /// `errdefer { ... }` / `errdefer(e) { ... }` — fires only on
    /// non-success exits (per design.md § "errdefer" line 690): a `?`
    /// propagating `Err` / `None`, a `return Err(...)` / `return
    /// None`, or panic. Within the CFG model we conservatively fire
    /// errdefer on every `return` (we do not statically distinguish
    /// `return Ok(...)` from `return Err(...)`) and on every
    /// `?`-error edge; we do *not* fire on fall-through, `break`, or
    /// `continue`. The conservative model produces sound diagnostics
    /// — a defer/errdefer body may only *read* outer captures (per
    /// design.md "may not capture by move"), so spurious cleanup
    /// reads on a wrongly-classified `return Ok(...)` cannot manifest
    /// as a UAM.
    ErrDefer,
}

/// One entry in a scope's cleanup-on-exit list. We store a borrowed
/// reference to the AST body so the cleanup walker re-lowers the body
/// at every exit edge — see "Per-exit-site lowering" below.
#[derive(Clone)]
struct DeferItem<'a> {
    kind: DeferKind,
    body: &'a Block,
}

/// A single scope's defer/errdefer registry, in declaration order.
/// Frames are pushed by `lower_block` on entry and popped on
/// fall-through. Within a frame, `items` is appended-to as we walk
/// statements (so the items visible at any given point are exactly
/// those declared lexically *before* that point); cleanup emission
/// always drains in reverse declaration order (LIFO).
///
/// ## Per-exit-site lowering
///
/// Every exit edge that crosses a frame re-lowers the frame's items
/// into fresh CFG blocks: a `return` and a `?`-error lower the same
/// defer body into two separate cleanup chains, the same as a
/// `break` and a fall-through within the loop body. Sharing one
/// cleanup chain across exit sites is structurally impossible
/// because different exit sites have different downstream successors
/// (function exit, loop merge, parent fall-through). Per-site
/// lowering does duplicate use-site recording, but defer/errdefer
/// bodies may only *read* outer captures (design.md "may not capture
/// by move"), so the duplicates are reads — they cannot pair to fire
/// the formal RC predicate (which requires at least one Consume).
/// Inner-local Consume sites of the defer body itself remain a known
/// imprecision corner.
#[derive(Default)]
struct DeferFrame<'a> {
    items: Vec<DeferItem<'a>>,
}

struct CfgBuilder<'a> {
    blocks: Vec<BasicBlock>,
    classification: &'a Classification,
    /// Stack of defer/errdefer registries, one per active scope.
    /// `defer_stack[0]` is the function body's frame; each nested
    /// `lower_block` call pushes one frame. At any point the frame
    /// at the top of the stack is the innermost active scope.
    defer_stack: Vec<DeferFrame<'a>>,
}

impl<'a> CfgBuilder<'a> {
    fn new(classification: &'a Classification) -> Self {
        CfgBuilder {
            blocks: Vec::new(),
            classification,
            defer_stack: Vec::new(),
        }
    }

    fn classify(&self, span: &Span) -> UseKind {
        self.classification
            .kinds
            .get(&SpanKey::from_span(span))
            .copied()
            .unwrap_or(UseKind::Read)
    }

    fn consume_origin(&self, span: &Span) -> ConsumeOrigin {
        self.classification
            .consume_origins
            .get(&SpanKey::from_span(span))
            .copied()
            .unwrap_or(ConsumeOrigin::Direct)
    }

    fn is_sink_arg(&self, span: &Span) -> bool {
        self.classification
            .sink_arg_spans
            .contains(&SpanKey::from_span(span))
    }

    fn new_block(&mut self) -> BlockId {
        let id = self.blocks.len();
        self.blocks.push(BasicBlock {
            id,
            uses: Vec::new(),
            successors: Vec::new(),
        });
        id
    }

    fn add_edge(&mut self, from: BlockId, to: BlockId) {
        let succs = &mut self.blocks[from].successors;
        if !succs.contains(&to) {
            succs.push(to);
        }
    }

    fn record_use(&mut self, block: BlockId, use_site: UseSite) {
        self.blocks[block].uses.push(use_site);
    }

    /// Walk a statement-block, returning the block id where execution
    /// continues after the block. `cur` is the entry block for the block;
    /// `exit` is the function's exit block (for `return` lowering).
    ///
    /// The block pushes a fresh `DeferFrame` on entry; statements
    /// that are `defer` / `errdefer` register on that frame without
    /// walking the body inline (the body is re-walked at each exit
    /// edge that crosses this scope). On fall-through, the frame's
    /// `defer` bodies (NOT errdefers — fall-through is a success
    /// path) are emitted in LIFO order into `cur`, and the frame is
    /// popped.
    fn lower_block(
        &mut self,
        block: &'a Block,
        cur: BlockId,
        exit: BlockId,
        loops: &[LoopFrame],
    ) -> BlockId {
        self.defer_stack.push(DeferFrame::default());
        let mut cur = cur;
        for stmt in &block.stmts {
            cur = self.lower_stmt(stmt, cur, exit, loops);
        }
        if let Some(final_expr) = &block.final_expr {
            cur = self.lower_expr(final_expr, cur, exit, loops);
        }
        // Fall-through is a success exit — only `defer` items fire
        // (errdefer is skipped on success per design.md). Emit the
        // current scope's frame LIFO before popping.
        cur = self.emit_scope_cleanup(self.defer_stack.len() - 1, cur, exit, loops, false);
        self.defer_stack.pop();
        cur
    }

    /// Emit cleanup for one scope's `DeferFrame` (the frame at index
    /// `frame_idx` in `defer_stack`) by re-lowering its registered
    /// bodies in reverse declaration order. Returns the block id
    /// where cleanup-tail control continues.
    ///
    /// `error_path` selects whether `errdefer` items participate:
    /// `true` for error exits (`return`, `?`-error), `false` for
    /// success exits (fall-through, `break`, `continue`). On error
    /// paths every `errdefer` item fires LIFO *first*, then every
    /// `defer` item fires LIFO (per design.md § Unified drop+defer
    /// stack — `errdefer` group precedes the `defer` group). On
    /// success paths only `defer` items fire.
    ///
    /// Each body is lowered through the normal `lower_block`
    /// pathway, so it gets its own pushed frame and its own internal
    /// control-flow handling. Body uses are recorded into fresh CFG
    /// blocks downstream of `cur`.
    fn emit_scope_cleanup(
        &mut self,
        frame_idx: usize,
        cur: BlockId,
        exit: BlockId,
        loops: &[LoopFrame],
        error_path: bool,
    ) -> BlockId {
        // Clone the item list so we can drop the borrow on
        // `self.defer_stack` before recursing into `lower_block` (which
        // mutates the stack).
        let frame = self.defer_stack[frame_idx].items.clone();
        let mut cur = cur;
        // Each body lowers into a fresh block so its uses are not
        // recorded into the caller's `cur` (which would conflate the
        // pre-exit "real" code with the cleanup chain). The fresh
        // block keeps the cleanup chain a clean sub-CFG of its own.
        if error_path {
            for item in frame.iter().rev() {
                if item.kind == DeferKind::ErrDefer {
                    let body_entry = self.new_block();
                    self.add_edge(cur, body_entry);
                    cur = self.lower_block(item.body, body_entry, exit, loops);
                }
            }
        }
        for item in frame.iter().rev() {
            if item.kind == DeferKind::Defer {
                let body_entry = self.new_block();
                self.add_edge(cur, body_entry);
                cur = self.lower_block(item.body, body_entry, exit, loops);
            }
        }
        cur
    }

    /// Emit cleanup for every scope from `defer_stack.len() - 1`
    /// (innermost) down to (and including) `target_depth`, in that
    /// order. `error_path` selects whether `errdefer` items
    /// participate at each frame. Returns the cleanup-tail block id.
    ///
    /// `target_depth` is the *index* of the outermost frame to clean
    /// up. Use `0` for full unwinds (return, `?`-error). Use a loop
    /// frame's saved `defer_depth` for `break` / `continue` (clean
    /// frames inside the loop body but not the frames outside it).
    fn emit_unwind_chain(
        &mut self,
        target_depth: usize,
        cur: BlockId,
        exit: BlockId,
        loops: &[LoopFrame],
        error_path: bool,
    ) -> BlockId {
        let mut cur = cur;
        let top = self.defer_stack.len();
        for frame_idx in (target_depth..top).rev() {
            cur = self.emit_scope_cleanup(frame_idx, cur, exit, loops, error_path);
        }
        cur
    }

    fn lower_stmt(
        &mut self,
        stmt: &'a Stmt,
        cur: BlockId,
        exit: BlockId,
        loops: &[LoopFrame],
    ) -> BlockId {
        match &stmt.kind {
            StmtKind::Let {
                pattern: _, value, ..
            } => {
                // Lower the RHS — it may consume / read bindings — then
                // the let binding is just a definition (no use of itself).
                self.lower_expr(value, cur, exit, loops)
            }
            StmtKind::LetUninit { .. } => cur,
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                // `let pat = expr else { diverge }` — the else branch
                // diverges (returns / break) and never falls through.
                let after = self.lower_expr(value, cur, exit, loops);
                let else_entry = self.new_block();
                self.add_edge(after, else_entry);
                // The else block diverges — we still walk it for
                // use-collection but don't merge it back.
                self.lower_block(else_block, else_entry, exit, loops);
                after
            }
            StmtKind::Defer { body } => {
                // Register on the innermost scope's defer frame; do
                // NOT walk the body inline. The body is re-lowered at
                // each exit edge that crosses this scope by
                // `emit_scope_cleanup`. Per design.md § Drop
                // ordering, the body's uses must appear at scope-exit
                // edges, not at the lexical declaration site.
                self.defer_stack
                    .last_mut()
                    .expect("defer outside any scope frame")
                    .items
                    .push(DeferItem {
                        kind: DeferKind::Defer,
                        body,
                    });
                cur
            }
            StmtKind::ErrDefer { body, .. } => {
                self.defer_stack
                    .last_mut()
                    .expect("errdefer outside any scope frame")
                    .items
                    .push(DeferItem {
                        kind: DeferKind::ErrDefer,
                        body,
                    });
                cur
            }
            StmtKind::Assign { target, value } => {
                let after_rhs = self.lower_expr(value, cur, exit, loops);
                self.lower_expr(target, after_rhs, exit, loops)
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                let after_rhs = self.lower_expr(value, cur, exit, loops);
                self.lower_expr(target, after_rhs, exit, loops)
            }
            StmtKind::Expr(e) => self.lower_expr(e, cur, exit, loops),
        }
    }

    /// Lower an expression for use-collection. Returns the block id where
    /// execution continues. Leaves and most operators record uses in the
    /// current block; control-flow expressions (`if`, `match`, `while`,
    /// `loop`, `for`, `break`, `continue`, `return`, `?`) split the CFG.
    fn lower_expr(
        &mut self,
        expr: &'a Expr,
        cur: BlockId,
        exit: BlockId,
        loops: &[LoopFrame],
    ) -> BlockId {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                let kind = self.classify(&expr.span);
                let consume_origin = self.consume_origin(&expr.span);
                self.record_use(
                    cur,
                    UseSite {
                        binding: name.clone(),
                        kind,
                        span: expr.span.clone(),
                        consume_origin,
                    },
                );
                cur
            }
            ExprKind::SelfValue => {
                let kind = self.classify(&expr.span);
                let consume_origin = self.consume_origin(&expr.span);
                self.record_use(
                    cur,
                    UseSite {
                        binding: "self".to_string(),
                        kind,
                        span: expr.span.clone(),
                        consume_origin,
                    },
                );
                cur
            }
            // Literals and pure-constant forms.
            ExprKind::Integer(..)
            | ExprKind::Float(..)
            | ExprKind::Bool(..)
            | ExprKind::CharLit(..)
            | ExprKind::StringLit(..)
            | ExprKind::MultiStringLit(..)
            | ExprKind::InterpolatedStringLit(..)
            | ExprKind::Path(..)
            | ExprKind::SelfType => cur,

            ExprKind::Binary { left, right, .. }
            | ExprKind::Pipe { left, right }
            | ExprKind::NilCoalesce { left, right } => {
                let c = self.lower_expr(left, cur, exit, loops);
                self.lower_expr(right, c, exit, loops)
            }
            ExprKind::Unary { operand, .. } => self.lower_expr(operand, cur, exit, loops),

            ExprKind::Call { callee, args } => {
                let mut c = self.lower_expr(callee, cur, exit, loops);
                for arg in args {
                    c = self.lower_expr(&arg.value, c, exit, loops);
                }
                c
            }
            ExprKind::MethodCall { object, args, .. } => {
                let mut c = self.lower_expr(object, cur, exit, loops);
                // Lower non-sink args in source order into the pre-fork
                // block; collect sink args for a deferred sibling-sink
                // lowering. Source-order interleaving is preserved per
                // arg-bucket — the predicate is per-binding so the
                // relative position of disjoint reads/consumes between
                // buckets has no effect on RC detection.
                let mut sink_arg_exprs: Vec<&Expr> = Vec::new();
                for arg in args {
                    if self.is_sink_arg(&arg.value.span) {
                        sink_arg_exprs.push(&arg.value);
                    } else {
                        c = self.lower_expr(&arg.value, c, exit, loops);
                    }
                }
                if sink_arg_exprs.is_empty() {
                    c
                } else {
                    // Container-store call (round 12.12): the receiver is
                    // a `mut ref self` method, so each owned arg flows
                    // into a container that outlives the call. To make
                    // the consume dominance-incomparable with subsequent
                    // outer uses of the same binding, lower sink args
                    // into a sibling sink block that does not reach the
                    // outer continuation — mirrors round 12.11's
                    // closure-body lowering for trigger-2.
                    let sink_block = self.new_block();
                    let after_call = self.new_block();
                    self.add_edge(c, sink_block);
                    self.add_edge(c, after_call);
                    for sink_arg in sink_arg_exprs {
                        self.lower_expr(sink_arg, sink_block, exit, loops);
                    }
                    after_call
                }
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.lower_expr(object, cur, exit, loops)
            }
            ExprKind::Index { object, index } => {
                let c = self.lower_expr(object, cur, exit, loops);
                self.lower_expr(index, c, exit, loops)
            }

            ExprKind::Block(block) => self.lower_block(block, cur, exit, loops),

            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                let after_cond = self.lower_expr(condition, cur, exit, loops);
                let then_entry = self.new_block();
                self.add_edge(after_cond, then_entry);
                let then_exit = self.lower_block(then_block, then_entry, exit, loops);

                let merge = self.new_block();
                self.add_edge(then_exit, merge);

                if let Some(eb) = else_branch {
                    let else_entry = self.new_block();
                    self.add_edge(after_cond, else_entry);
                    let else_exit = self.lower_expr(eb, else_entry, exit, loops);
                    self.add_edge(else_exit, merge);
                } else {
                    self.add_edge(after_cond, merge);
                }
                merge
            }
            ExprKind::IfLet {
                pattern: _,
                value,
                then_block,
                else_branch,
            } => {
                let after_scrutinee = self.lower_expr(value, cur, exit, loops);
                let then_entry = self.new_block();
                self.add_edge(after_scrutinee, then_entry);
                let then_exit = self.lower_block(then_block, then_entry, exit, loops);

                let merge = self.new_block();
                self.add_edge(then_exit, merge);

                if let Some(eb) = else_branch {
                    let else_entry = self.new_block();
                    self.add_edge(after_scrutinee, else_entry);
                    let else_exit = self.lower_expr(eb, else_entry, exit, loops);
                    self.add_edge(else_exit, merge);
                } else {
                    self.add_edge(after_scrutinee, merge);
                }
                merge
            }
            ExprKind::Match { scrutinee, arms } => {
                let after_scrut = self.lower_expr(scrutinee, cur, exit, loops);
                let merge = self.new_block();
                if arms.is_empty() {
                    self.add_edge(after_scrut, merge);
                } else {
                    for arm in arms {
                        let arm_entry = self.new_block();
                        self.add_edge(after_scrut, arm_entry);
                        let mut arm_cur = arm_entry;
                        if let Some(guard) = &arm.guard {
                            arm_cur = self.lower_expr(guard, arm_cur, exit, loops);
                        }
                        let arm_exit = self.lower_expr(&arm.body, arm_cur, exit, loops);
                        self.add_edge(arm_exit, merge);
                    }
                }
                merge
            }

            ExprKind::While {
                label,
                condition,
                body,
            } => {
                let header = self.new_block();
                self.add_edge(cur, header);
                let after_cond = self.lower_expr(condition, header, exit, loops);
                let body_entry = self.new_block();
                self.add_edge(after_cond, body_entry);
                let merge = self.new_block();
                self.add_edge(after_cond, merge);

                let frame = LoopFrame {
                    label: label.clone(),
                    break_target: merge,
                    continue_target: header,
                    defer_depth: self.defer_stack.len(),
                };
                let mut new_loops = loops.to_vec();
                new_loops.push(frame);
                let body_exit = self.lower_block(body, body_entry, exit, &new_loops);
                self.add_edge(body_exit, header);
                merge
            }
            ExprKind::WhileLet {
                label, value, body, ..
            } => {
                let header = self.new_block();
                self.add_edge(cur, header);
                let after_scrut = self.lower_expr(value, header, exit, loops);
                let body_entry = self.new_block();
                self.add_edge(after_scrut, body_entry);
                let merge = self.new_block();
                self.add_edge(after_scrut, merge);

                let frame = LoopFrame {
                    label: label.clone(),
                    break_target: merge,
                    continue_target: header,
                    defer_depth: self.defer_stack.len(),
                };
                let mut new_loops = loops.to_vec();
                new_loops.push(frame);
                let body_exit = self.lower_block(body, body_entry, exit, &new_loops);
                self.add_edge(body_exit, header);
                merge
            }
            ExprKind::For {
                label,
                pattern: _,
                iterable,
                body,
            } => {
                // Iterable is evaluated once before the loop.
                let after_iter = self.lower_expr(iterable, cur, exit, loops);
                let header = self.new_block();
                self.add_edge(after_iter, header);
                let body_entry = self.new_block();
                self.add_edge(header, body_entry);
                let merge = self.new_block();
                self.add_edge(header, merge);

                let frame = LoopFrame {
                    label: label.clone(),
                    break_target: merge,
                    continue_target: header,
                    defer_depth: self.defer_stack.len(),
                };
                let mut new_loops = loops.to_vec();
                new_loops.push(frame);
                let body_exit = self.lower_block(body, body_entry, exit, &new_loops);
                self.add_edge(body_exit, header);
                merge
            }
            ExprKind::Loop { label, body } => {
                let header = self.new_block();
                self.add_edge(cur, header);
                let merge = self.new_block();
                let frame = LoopFrame {
                    label: label.clone(),
                    break_target: merge,
                    continue_target: header,
                    defer_depth: self.defer_stack.len(),
                };
                let mut new_loops = loops.to_vec();
                new_loops.push(frame);
                let body_exit = self.lower_block(body, header, exit, &new_loops);
                self.add_edge(body_exit, header);
                merge
            }

            ExprKind::Break { label, value } => {
                let mut c = cur;
                if let Some(v) = value {
                    c = self.lower_expr(v, c, exit, loops);
                }
                if let Some(frame) = resolve_loop_frame(loops, label.as_deref()) {
                    // Unwind every defer frame inside the loop body
                    // (LIFO across frames, LIFO within each frame),
                    // then route to the loop's break_target. Break
                    // is a success exit so errdefers do not fire.
                    c = self.emit_unwind_chain(
                        frame.defer_depth,
                        c,
                        exit,
                        loops,
                        /*error_path=*/ false,
                    );
                    self.add_edge(c, frame.break_target);
                }
                // Successor in the linear walk is a fresh sink — anything
                // after `break` in the same source-level block is
                // unreachable, but we still need a node to attach further
                // statements to (the parser may emit them; they get an
                // unreachable sub-CFG that just isn't part of any path
                // from entry).
                self.new_block()
            }
            ExprKind::Continue { label } => {
                if let Some(frame) = resolve_loop_frame(loops, label.as_deref()) {
                    let c = self.emit_unwind_chain(
                        frame.defer_depth,
                        cur,
                        exit,
                        loops,
                        /*error_path=*/ false,
                    );
                    self.add_edge(c, frame.continue_target);
                }
                self.new_block()
            }
            ExprKind::Return(value) => {
                let mut c = cur;
                if let Some(v) = value {
                    c = self.lower_expr(v, c, exit, loops);
                }
                // Full unwind of every active defer frame, in
                // reverse declaration order (innermost first), with
                // errdefer items firing because `return` is
                // conservatively treated as a non-success exit (the
                // CFG model cannot statically distinguish `return
                // Ok(...)` from `return Err(...)`; the conservative
                // classification produces sound diagnostics — defer
                // bodies may only read outer captures, so spurious
                // cleanup reads on a wrongly-classified `return
                // Ok(...)` cannot manifest as a UAM).
                c = self.emit_unwind_chain(0, c, exit, loops, /*error_path=*/ true);
                self.add_edge(c, exit);
                self.new_block()
            }
            ExprKind::Question(inner) => {
                // `?` is a conditional return: if the inner is `Err`/`None`,
                // control transfers to the function exit; otherwise it
                // continues sequentially with the unwrapped value. The
                // error edge is a non-success exit — every active
                // defer frame unwinds with errdefer items firing
                // first (LIFO), then defer items (LIFO).
                let after = self.lower_expr(inner, cur, exit, loops);
                let err_tail =
                    self.emit_unwind_chain(0, after, exit, loops, /*error_path=*/ true);
                self.add_edge(err_tail, exit);
                let cont = self.new_block();
                self.add_edge(after, cont);
                cont
            }
            ExprKind::OptionalChain { object, args, .. } => {
                let mut c = self.lower_expr(object, cur, exit, loops);
                if let Some(arg_list) = args {
                    for arg in arg_list {
                        c = self.lower_expr(&arg.value, c, exit, loops);
                    }
                }
                c
            }

            ExprKind::Closure { body, .. } => {
                // Capture happens at the creation site; the body
                // executes at an unknown future time (zero, one, or
                // many invocations). For the formal RC predicate to
                // see capture-position consumes as RC trigger-2
                // candidates, the closure body and the outer
                // continuation must be *sibling* blocks of the
                // creation point — neither dominating the other.
                //
                // We therefore fork `cur` into two successors:
                //   - `closure_body_block` — the body is lowered
                //     here; its uses are observed but it is a sink
                //     (it does not flow back into the outer CFG).
                //   - `after_creation` — the outer continuation; the
                //     enclosing block continues straight through this.
                //
                // A capture-position consume inside the body and a
                // subsequent outer use both descend from `cur`, so
                // both are dominated by `cur` but neither dominates
                // the other — the predicate fires (round 12.11).
                let closure_body_block = self.new_block();
                let after_creation = self.new_block();
                self.add_edge(cur, closure_body_block);
                self.add_edge(cur, after_creation);
                self.lower_expr(body, closure_body_block, exit, loops);
                after_creation
            }

            ExprKind::Cast { expr: inner, .. } => self.lower_expr(inner, cur, exit, loops),
            ExprKind::Range { start, end, .. } => {
                let mut c = cur;
                if let Some(s) = start {
                    c = self.lower_expr(s, c, exit, loops);
                }
                if let Some(e) = end {
                    c = self.lower_expr(e, c, exit, loops);
                }
                c
            }

            ExprKind::Tuple(es) | ExprKind::ArrayLiteral(es) => {
                let mut c = cur;
                for e in es {
                    c = self.lower_expr(e, c, exit, loops);
                }
                c
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                let mut c = cur;
                for e in items {
                    c = self.lower_expr(e, c, exit, loops);
                }
                c
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                let c = self.lower_expr(value, cur, exit, loops);
                self.lower_expr(count, c, exit, loops)
            }
            ExprKind::MapLiteral(entries) => {
                let mut c = cur;
                for (k, v) in entries {
                    c = self.lower_expr(k, c, exit, loops);
                    c = self.lower_expr(v, c, exit, loops);
                }
                c
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                let mut c = cur;
                for f in fields {
                    c = self.lower_expr(&f.value, c, exit, loops);
                }
                if let Some(s) = spread {
                    c = self.lower_expr(s, c, exit, loops);
                }
                c
            }

            // Block-bodied "transparent" forms — `par`, `seq`, `unsafe`,
            // `lock`, `providers`. Each evaluates its body in the current
            // sequential thread of control from the dataflow perspective:
            // every statement inside is reachable from the form's entry.
            // For `par` the body's *internal* parallelism affects Phase 2
            // (Rc → Arc promotion) but not the Phase-1 trigger predicate,
            // which only cares whether use sites are visible to the CFG.
            // Without this arm these forms would fall through `_ => cur`
            // and silently drop their bodies' use sites — see parity test
            // `parity_t1_in_par_block_predicate_matches_legacy` for the
            // shape that exposed this gap.
            ExprKind::Par(body)
            | ExprKind::Seq(body)
            | ExprKind::Unsafe(body)
            | ExprKind::Try(body) => self.lower_block(body, cur, exit, loops),
            ExprKind::Lock { body, .. } => self.lower_block(body, cur, exit, loops),
            ExprKind::Providers { bindings, body } => {
                let mut c = cur;
                for binding in bindings {
                    c = self.lower_expr(&binding.value, c, exit, loops);
                }
                self.lower_block(body, c, exit, loops)
            }

            // Catch-all for forms that are pure leaves or decay to no-ops
            // for use-collection (e.g. type annotations, error nodes).
            // Walking them is harmless: they emit no use sites.
            _ => cur,
        }
    }
}

/// Resolve a `break` / `continue` target's loop frame. With a label,
/// walk the loop stack until we find a matching frame; without a
/// label, target the innermost loop. Returns None if the program is
/// malformed (the resolver should have caught this earlier).
fn resolve_loop_frame<'f>(loops: &'f [LoopFrame], label: Option<&str>) -> Option<&'f LoopFrame> {
    match label {
        Some(l) => loops.iter().rev().find(|f| f.label.as_deref() == Some(l)),
        None => loops.last(),
    }
}

/// Helper: extract the binding names introduced by a pattern. Used by
/// the integrated dataflow pass; exposed here because the CFG builder
/// needs to know what bindings come into scope at a `let pat = ...`.
#[allow(dead_code)]
pub(crate) fn pattern_bindings(pattern: &Pattern) -> Vec<String> {
    fn walk(p: &Pattern, out: &mut Vec<String>) {
        match &p.kind {
            PatternKind::Binding(name) => out.push(name.clone()),
            PatternKind::Tuple(ps) => {
                for sp in ps {
                    walk(sp, out);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for f in fields {
                    if let Some(p) = &f.pattern {
                        walk(p, out);
                    } else {
                        out.push(f.name.clone());
                    }
                }
            }
            PatternKind::TupleVariant { patterns, .. } => {
                for sp in patterns {
                    walk(sp, out);
                }
            }
            PatternKind::AtBinding { name, pattern } => {
                out.push(name.clone());
                walk(pattern, out);
            }
            PatternKind::Or(alts) => {
                if let Some(first) = alts.first() {
                    walk(first, out);
                }
            }
            _ => {}
        }
    }
    let mut v = Vec::new();
    walk(pattern, &mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, resolve};

    fn cfg_of(src: &str) -> Cfg {
        let parsed = parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = resolve(&parsed.program);
        assert!(
            resolved.errors.is_empty(),
            "resolve errors: {:?}",
            resolved.errors
        );
        let main = parsed
            .program
            .items
            .iter()
            .find_map(|i| {
                if let crate::ast::Item::Function(f) = i {
                    if f.name == "main" {
                        return Some(f);
                    }
                }
                None
            })
            .expect("no fn main in source");
        build_cfg(&main.body)
    }

    #[test]
    fn linear_program_is_one_real_block_plus_exit() {
        // No control-flow constructs — entry block flows directly to the
        // synthetic exit. (build_cfg always adds an implicit
        // fall-through edge to exit.)
        let cfg = cfg_of("fn main() { let x = 1; let y = x + 2; }");
        assert!(cfg.num_blocks() >= 2);
        assert_eq!(cfg.entry, 0);
        // Entry should reach exit.
        assert!(cfg.block(cfg.entry).successors.contains(&cfg.exit));
    }

    #[test]
    fn linear_program_records_reads_in_order() {
        let cfg = cfg_of("fn main() { let x = 1; let y = x + x; let _z = y; }");
        let entry = cfg.block(cfg.entry);
        let names: Vec<&str> = entry.uses.iter().map(|u| u.binding.as_str()).collect();
        // We expect at least the two reads of `x` followed by one of `y`,
        // in source order. Other helper symbols may appear if any.
        let user_names: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| matches!(*n, "x" | "y"))
            .collect();
        assert_eq!(user_names, vec!["x", "x", "y"]);
    }

    #[test]
    fn if_else_creates_three_branches_meeting_at_merge() {
        // cond → then-entry, cond → else-entry, both reach merge.
        let cfg = cfg_of(
            "fn main() {\n\
                 let c = true;\n\
                 if c { let _a = 1; } else { let _b = 2; }\n\
             }",
        );
        // There should be at least: entry, then-entry, else-entry, merge,
        // exit — five distinct blocks.
        assert!(cfg.num_blocks() >= 5);

        // The entry block reads `c` and then forks.
        let entry_uses: Vec<&str> = cfg
            .block(cfg.entry)
            .uses
            .iter()
            .map(|u| u.binding.as_str())
            .collect();
        assert!(entry_uses.contains(&"c"), "entry reads c: {entry_uses:?}");
        assert_eq!(
            cfg.block(cfg.entry).successors.len(),
            2,
            "if-cond block must have two successors (then + else)"
        );

        // No use-after-move regression: every successor of entry should
        // eventually reach the exit block.
        assert!(reaches(&cfg, cfg.entry, cfg.exit));
    }

    #[test]
    fn while_loop_has_back_edge_to_header() {
        let cfg = cfg_of(
            "fn main() {\n\
                 let mut i = 0;\n\
                 while i < 3 { i = i + 1; }\n\
             }",
        );
        // The header has at least two successors — body-entry and merge.
        let header = cfg
            .blocks
            .iter()
            .find(|b| b.successors.len() >= 2 && b.id != cfg.entry)
            .expect("expected a header block with two successors");
        // Some block must have the header as a successor (the back edge).
        let has_back = cfg.blocks.iter().any(|b| b.successors.contains(&header.id));
        assert!(has_back, "expected a back edge into the loop header");
    }

    #[test]
    fn return_terminates_with_edge_to_exit() {
        let cfg = cfg_of(
            "fn main() {\n\
                 let x = 5;\n\
                 if x > 0 { return; }\n\
                 let _y = x;\n\
             }",
        );
        // Some block must point to exit — both the explicit `return` and
        // the function-end fall-through.
        let preds_of_exit: Vec<BlockId> = cfg
            .blocks
            .iter()
            .filter(|b| b.successors.contains(&cfg.exit))
            .map(|b| b.id)
            .collect();
        assert!(
            preds_of_exit.len() >= 2,
            "expected ≥2 preds of exit (return + fall-through), got {}",
            preds_of_exit.len()
        );
    }

    #[test]
    fn break_routes_to_loop_merge() {
        let cfg = cfg_of(
            "fn main() {\n\
                 loop { break; }\n\
             }",
        );
        // After lowering, the loop header has the body block, which
        // contains the break — it routes to the merge block (which then
        // falls through to exit).
        assert!(reaches(&cfg, cfg.entry, cfg.exit));
    }

    #[test]
    fn continue_routes_to_header() {
        let cfg = cfg_of(
            "fn main() {\n\
                 let mut i = 0;\n\
                 while i < 3 {\n\
                     i = i + 1;\n\
                     continue;\n\
                 }\n\
             }",
        );
        assert!(reaches(&cfg, cfg.entry, cfg.exit));
    }

    #[test]
    fn match_lowers_to_one_branch_per_arm() {
        let cfg = cfg_of(
            "fn main() {\n\
                 let n = 1;\n\
                 match n { 1 => { let _ = 1; }, 2 => { let _ = 2; }, _ => { let _ = 0; } }\n\
             }",
        );
        // The scrutinee block should have one successor per arm.
        let scrut_block = cfg
            .blocks
            .iter()
            .find(|b| b.successors.len() == 3)
            .expect("expected a 3-way match dispatch block");
        assert_eq!(scrut_block.successors.len(), 3);
        assert!(reaches(&cfg, cfg.entry, cfg.exit));
    }

    #[test]
    fn method_call_with_sink_arg_forks_into_sibling_sink() {
        // Round 12.12: when a method call's arg span is in
        // `sink_arg_spans`, the CFG lowers it into a sibling sink block
        // of the call site so its consume is dominance-incomparable
        // with subsequent outer uses. This test synthesizes a
        // Classification by hand to exercise the fork shape without
        // depending on the full classifier pipeline.
        let parsed = crate::parse(
            "struct Widget { value: i64 }\n\
             struct Bag { count: i64 }\n\
             impl Bag {\n\
                 fn insert(mut ref self, key: i64, value: Widget) { }\n\
             }\n\
             fn audit(w: Widget) { }\n\
             fn main() {\n\
                 let w = Widget { value: 42 };\n\
                 let mut bag = Bag { count: 0 };\n\
                 bag.insert(0, w);\n\
                 audit(w);\n\
             }",
        );
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );

        // Pull the `MethodCall` arg span for `w` out of the AST.
        let main = parsed
            .program
            .items
            .iter()
            .find_map(|i| {
                if let crate::ast::Item::Function(f) = i {
                    if f.name == "main" {
                        return Some(f);
                    }
                }
                None
            })
            .unwrap();

        // Drill: stmts[2] (zero-indexed) is `bag.insert(0, w);`.
        let mc_stmt = &main.body.stmts[2];
        let StmtKind::Expr(mc_expr) = &mc_stmt.kind else {
            panic!("expected expression statement");
        };
        let ExprKind::MethodCall { args, .. } = &mc_expr.kind else {
            panic!("expected method call");
        };
        // Synthesize a classification that marks the `w` arg (args[1])
        // as a sink-arg and tags its identifier-leaf use as Consume.
        let w_arg_span = args[1].value.span.clone();
        let mut classification = Classification::default();
        classification
            .sink_arg_spans
            .insert(SpanKey::from_span(&w_arg_span));
        classification
            .kinds
            .insert(SpanKey::from_span(&w_arg_span), UseKind::Consume);

        let cfg = build_cfg_with_classification(&main.body, &classification);

        // Find the block that records the Consume of `w` — that's the
        // sink block. Its successor list must NOT include any block
        // that records the subsequent outer Read of `w` from `audit(w)`.
        let consume_block = cfg
            .blocks
            .iter()
            .find(|b| {
                b.uses
                    .iter()
                    .any(|u| u.binding == "w" && u.kind == UseKind::Consume)
            })
            .expect("expected a block with Consume of w");
        let read_blocks: Vec<BlockId> = cfg
            .blocks
            .iter()
            .filter(|b| {
                b.uses
                    .iter()
                    .any(|u| u.binding == "w" && u.kind == UseKind::Read)
            })
            .map(|b| b.id)
            .collect();
        for rb in &read_blocks {
            assert!(
                !consume_block.successors.contains(rb),
                "sink block {} should not flow directly to read block {}",
                consume_block.id,
                rb
            );
        }

        // The dominator predicate should fire — neither block dominates
        // the other.
        let dom = crate::dominator::compute_dominators(&cfg);
        for rb in &read_blocks {
            assert!(
                !dom.dominates(consume_block.id, *rb),
                "consume block must not dominate read block (sibling sink)"
            );
            assert!(
                !dom.dominates(*rb, consume_block.id),
                "read block must not dominate consume block (sibling sink)"
            );
        }
    }

    fn cfg_records_d_inside(form_src: &str) -> Cfg {
        // Build a small program where `d` is used inside the `form_src`
        // block-bodied form. Round 12.13's CFG arm should walk into the
        // body and record the use.
        let src = format!(
            "struct Data {{ value: i64 }}\n\
             fn use_d(d: Data) {{ }}\n\
             fn main() {{\n\
                 let d = Data {{ value: 0 }};\n\
                 {form_src}\n\
             }}"
        );
        cfg_of(&src)
    }

    fn assert_use_of_d_recorded(cfg: &Cfg, form_label: &str) {
        let total: usize = cfg
            .blocks
            .iter()
            .map(|b| b.uses.iter().filter(|u| u.binding == "d").count())
            .sum();
        assert!(
            total >= 1,
            "expected at least one CFG use of `d` inside `{form_label}` body — got {total}"
        );
    }

    #[test]
    fn par_block_body_uses_are_visible_in_cfg() {
        // Round 12.13: `par { ... }` no longer falls through `_ => cur`
        // — its body is walked and use sites become CFG `UseSite`s.
        // Without this round's arm, `total` would be 0 and the parity
        // test `parity_t1_in_par_block_predicate_matches_legacy` would
        // not see the second use of `d`.
        let cfg = cfg_records_d_inside("par { use_d(d); }");
        assert_use_of_d_recorded(&cfg, "par");
    }

    #[test]
    fn seq_block_body_uses_are_visible_in_cfg() {
        let cfg = cfg_records_d_inside("seq { use_d(d); }");
        assert_use_of_d_recorded(&cfg, "seq");
    }

    #[test]
    fn unsafe_block_body_uses_are_visible_in_cfg() {
        let cfg = cfg_records_d_inside("unsafe { use_d(d); }");
        assert_use_of_d_recorded(&cfg, "unsafe");
    }

    #[test]
    fn predecessors_match_successors() {
        let cfg = cfg_of(
            "fn main() {\n\
                 let c = true;\n\
                 if c { let _ = 1; } else { let _ = 2; }\n\
             }",
        );
        // For every edge (a → b) recorded as a successor of a, b must
        // list a as a predecessor.
        for b in &cfg.blocks {
            for s in &b.successors {
                let preds = cfg.predecessors(*s);
                assert!(
                    preds.contains(&b.id),
                    "edge {} → {} is not in the predecessor list of {}",
                    b.id,
                    s,
                    s
                );
            }
        }
    }

    /// Reachability check: BFS from `from` looking for `to`.
    fn reaches(cfg: &Cfg, from: BlockId, to: BlockId) -> bool {
        let mut seen = vec![false; cfg.num_blocks()];
        let mut stack = vec![from];
        while let Some(b) = stack.pop() {
            if b == to {
                return true;
            }
            if seen[b] {
                continue;
            }
            seen[b] = true;
            for s in &cfg.blocks[b].successors {
                stack.push(*s);
            }
        }
        false
    }

    // ── defer / errdefer cleanup-edge lowering ────────────────────

    #[test]
    fn defer_body_use_appears_after_subsequent_consume() {
        // `let x = ...; defer use(x); consume(x);` — the defer
        // body's read of `x` must appear in a cleanup block
        // downstream of the consume site (NOT at the lexical defer
        // declaration position). Pre-round-12.41 the body was
        // walked inline, so the read landed before the consume in
        // source order — masking the use-after-move (the defer
        // actually fires at scope exit, after the consume).
        let cfg = cfg_of(
            "struct Box { v: i64 }\n\
             fn use_x(b: ref Box) { }\n\
             fn drop_x(b: Box) { }\n\
             fn main() {\n\
                 let x = Box { v: 0 };\n\
                 defer { use_x(x); }\n\
                 drop_x(x);\n\
             }",
        );
        // Find the function-entry block (records `let x` and the
        // `drop_x(x)` arg). Then find any block reading `x` that is
        // a successor of (or downstream of) the entry — the defer
        // cleanup chain.
        let entry_uses: Vec<&str> = cfg
            .block(cfg.entry)
            .uses
            .iter()
            .map(|u| u.binding.as_str())
            .collect();
        // Entry should record the consume of `x` (passed to drop_x).
        assert!(
            entry_uses.contains(&"x"),
            "entry should record `x` use from drop_x(x): {entry_uses:?}"
        );
        // Some block strictly downstream of the entry must record a
        // read of `x` from the defer body's `use_x(x)` call.
        let downstream_x_uses: usize = cfg
            .blocks
            .iter()
            .filter(|b| b.id != cfg.entry)
            .map(|b| b.uses.iter().filter(|u| u.binding == "x").count())
            .sum();
        assert!(
            downstream_x_uses >= 1,
            "expected ≥1 use of `x` in a cleanup block downstream of entry, got {downstream_x_uses}"
        );
    }

    #[test]
    fn defer_body_does_not_walk_inline_at_declaration_site() {
        // The defer body's content must NOT appear inline in the
        // block where the `defer` statement was declared. Post
        // round-12.41 the body lowers onto exit edges, so a
        // function whose only use of `x` is inside a defer body
        // should record `x` only in cleanup blocks — not in the
        // function-entry block. (Without the round, the inline
        // walk would record `x` directly in the entry block,
        // failing this assertion.)
        let cfg = cfg_of(
            "struct Box { v: i64 }\n\
             fn use_x(b: ref Box) { }\n\
             fn main() {\n\
                 let x = Box { v: 0 };\n\
                 defer { use_x(x); }\n\
             }",
        );
        let entry_x_uses = cfg
            .block(cfg.entry)
            .uses
            .iter()
            .filter(|u| u.binding == "x")
            .count();
        assert_eq!(
            entry_x_uses, 0,
            "defer body must not be walked inline in declaration block; entry has {entry_x_uses} uses of `x`"
        );
        // It must appear somewhere though — in a cleanup block
        // downstream of entry.
        let total_x: usize = cfg
            .blocks
            .iter()
            .map(|b| b.uses.iter().filter(|u| u.binding == "x").count())
            .sum();
        assert!(
            total_x >= 1,
            "defer body's read of `x` must appear in some cleanup block, got {total_x}"
        );
    }

    #[test]
    fn errdefer_body_is_emitted_on_question_error_edge() {
        // `errdefer` fires on error paths: a `?`-error edge must
        // route through a cleanup block that contains the
        // errdefer body's uses. (Compare to fall-through, which
        // does NOT include errdefer items.)
        let cfg = cfg_of(
            "struct E { msg: i64 }\n\
             struct T { v: i64 }\n\
             fn try_op() -> Result[T, E] { Ok(T { v: 0 }) }\n\
             fn cleanup(b: ref T) { }\n\
             fn main() -> Result[T, E] {\n\
                 let x = T { v: 0 };\n\
                 errdefer { cleanup(x); }\n\
                 let y = try_op()?;\n\
                 Ok(y)\n\
             }",
        );
        // Some block must read `x` (the errdefer body fires on ?-error).
        let x_reads: usize = cfg
            .blocks
            .iter()
            .map(|b| b.uses.iter().filter(|u| u.binding == "x").count())
            .sum();
        assert!(
            x_reads >= 1,
            "errdefer body must place a read of `x` in the ?-error cleanup chain, got {x_reads}"
        );
    }

    #[test]
    fn defer_lifo_order_in_fall_through_cleanup() {
        // Two defers in declaration order A then B; fall-through
        // cleanup must walk B's body before A's body. Encode this
        // by reading two distinct outer bindings and checking the
        // cleanup-block order.
        let cfg = cfg_of(
            "struct Box { v: i64 }\n\
             fn use_a(b: ref Box) { }\n\
             fn use_b(b: ref Box) { }\n\
             fn main() {\n\
                 let a = Box { v: 1 };\n\
                 let b = Box { v: 2 };\n\
                 defer { use_a(a); }\n\
                 defer { use_b(b); }\n\
             }",
        );
        // Walk forward from entry, collecting bindings read in source
        // order across blocks. The first cleanup block should read
        // `b` (last-declared defer fires first), a later block reads `a`.
        let mut seen_a_first = false;
        let mut a_block: Option<BlockId> = None;
        let mut b_block: Option<BlockId> = None;
        for blk in &cfg.blocks {
            for u in &blk.uses {
                if u.binding == "a" && a_block.is_none() {
                    a_block = Some(blk.id);
                    if b_block.is_none() {
                        seen_a_first = true;
                    }
                }
                if u.binding == "b" && b_block.is_none() {
                    b_block = Some(blk.id);
                }
            }
        }
        let (a_block, b_block) = (
            a_block.expect("defer body for `a` must record a use"),
            b_block.expect("defer body for `b` must record a use"),
        );
        // In LIFO order (b's defer fires first), `b`'s use should
        // appear in a block dominating `a`'s use, OR at minimum
        // `b`'s block id is less than `a`'s block id (block ids
        // are allocated in walk order, and the cleanup walker
        // emits LIFO). Check the latter as a robust ordering
        // signal that does not depend on the dominator computation.
        assert!(
            !seen_a_first,
            "expected b's defer to fire first (LIFO), but `a` was recorded earlier"
        );
        assert!(
            b_block < a_block,
            "expected b's cleanup block ({b_block}) to come before a's ({a_block}) in LIFO order"
        );
    }
}
