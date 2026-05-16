// src/ownership.rs

//! Ownership analysis for the Kāra language.
//!
//! Tracks value moves, detects use-after-move, infers parameter ownership
//! modes (own/ref/mut ref), and checks for ownership cycles in the type graph.

use crate::ast::*;
use crate::cfg::ConsumeOrigin;
use crate::rc_predicate::{direct_uam_candidates, run_predicate_for_function};
use crate::resolver::SpanKey;
use crate::token::Span;
use crate::typechecker::{FloatSize, IntSize, Type, TypeCheckResult, UIntSize};
use std::collections::{HashMap, HashSet};

mod block_stmt;
mod borrow;
mod capture_body;
mod closure_escape;
mod expr_check;
mod par_helpers;
mod rc_promote;

// ── Core Types ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum OwnershipMode {
    Own,
    Ref,
    MutRef,
}

impl std::fmt::Display for OwnershipMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnershipMode::Own => write!(f, "own"),
            OwnershipMode::Ref => write!(f, "ref"),
            OwnershipMode::MutRef => write!(f, "mut ref"),
        }
    }
}

/// A projection step from a root binding to a sub-place.
/// `Field("inner")` for `c.inner`, `Index` for `arr[i]` or `tup.0`,
/// `Range` for the half-open `v[a..b]` slice form (kept distinct from
/// scalar `Index` so future tighter analyses can treat range
/// projections separately).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Projection {
    Field(String),
    Index,
    Range,
}

/// A normalized place expression rooted at a named binding. Used by
/// the slice borrow tracker to attribute every slice view to the
/// original source binding (slice-of-slice resolves transitively to
/// the original `Vec` / `Array` / `Slice` storage). `projections`
/// lists the projection chain root-to-leaf — `c.inner[0]` → root
/// `"c"`, projections `[Field("inner"), Index]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlaceExpr {
    pub root: String,
    pub projections: Vec<Projection>,
}

/// Kind of an active borrow tracked by Slice 2's conflict matrix.
/// `Imm*` / `Mut*` distinguish read vs. write borrows; `*Ref` /
/// `*Slice` distinguish the borrow form. The four-way split lets the
/// matrix emit shape-correct diagnostics — slice-vs-slice conflicts
/// route through `SliceBorrowConflict`, slice-vs-ref through
/// `CrossBorrowConflict`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BorrowKind {
    ImmRef,
    MutRef,
    ImmSlice,
    MutSlice,
}

impl BorrowKind {
    fn is_slice(&self) -> bool {
        matches!(self, BorrowKind::ImmSlice | BorrowKind::MutSlice)
    }
    fn is_mut(&self) -> bool {
        matches!(self, BorrowKind::MutRef | BorrowKind::MutSlice)
    }
}

/// A live borrow recorded against a source binding. Pushed at slice
/// creation sites (and at fn-call boundaries for the scoped ref-side
/// push) and drained at block exit when `scope_depth > current_scope_depth`.
#[derive(Debug, Clone)]
pub struct ActiveBorrow {
    pub kind: BorrowKind,
    pub source: PlaceExpr,
    pub span: Span,
    pub scope_depth: usize,
}

/// Shape of a slice-vs-slice / slice-vs-source-state-change conflict.
/// Used to route the rendered diagnostic message variant for
/// `error[E_SLICE_BORROW_CONFLICT]`. Cross-borrow conflicts (slice +
/// `ref T` / `mut ref T` of the same root) use a separate
/// `CrossBorrowConflict` variant so their diagnostic family stays
/// distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceConflictShape {
    /// Shape A: an immutable slice and a mutable slice both borrow
    /// the same source. Either order — the existing borrow may be
    /// imm and the new one mut, or vice versa.
    ImmSliceVsMutSlice,
    /// Shape B: two mutable slices of the same source.
    MutSliceVsMutSlice,
    /// Shape C: source binding consumed (moved) while a slice borrow
    /// is live.
    MoveOfBorrowed,
    /// Shape D: slice's lifetime extends past its source binding's
    /// scope (source dropped while slice still live). v1 detects this
    /// at block-exit drain when the source's scope is exiting and a
    /// slice into it was bound at a shallower scope.
    DropOfBorrowed,
}

#[derive(Debug, Clone)]
pub(crate) enum ValueState {
    Live,
    /// Declared via `let x: T;` (LetUninit) but not yet assigned.
    /// Reading errors with UseOfUninitialized; the first assignment
    /// promotes — to `Live` if `is_mut`, to `InitOnce` otherwise.
    Uninit {
        let_span: Span,
        is_mut: bool,
    },
    /// A non-mut LetUninit binding that has been assigned exactly once.
    /// Reads succeed, but a second assignment errors (the binding was
    /// declared without `mut`). Per design.md "first assignment is
    /// initialization, not reassignment".
    InitOnce {
        first_assign: Span,
    },
    /// The binding has been consumed at `at`. Round 12.42 collapsed
    /// the former `MoveKind` enum (Direct / BranchMerged /
    /// ClosureCapture / ContainerStore) into a single `Moved` state:
    /// the predicate pipeline now drives every diagnostic and every
    /// `rc_values` flavor decision (rounds 12.16 / 12.17 / 12.21 /
    /// 12.38), so the kind tag no longer routes anything. The legacy
    /// state machine's remaining job is binary — "is this binding
    /// live or moved?" — which feeds (a) `handle_moved_use`'s
    /// short-circuit on already-erroring identifier walks and
    /// (b) the closure-capture mode classifier in `check_expr_consuming`'s
    /// `Closure` arm (Live → consumed-by-body iff post-walk `Moved`).
    Moved {
        at: Span,
    },
}

/// Trigger that caused the compiler to insert RC for a value.
#[derive(Debug, Clone, PartialEq)]
pub enum RcTrigger {
    DirectReuseAfterConsume,
    ClosureCaptureWithOuterUse,
    /// Value moved into a container (a `mut ref self` method's owned arg)
    /// and used again after the call. Per design.md § Part 4 trigger 3.
    ContainerStoreWithSubsequentUse,
}

impl RcTrigger {
    fn label(&self) -> &'static str {
        match self {
            RcTrigger::DirectReuseAfterConsume => "direct re-use after consume",
            RcTrigger::ClosureCaptureWithOuterUse => "closure capture with outer use",
            RcTrigger::ContainerStoreWithSubsequentUse => "container store with subsequent use",
        }
    }
}

/// Per-binding RC fallback record. Recorded once per binding per
/// function the first time the trigger fires.
#[derive(Debug, Clone)]
pub struct RcEntry {
    pub binding: String,
    pub trigger: RcTrigger,
    pub consume_span: Span,
    pub other_use_span: Span,
    /// Optional type name of the binding (used for `@no_rc` enforcement).
    pub type_name: Option<String>,
}

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OwnershipError {
    pub message: String,
    pub span: Span,
    pub kind: OwnershipErrorKind,
    pub suggestion: Option<String>,
    /// Machine-applicable rewrite for the diagnostic, when one exists.
    /// Today: N0507 (UnusedMutCaptureNote) carries an edit replacing
    /// `mut ref` with `ref` over the closure prefix span. Other kinds
    /// emit `None` because their suggestions are descriptive prose.
    /// Boxed so the sparse `Some` case doesn't bloat the error vector
    /// past clippy's `result_large_err` / large-enum heuristics.
    pub replacement: Option<Box<crate::resolver::TextEdit>>,
    /// Secondary span carrying the consume site for `UseAfterMove`
    /// diagnostics. `span` is the offending later-use site; this field
    /// records *where* the binding was consumed. Threaded so REPL-aware
    /// diagnostic enrichment can map the consume site to its origin
    /// cell (see `Session::cell_for_span`). `None` for every other
    /// diagnostic kind.
    pub consume_span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OwnershipErrorKind {
    UseAfterMove,
    OwnershipCycle,
    /// A value of a `@no_rc` type or inside a `#[no_rc]` function
    /// would require RC fallback.
    NoRcViolation,
    /// Performance note: the compiler inserted RC fallback. Not blocking.
    RcFallbackNote,
    /// A closure declared `ref |...|` or `mut ref |...|` consumed a
    /// captured value in its body. Per Rule 2½ K2 conflict table —
    /// declared mode is the floor; body usage may not exceed it.
    CaptureModeViolation,
    /// Read of a binding declared via `let x: T;` before any assignment
    /// reached this program point. Definite-assignment failure.
    UseOfUninitialized,
    /// A `let x: T;` (no `mut`) binding was assigned more than once.
    /// First assignment is initialization; a second requires `let mut`.
    ReassignToImmutable,
    /// Performance note: a closure declared with `mut ref |...|` reads but
    /// never mutates a captured name. Per Rule 2½ K2 conflict table — the
    /// declared mode is stronger than the body's actual usage; suggest
    /// dropping `mut ref` to plain `ref`.
    UnusedMutCaptureNote,
    /// A closure with one or more `ref` / `mut ref` captures escapes its
    /// creation scope (today: returned from the enclosing function, either
    /// directly or via a let-bound rebind). The captured value is owned by
    /// the current function; the ref capture would outlive its source.
    /// Per design.md § Closures Rule 2 sub-case (iv).
    RefCaptureEscapesScope,
    /// A slice was created from a temporary value (a function call result,
    /// composite literal, etc. — anything without a rooted source binding)
    /// and bound to a name that escapes the enclosing statement. The
    /// slice's storage would be dropped at end-of-statement leaving the
    /// binding pointing at freed memory. Phase-5 § Slice borrow source
    /// attribution sub-step (d).
    SliceFromTemporaryEscapes,
    /// Slice-vs-slice or slice-vs-source-state-change conflict against a
    /// shared source binding. The `shape` field selects the diagnostic
    /// message variant (imm + mut, mut + mut, move-of-borrowed,
    /// drop-of-borrowed). Phase-5 § Slice borrow conflict detection
    /// sub-step (d) / (e) / (f).
    SliceBorrowConflict {
        shape: SliceConflictShape,
    },
    /// A slice borrow and a `ref T` / `mut ref T` borrow are simultaneously
    /// live against the same source. Distinct from the slice-vs-slice
    /// `SliceBorrowConflict` family because the diagnostic wording names
    /// the cross-form pairing. v1 surfaces this for in-call mutation of
    /// a source while a slice into it is live (`v.push(...)` with
    /// `let s = v.as_slice_mut();` outstanding). Phase-5 § Slice borrow
    /// conflict detection sub-step (g).
    CrossBorrowConflict,
}

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

// ── Result ──────────────────────────────────────────────────────

pub struct OwnershipCheckResult {
    /// Inferred parameter modes: function name → [(param_name, mode)].
    pub param_modes: HashMap<String, Vec<(String, OwnershipMode)>>,
    /// Inferred per-closure parameter modes (round 12.23 — Closure
    /// ownership Step 1). Keyed by `SpanKey` of the closure
    /// expression. Each entry lists the closure's parameters in
    /// source order with the inferred mode (`own` / `ref` /
    /// `mut ref`) derived from a use-predicate scan over the body
    /// — the same `ParamUsage`-driven classification fn-param
    /// inference uses, applied with each closure parameter as the
    /// subject.
    pub closure_param_modes: HashMap<SpanKey, Vec<(String, OwnershipMode)>>,
    /// Inferred per-closure capture lists (round 12.24 — Closure
    /// ownership Step 2). Keyed by `SpanKey` of the closure
    /// expression. Each entry lists the names captured from an
    /// enclosing scope along with the capture mode derived from
    /// body usage: `Own` for consume-captures (the body moved the
    /// outer binding via the closure), `MutRef` when the body only
    /// mutates the captured binding through projection / call-site
    /// `mut` markers / `mut ref self` receivers, and `Ref` for
    /// read-only captures. Names that are referenced only as
    /// closure-local rebindings (let-shadowed inside the body) are
    /// not captured. The ordering inside each `Vec` is unspecified
    /// — captures form a set semantically; the alphabetic sort at
    /// emission time gives stable output for tests / `karac
    /// explain`.
    pub closure_captures: HashMap<SpanKey, Vec<(String, OwnershipMode)>>,
    /// Closure expression span → enclosing function key (round
    /// 12.25). Lets `karac query ownership <fn>` filter
    /// `closure_param_modes` / `closure_captures` to closures whose
    /// creation site lies within the queried function. The function
    /// key follows the same convention as `param_modes` /
    /// `rc_values`: bare name for free functions, `"Type.method"`
    /// for impl methods.
    pub closure_function: HashMap<SpanKey, String>,
    /// Closure expression `SpanKey` → full `Span`. The other
    /// closure-keyed maps store only `SpanKey` (offset+length); this
    /// table makes line/column available to consumers that surface
    /// closure-creation locations (e.g. `karac query ownership`).
    pub closure_spans: HashMap<SpanKey, Span>,
    pub errors: Vec<OwnershipError>,
    /// Non-blocking notes (e.g. RC fallback perf notes). Distinct from
    /// `errors` so callers can render them separately.
    pub notes: Vec<OwnershipError>,
    /// Representation for each binding/parameter: "owned (stack)", "ref (borrow)",
    /// "shared (Rc)", "shared (Arc)". Key: "function_name.binding_name".
    pub representations: HashMap<String, String>,
    /// Per-function RC values produced by Phase 1. Function name → binding name → entry.
    pub rc_values: HashMap<String, HashMap<String, RcEntry>>,
    /// Per-function Arc-promoted bindings (Phase 2). Subset of `rc_values`.
    pub arc_values: HashMap<String, HashSet<String>>,
    /// Per-slice-creation-site borrow source attribution. Keyed by the
    /// slice expression's `SpanKey` — the `.as_slice()` / `.as_slice_mut()`
    /// MethodCall, the range-indexing `Index`, the let-RHS, or the
    /// implicitly-coerced call-arg expression. The value is the resolved
    /// root place plus the slice's mutability. Slice-of-slice creations
    /// are walked through to the original root, so an entry's `root`
    /// always names the storage binding (never an intermediate slice).
    /// Populated by Phase-5 Theme 1 Slice 1 (borrow source attribution);
    /// consumed by Slice 2's conflict detector.
    pub slice_borrow_sources: HashMap<SpanKey, (PlaceExpr, bool)>,
}

// ── Copy Type Detection ─────────────────────────────────────────

fn is_copy_type_basic(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Unit
            | Type::Never
            | Type::Error
    )
}

/// Free-function form of `OwnershipChecker::is_copy_type`. Lives here so
/// auxiliary passes (use classifier, future RC-fallback predicate driver)
/// can answer the same question without instantiating an `OwnershipChecker`.
pub(crate) fn is_copy_type(ty: &Type, tc: &TypeCheckResult) -> bool {
    if is_copy_type_basic(ty) {
        return true;
    }
    match ty {
        Type::Tuple(types) => types.iter().all(|t| is_copy_type(t, tc)),
        Type::Array { element, .. } => is_copy_type(element, tc),
        Type::Slice { mutable, .. } => !mutable,
        // RC-tier types — `shared struct S`, `Rc[T]`, `Arc[T]` — are
        // cheap-clone reference handles. A use does not consume the
        // originating binding; the runtime bumps the refcount. Without
        // this arm, `let mut q = VecDeque.new(); q.push_back(node);
        // ... return node;` fires a spurious UAM on `node` because
        // `push_back`'s owned-arg slot is classified as Consume.
        Type::Shared(_) | Type::Rc(_) | Type::Arc(_) => true,
        Type::Named { name, args } => {
            if matches!(name.as_str(), "Option" | "Result") {
                return args.iter().all(|a| is_copy_type(a, tc));
            }
            if let Some(info) = tc.struct_info.get(name) {
                info.derived_traits.contains("Copy")
            } else if let Some(info) = tc.enum_info.get(name) {
                info.derived_traits.contains("Copy")
            } else if let Some(traits) = tc.distinct_type_traits.get(name) {
                traits.contains("Copy")
            } else {
                false
            }
        }
        _ => false,
    }
}

// ── Ownership Checker ───────────────────────────────────────────

pub struct OwnershipChecker<'a> {
    pub(crate) program: &'a Program,
    pub(crate) typecheck_result: &'a TypeCheckResult,
    pub(crate) param_modes: HashMap<String, Vec<(String, OwnershipMode)>>,
    /// Inferred closure parameter modes (round 12.23). Keyed by the
    /// closure expression's `SpanKey`; values mirror `param_modes`'s
    /// per-fn `(name, mode)` shape. Surfaced via
    /// `OwnershipCheckResult::closure_param_modes`.
    pub(crate) closure_param_modes: HashMap<SpanKey, Vec<(String, OwnershipMode)>>,
    /// Inferred closure captures (round 12.24). Keyed by the closure
    /// expression's `SpanKey`. Surfaced via
    /// `OwnershipCheckResult::closure_captures`.
    pub(crate) closure_captures: HashMap<SpanKey, Vec<(String, OwnershipMode)>>,
    /// Closure span → enclosing function key (round 12.25). Built
    /// up at every `Closure` arm visit alongside the param/capture
    /// inference. Surfaced via `OwnershipCheckResult::closure_function`.
    pub(crate) closure_function: HashMap<SpanKey, String>,
    /// Closure `SpanKey` → full `Span`. Surfaced via
    /// `OwnershipCheckResult::closure_spans`.
    pub(crate) closure_spans: HashMap<SpanKey, Span>,
    pub(crate) errors: Vec<OwnershipError>,
    pub(crate) notes: Vec<OwnershipError>,
    /// Per-function RC values populated during Phase 1.
    pub(crate) rc_values: HashMap<String, HashMap<String, RcEntry>>,
    /// Per-function Arc-promoted values populated during Phase 2.
    pub(crate) arc_values: HashMap<String, HashSet<String>>,
    /// Function currently being analysed (key into the per-function maps).
    pub(crate) current_function: String,
    /// Whether the current function suppresses RC fallback notes via
    /// `#[allow(rc_fallback)]`. Errors from `#[no_rc]` / `@no_rc` are
    /// not suppressed.
    pub(crate) suppress_rc_notes: bool,
    /// Function keys where RC notes are suppressed via `#[allow(rc_fallback)]`.
    /// Consulted after Phase 2 when emitting flavor-annotated notes.
    pub(crate) suppressed_rc_fn_keys: HashSet<String>,
    /// Type name of each binding in scope for the current function.
    /// Used so RC trigger sites can look up `@no_rc` on the type.
    pub(crate) binding_type_names: HashMap<String, String>,
    /// Full type of each binding in scope for the current function.
    /// Parallel to `binding_type_names` but stores the structured `Type`
    /// rather than just the head name. Populated at the param-scan and
    /// at every `let` binding's RHS span (which is unaliased — unlike
    /// the LHS / chained-access spans the typechecker may overwrite).
    /// Consumed by `consume_named_binding` to look up Copy-ness without
    /// going through the unreliable `expr_types[span]` path.
    pub(crate) binding_types: HashMap<String, Type>,
    // Round 12.38 — once-callable closure tracking removed from the
    // ownership-side state machine. Detection now lives in
    // `use_classifier::UseClassifier::once_callable_closures` (round
    // 12.20); UAM/RC emission is owned by `populate_predicate_outputs`.
    /// `Type.method` → declared receiver mode (`self` / `ref self` /
    /// `mut ref self`). Populated once at construction by walking the
    /// program's impl blocks and trait declarations. Consulted at every
    /// `MethodCall` to drive consume-vs-read classification of the receiver
    /// per design.md § Consume Predicate step 1.
    pub(crate) method_self_modes: HashMap<String, SelfParam>,
    /// Callee name → per-position parameter ownership modes. Free functions
    /// are keyed by bare name (`"my_fn"`); static methods (impl methods
    /// with no `self_param`) are keyed by `"Type.method"`. The mode of
    /// each position is derived from the syntactic param type — `ref T`
    /// → `Ref`, `mut ref T` / `mut Slice[T]` → `MutRef`, otherwise
    /// `Own`. Drives `Call`-arg consume-vs-read classification per
    /// design.md § Consume Predicate step 2.
    pub(crate) callee_param_modes: HashMap<String, Vec<OwnershipMode>>,
    /// Callee name → per-position "is the formal a slice?" flag. `Some(true)`
    /// for `mut Slice[T]`, `Some(false)` for `Slice[T]`, `None` for
    /// non-slice formals. Drives the Slice 1 call-arg coercion site
    /// detection: when a Vec / Array / Slice expression flows into a
    /// formal slot whose type is `Slice[T]` / `mut Slice[T]`, the
    /// implicit coercion creates a slice view that needs source
    /// attribution. Same key convention as `callee_param_modes`.
    pub(crate) callee_param_slice_kind: HashMap<String, Vec<Option<bool>>>,
    /// Slice creation sites recorded by Slice 1. Surfaced via
    /// `OwnershipCheckResult::slice_borrow_sources`. Populated at
    /// `.as_slice()` / `.as_slice_mut()`, range-indexing, and call-arg
    /// coercion sites; the let-binding-rhs site reuses whichever
    /// recording its RHS expression already produced.
    pub(crate) slice_borrow_sources: HashMap<SpanKey, (PlaceExpr, bool)>,
    /// Per-binding slice source attribution. Populated at `let pat = rhs`
    /// time when the RHS is a slice creation expression — the binding
    /// name maps to the same `(PlaceExpr, mutable)` pair recorded for the
    /// RHS's span. Consumed by `place_expr_root` so a use of the binding
    /// in a later slice creation chains through to the original storage
    /// root rather than the intermediate slice.
    pub(crate) slice_binding_sources: HashMap<String, (PlaceExpr, bool)>,
    /// Slice 2 — active borrow stack per source root binding name. Pushed
    /// at slice creation sites and at the call-statement-scoped ref-side
    /// boundary; drained at block exit when an entry's `scope_depth` is
    /// strictly greater than the current scope depth. Conflict detection
    /// scans this list at every push to find slice-vs-slice and
    /// slice-vs-ref overlaps against the same root.
    pub(crate) active_borrows: HashMap<String, Vec<ActiveBorrow>>,
    /// Slice 2 — current block scope depth, incremented on `check_block`
    /// entry and decremented on exit. Used to stamp `ActiveBorrow` and
    /// to drive the drain-on-exit cleanup. Top-level fn body sits at
    /// depth 1 after entry; nested blocks bump deeper.
    pub(crate) current_scope_depth: usize,
    /// Slice 2 — scope depth at which each binding was declared. Used by
    /// drop-of-borrowed detection: at block-exit drain, a source binding
    /// whose scope ends now (`scope_depth == current_scope_depth`) with
    /// any live slice into it whose own binding scope is shallower
    /// triggers shape D.
    pub(crate) binding_scope_depth: HashMap<String, usize>,
    /// Slice 2 — scope depth at which each slice binding was declared.
    /// Populated at the `StmtKind::Let` arm when the RHS produced a
    /// `slice_borrow_sources` entry. Drives the drop-of-borrowed
    /// trigger comparison.
    pub(crate) slice_binding_scope_depth: HashMap<String, usize>,
}

impl<'a> OwnershipChecker<'a> {
    pub fn new(program: &'a Program, typecheck_result: &'a TypeCheckResult) -> Self {
        OwnershipChecker {
            program,
            typecheck_result,
            param_modes: HashMap::new(),
            closure_param_modes: HashMap::new(),
            closure_captures: HashMap::new(),
            closure_function: HashMap::new(),
            closure_spans: HashMap::new(),
            errors: Vec::new(),
            notes: Vec::new(),
            rc_values: HashMap::new(),
            arc_values: HashMap::new(),
            current_function: String::new(),
            suppress_rc_notes: false,
            suppressed_rc_fn_keys: HashSet::new(),
            binding_type_names: HashMap::new(),
            binding_types: HashMap::new(),
            method_self_modes: collect_method_self_modes(program),
            callee_param_modes: collect_callee_param_modes(program),
            callee_param_slice_kind: collect_callee_param_slice_kind(program),
            slice_borrow_sources: HashMap::new(),
            slice_binding_sources: HashMap::new(),
            active_borrows: HashMap::new(),
            current_scope_depth: 0,
            binding_scope_depth: HashMap::new(),
            slice_binding_scope_depth: HashMap::new(),
        }
    }

    /// Check whether a type is Copy — primitives, or named types with #[derive(Copy)].
    fn is_copy_type(&self, ty: &Type) -> bool {
        is_copy_type(ty, self.typecheck_result)
    }

    pub fn check(mut self) -> OwnershipCheckResult {
        self.check_cycles();
        self.check_items();
        self.promote_rc_to_arc();
        self.emit_rc_fallback_notes();
        self.enforce_no_rc_attrs();

        // Build representations: parameter modes first, then overlay RC/Arc
        // for any binding (parameter or local) flagged by Phase 1/2.
        let mut representations = HashMap::new();
        for (func_name, modes) in &self.param_modes {
            for (param_name, mode) in modes {
                let key = format!("{}.{}", func_name, param_name);
                let repr = if Self::contains_in(&self.arc_values, func_name, param_name) {
                    "shared (Arc)"
                } else if Self::contains_in_map(&self.rc_values, func_name, param_name)
                    || self
                        .param_type_head(func_name, param_name)
                        .as_deref()
                        .is_some_and(|n| self.is_shared_type(n))
                {
                    "shared (Rc)"
                } else {
                    match mode {
                        OwnershipMode::Own => "owned (stack)",
                        OwnershipMode::Ref | OwnershipMode::MutRef => "ref (borrow)",
                    }
                };
                representations.insert(key, repr.to_string());
            }
        }
        for (func_name, rc_map) in &self.rc_values {
            for binding in rc_map.keys() {
                let key = format!("{}.{}", func_name, binding);
                let repr = if Self::contains_in(&self.arc_values, func_name, binding) {
                    "shared (Arc)"
                } else {
                    "shared (Rc)"
                };
                representations
                    .entry(key)
                    .or_insert_with(|| repr.to_string());
            }
        }

        OwnershipCheckResult {
            param_modes: self.param_modes,
            closure_param_modes: self.closure_param_modes,
            closure_captures: self.closure_captures,
            closure_function: self.closure_function,
            closure_spans: self.closure_spans,
            errors: self.errors,
            notes: self.notes,
            representations,
            rc_values: self.rc_values,
            arc_values: self.arc_values,
            slice_borrow_sources: self.slice_borrow_sources,
        }
    }

    fn contains_in(map: &HashMap<String, HashSet<String>>, fk: &str, bk: &str) -> bool {
        map.get(fk).is_some_and(|s| s.contains(bk))
    }

    fn contains_in_map(
        map: &HashMap<String, HashMap<String, RcEntry>>,
        fk: &str,
        bk: &str,
    ) -> bool {
        map.get(fk).is_some_and(|m| m.contains_key(bk))
    }

    // ── Cycle Detection ─────────────────────────────────────────

    fn check_cycles(&mut self) {
        // Build ownership graph: type name → owned field type names
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();

        for (name, info) in &self.typecheck_result.struct_info {
            let mut edges = Vec::new();
            for (_, field_ty, _) in &info.fields {
                if let Some(target) = owned_type_name(field_ty) {
                    edges.push(target);
                }
            }
            graph.insert(name.clone(), edges);
        }

        for (name, info) in &self.typecheck_result.enum_info {
            let mut edges = Vec::new();
            for (_, variant) in &info.variants {
                match variant {
                    crate::typechecker::VariantTypeInfo::Tuple(types) => {
                        for ty in types {
                            if let Some(target) = owned_type_name(ty) {
                                edges.push(target);
                            }
                        }
                    }
                    crate::typechecker::VariantTypeInfo::Struct(fields) => {
                        for (_, ty) in fields {
                            if let Some(target) = owned_type_name(ty) {
                                edges.push(target);
                            }
                        }
                    }
                    crate::typechecker::VariantTypeInfo::Unit => {}
                }
            }
            graph.insert(name.clone(), edges);
        }

        // DFS for cycles
        let mut visited = HashSet::new();
        let mut in_stack = HashSet::new();

        let all_types: Vec<String> = graph.keys().cloned().collect();
        for type_name in &all_types {
            if !visited.contains(type_name) {
                self.dfs_cycle(
                    type_name,
                    &graph,
                    &mut visited,
                    &mut in_stack,
                    &mut Vec::new(),
                );
            }
        }
    }

    fn dfs_cycle(
        &mut self,
        node: &str,
        graph: &HashMap<String, Vec<String>>,
        visited: &mut HashSet<String>,
        in_stack: &mut HashSet<String>,
        path: &mut Vec<String>,
    ) {
        visited.insert(node.to_string());
        in_stack.insert(node.to_string());
        path.push(node.to_string());

        if let Some(neighbors) = graph.get(node) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    self.dfs_cycle(neighbor, graph, visited, in_stack, path);
                } else if in_stack.contains(neighbor) {
                    // Found a cycle
                    let cycle_start = path.iter().position(|n| n == neighbor).unwrap_or(0);
                    let cycle: Vec<&str> = path[cycle_start..].iter().map(|s| s.as_str()).collect();

                    // Find span for the type that starts the cycle
                    let span = self.find_type_span(node);
                    let all_shared = cycle.iter().all(|n| self.is_shared_type(n));
                    let (message, suggestion) = if all_shared {
                        (
                            format!(
                                "shared-type cycle detected: {} → {}. Shared types use reference counting — a cycle without a 'weak' edge will leak.",
                                cycle.join(" → "),
                                neighbor,
                            ),
                            Some("add 'weak' to one field in the back-edge of the cycle".to_string()),
                        )
                    } else {
                        (
                            format!(
                                "ownership cycle detected: {} → {}. A non-shared type cannot transitively contain itself.",
                                cycle.join(" → "),
                                neighbor,
                            ),
                            Some("use 'ref', 'Box[T]', or mark the type as 'shared'".to_string()),
                        )
                    };
                    self.errors.push(OwnershipError {
                        message,
                        span,
                        kind: OwnershipErrorKind::OwnershipCycle,
                        suggestion,
                        replacement: None,
                        consume_span: None,
                    });
                }
            }
        }

        in_stack.remove(node);
        path.pop();
    }

    /// Look up whether a named struct/enum is declared as `shared`.
    fn is_shared_type(&self, name: &str) -> bool {
        if let Some(info) = self.typecheck_result.struct_info.get(name) {
            return info.is_shared;
        }
        if let Some(info) = self.typecheck_result.enum_info.get(name) {
            return info.is_shared;
        }
        false
    }

    /// Look up the head type name of a function parameter by walking the
    /// program. `func_name` is the fn_key used in `param_modes` — either a
    /// bare function name or `"TypeName.method"` for impl methods. Returns
    /// the outermost Named type, peeling `ref`/`mut ref`/`weak` wrappers.
    fn param_type_head(&self, func_name: &str, param_name: &str) -> Option<String> {
        let (target_type, fn_name) = match func_name.split_once('.') {
            Some((t, m)) => (Some(t), m),
            None => (None, func_name),
        };
        for item in &self.program.items {
            match item {
                Item::Function(f) if target_type.is_none() && f.name == fn_name => {
                    return f
                        .params
                        .iter()
                        .find(|p| p.name() == Some(param_name))
                        .and_then(|p| type_expr_head(&p.ty));
                }
                Item::ImplBlock(imp) if target_type.is_some() => {
                    let t = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().map(|s| s.as_str()),
                        _ => None,
                    };
                    if t != target_type {
                        continue;
                    }
                    for ii in &imp.items {
                        if let ImplItem::Method(m) = ii {
                            if m.name == fn_name {
                                return m
                                    .params
                                    .iter()
                                    .find(|p| p.name() == Some(param_name))
                                    .and_then(|p| type_expr_head(&p.ty));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn find_type_span(&self, type_name: &str) -> Span {
        for item in &self.program.items {
            match item {
                Item::StructDef(s) if s.name == type_name => return s.span.clone(),
                Item::EnumDef(e) if e.name == type_name => return e.span.clone(),
                _ => {}
            }
        }
        Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        }
    }

    // ── Per-Item Analysis ───────────────────────────────────────

    fn check_items(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) => self.check_function(f, None),
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for item in &imp.items {
                        if let ImplItem::Method(method) = item {
                            self.check_function(method, Some(&type_name));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn check_function(&mut self, f: &Function, impl_type: Option<&str>) {
        let fn_key = if let Some(t) = impl_type {
            format!("{}.{}", t, f.name)
        } else {
            f.name.clone()
        };

        self.current_function = fn_key.clone();
        self.suppress_rc_notes = f.attributes.iter().any(|a| {
            a.name == "allow"
                && a.args.iter().any(|arg| {
                    // `#[allow(rc_fallback)]` — positional arg whose value
                    // is the bare identifier `rc_fallback`.
                    if let Some(Expr {
                        kind: ExprKind::Identifier(name),
                        ..
                    }) = &arg.value
                    {
                        name == "rc_fallback"
                    } else {
                        false
                    }
                })
        });
        if self.suppress_rc_notes {
            self.suppressed_rc_fn_keys.insert(fn_key.clone());
        }
        self.binding_type_names.clear();
        self.binding_types.clear();
        // Slice 2 — reset per-function active borrow tracking. The
        // result-surfaced `slice_borrow_sources` is NOT cleared (it
        // accumulates across the program); the other maps are function-
        // local because binding names are.
        self.slice_binding_sources.clear();
        self.active_borrows.clear();
        self.binding_scope_depth.clear();
        self.slice_binding_scope_depth.clear();
        self.current_scope_depth = 0;

        // Initialize value states for parameters
        let mut states: HashMap<String, ValueState> = HashMap::new();
        let mut param_types: HashMap<String, Type> = HashMap::new();

        // Slice 2 — params are scoped to the body block (depth 1, after
        // `check_block` bumps). Register at depth 1 so the
        // drop-of-borrowed trigger lines up correctly when slices into
        // params are bound inside the body.
        let body_depth = self.current_scope_depth + 1;

        for param in &f.params {
            let ty = self.lower_type_for_ownership(&param.ty);
            for name in param.pattern.binding_names() {
                states.insert(name.clone(), ValueState::Live);
                if let Some(tn) = type_name(&ty) {
                    self.binding_type_names.insert(name.clone(), tn);
                }
                self.binding_types.insert(name.clone(), ty.clone());
                param_types.insert(name.clone(), ty.clone());
                self.binding_scope_depth.insert(name, body_depth);
            }
        }

        if f.self_param.is_some() {
            states.insert("self".to_string(), ValueState::Live);
            if let Some(t) = impl_type {
                self.binding_type_names
                    .insert("self".to_string(), t.to_string());
            }
        }

        // Track parameter usage for mode inference
        let mut param_usage: HashMap<String, ParamUsage> = HashMap::new();
        for param in &f.params {
            for name in param.pattern.binding_names() {
                param_usage.insert(name, ParamUsage::Unused);
            }
        }

        // Round 12.16 + 12.21: predicate pre-pass populates
        // `rc_values` AND emits `UseAfterMove` errors for this
        // function before the linear forward state machine walks
        // the body. The flavor labeling (12.14) maps each RC
        // witness's `consume_origin` onto an `RcTrigger`; UAM
        // witnesses (12.15) drive direct error emission. With both
        // wirings, the legacy `handle_moved_use` short-circuits in
        // every kind variant — RC arms via `already_rc=true`
        // (round 12.16/17) and the `Direct` arm via the predicate's
        // own emission (this round). The state machine still walks
        // the body for state tracking (parent-state propagation,
        // branch merging, K2 closure-capture retag); per round 12.38
        // once-callable detection migrated entirely into the predicate
        // pipeline (`UseClassifier`'s `once_callable_closures` set,
        // populated at let-RHS-is-closure sites with a captured-owned
        // signal — see round 12.20).
        self.populate_predicate_outputs(f, &fn_key);

        // Walk the body
        self.check_block(&f.body, &mut states, &param_types, &mut param_usage);

        // Round 12.35–12.39 — Closure ownership Step 7: detect ref-
        // captured values that escape their borrow's lifetime. A
        // closure with `ref` / `mut ref` capture of a binding owned by
        // the current function (parameter or local let, type not
        // itself a borrow) is rejected when the closure value escapes
        // via (a) return — direct, let-bound rebind, or implicit
        // tail-expression form (round 12.35); (b) embedded in a
        // composite literal that's returned (round 12.36); (c)
        // let-bound carrier then returned (round 12.37); or (d)
        // passed as a fn-arg to an Own-mode parameter slot (round
        // 12.39, conservative-fire — the slot may or may not actually
        // store the closure beyond the call, but without inter-
        // procedural analysis we cannot tell). Sub-case (d) is
        // suppressed by `#[allow(ref_capture_escape)]` on the
        // enclosing function. Per design.md § Closures Rule 2 sub-
        // case (iv). Emits E0508 at the closure expression with a
        // three-fix message.
        self.check_closure_ref_capture_escapes(f);

        // Infer parameter modes
        let mut modes: Vec<(String, OwnershipMode)> = Vec::new();
        for param in &f.params {
            for name in param.pattern.binding_names() {
                let usage = param_usage
                    .get(&name)
                    .cloned()
                    .unwrap_or(ParamUsage::Unused);
                let mode = match usage {
                    ParamUsage::Unused | ParamUsage::Read => OwnershipMode::Ref,
                    ParamUsage::Mutated => OwnershipMode::MutRef,
                    ParamUsage::Consumed => OwnershipMode::Own,
                };
                modes.push((name, mode));
            }
        }
        self.param_modes.insert(fn_key, modes);
    }

    /// Run both predicate passes over the function body in a single
    /// CFG/dominator construction. Round 12.16 populates `rc_values`
    /// from the formal RC predicate (`rc_candidates`); round 12.21
    /// emits `UseAfterMove` errors from `direct_uam_candidates`. With
    /// both passes wired, the legacy `handle_moved_use` short-
    /// circuits in every kind variant — RC arms via `already_rc=true`
    /// and the `Direct` arm via the predicate's own emission — so the
    /// linear forward state machine no longer drives diagnostic
    /// output for these shapes.
    fn populate_predicate_outputs(&mut self, f: &Function, fn_key: &str) {
        let (cfg, dom, rc_witnesses) =
            run_predicate_for_function(self.program, self.typecheck_result, f);
        for (binding, w) in rc_witnesses {
            let trigger = match w.consume_origin {
                ConsumeOrigin::Direct => RcTrigger::DirectReuseAfterConsume,
                ConsumeOrigin::ClosureCapture => RcTrigger::ClosureCaptureWithOuterUse,
                ConsumeOrigin::ContainerStore => RcTrigger::ContainerStoreWithSubsequentUse,
            };
            let type_name = self.binding_type_names.get(&binding).cloned();
            let entry = RcEntry {
                binding: binding.clone(),
                trigger,
                consume_span: w.consume_span,
                other_use_span: w.other_use_span,
                type_name,
            };
            self.rc_values
                .entry(fn_key.to_string())
                .or_default()
                .insert(binding, entry);
        }
        // Round 12.21: emit UseAfterMove errors directly from the
        // predicate's UAM witnesses. One error per binding (the
        // first witness in source order). Bindings already routed
        // through `rc_values` are mutually exclusive with UAM
        // witnesses by predicate construction (RC fires only for
        // dominance-incomparable C, U; UAM fires only for
        // dominance-comparable C, U), so no de-duplication needed.
        let uam_witnesses = direct_uam_candidates(&cfg, &dom);
        for (binding, w) in uam_witnesses {
            self.errors.push(OwnershipError {
                message: format!(
                    "value '{}' moved here, used again here (moved at line {}:{})",
                    binding, w.consume_span.line, w.consume_span.column
                ),
                span: w.other_use_span,
                kind: OwnershipErrorKind::UseAfterMove,
                suggestion: Some(format!(
                    "consider cloning '{}' before the move, or restructure to avoid reuse",
                    binding
                )),
                replacement: None,
                consume_span: Some(w.consume_span),
            });
        }
    }

    fn lower_type_for_ownership(&self, ty: &TypeExpr) -> Type {
        // Simple type lowering for ownership — just need to know if it's copy
        match &ty.kind {
            TypeKind::Path(path) if path.segments.len() == 1 => {
                let name = &path.segments[0];
                match name.as_str() {
                    "i8" => Type::Int(IntSize::I8),
                    "i16" => Type::Int(IntSize::I16),
                    "i32" => Type::Int(IntSize::I32),
                    "i64" => Type::Int(IntSize::I64),
                    "u8" => Type::UInt(UIntSize::U8),
                    "u16" => Type::UInt(UIntSize::U16),
                    "u32" => Type::UInt(UIntSize::U32),
                    "u64" => Type::UInt(UIntSize::U64),
                    "usize" => Type::UInt(UIntSize::Usize),
                    "f32" => Type::Float(FloatSize::F32),
                    "f64" => Type::Float(FloatSize::F64),
                    "bool" => Type::Bool,
                    "char" => Type::Char,
                    _ => Type::Named {
                        name: name.clone(),
                        args: Vec::new(),
                    },
                }
            }
            TypeKind::Unit => Type::Unit,
            TypeKind::Ref(inner) => Type::Ref(Box::new(self.lower_type_for_ownership(inner))),
            TypeKind::MutRef(inner) => Type::MutRef(Box::new(self.lower_type_for_ownership(inner))),
            TypeKind::Weak(inner) => Type::Weak(Box::new(self.lower_type_for_ownership(inner))),
            _ => Type::Named {
                name: "unknown".to_string(),
                args: Vec::new(),
            },
        }
    }

    fn define_pattern_states(&self, pattern: &Pattern, states: &mut HashMap<String, ValueState>) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                states.insert(name.clone(), ValueState::Live);
            }
            PatternKind::Tuple(patterns) => {
                for p in patterns {
                    self.define_pattern_states(p, states);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for field in fields {
                    if let Some(ref sub) = field.pattern {
                        self.define_pattern_states(sub, states);
                    } else {
                        states.insert(field.name.clone(), ValueState::Live);
                    }
                }
            }
            PatternKind::TupleVariant { patterns, .. } => {
                for p in patterns {
                    self.define_pattern_states(p, states);
                }
            }
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
            PatternKind::AtBinding { name, pattern } => {
                states.insert(name.clone(), ValueState::Live);
                self.define_pattern_states(pattern, states);
            }
            PatternKind::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    self.define_pattern_states(first, states);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for p in prefix.iter().chain(suffix.iter()) {
                    self.define_pattern_states(p, states);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    states.insert(name.clone(), ValueState::Live);
                }
            }
        }
    }
}

// ── Closure Capture Body Usage ──────────────────────────────────

/// Per-capture body-usage classification produced by
/// `classify_capture_body_uses`. `referenced` is true if the closure body
/// reads the bare identifier or a place expression rooted at it;
/// `mutated` is true if the body mutates it (assignment-target root,
/// `mut`-marker arg root, or `mut ref self` method-call receiver root).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CaptureBodyUsage {
    referenced: bool,
    mutated: bool,
}

// ── Parameter Usage Tracking ────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParamUsage {
    Unused,
    Read,
    #[allow(dead_code)]
    Mutated,
    Consumed,
}

// ── State Merging ───────────────────────────────────────────────

/// Merge two branch states. A binding moved in either branch ends up
/// Walk every `impl` block and `trait` declaration in `program` and
/// record `Type.method → SelfParam` for each method that carries a
/// `self` parameter. Keys match `typecheck_result.method_callee_types`
/// values (e.g. `"Container.compute"`, `"Iterator.next"`). Used by
/// MethodCall handling to drive consume-vs-read classification per
/// design.md § Consume Predicate step 1.
pub(crate) fn collect_method_self_modes(program: &Program) -> HashMap<String, SelfParam> {
    let mut map = HashMap::new();
    for item in &program.items {
        match item {
            Item::ImplBlock(impl_block) => {
                let Some(target_name) = impl_target_name(&impl_block.target_type) else {
                    continue;
                };
                for impl_item in &impl_block.items {
                    if let ImplItem::Method(method) = impl_item {
                        if let Some(self_param) = &method.self_param {
                            map.insert(
                                format!("{target_name}.{}", method.name),
                                self_param.clone(),
                            );
                        }
                    }
                }
            }
            Item::TraitDef(trait_def) => {
                for trait_item in &trait_def.items {
                    if let TraitItem::Method(tm) = trait_item {
                        if let Some(self_param) = &tm.self_param {
                            map.insert(
                                format!("{}.{}", trait_def.name, tm.name),
                                self_param.clone(),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    map
}

/// Extract the canonical type-name component from an impl-block's target.
/// Mirrors typechecker's `method_callee_type_name` for `Type::Named { name }`:
/// uses the *last* segment of a path (`impl path::Foo` → `"Foo"`). Returns
/// `None` for non-Path target types — those don't currently surface a
/// `Type.method` callee key from the typechecker either.
fn impl_target_name(target_type: &TypeExpr) -> Option<String> {
    if let TypeKind::Path(path) = &target_type.kind {
        path.segments.last().cloned()
    } else {
        None
    }
}

/// Collect per-position parameter ownership modes for every free function
/// and every static (no-`self`) impl method. Used by Call-handling to
/// decide whether each argument is consumed (Owned) or read (Ref / MutRef)
/// per design.md § Consume Predicate step 2. Keys: free fn name, or
/// `"Type.method"` for static methods.
pub(crate) fn collect_callee_param_modes(program: &Program) -> HashMap<String, Vec<OwnershipMode>> {
    let mut map = HashMap::new();
    for item in &program.items {
        match item {
            Item::Function(f) => {
                map.insert(f.name.clone(), param_modes_from_signature(&f.params));
            }
            Item::ImplBlock(impl_block) => {
                let Some(target_name) = impl_target_name(&impl_block.target_type) else {
                    continue;
                };
                for impl_item in &impl_block.items {
                    if let ImplItem::Method(method) = impl_item {
                        // Static methods only — instance methods are
                        // dispatched as `MethodCall`, handled in step 1.
                        if method.self_param.is_none() {
                            map.insert(
                                format!("{target_name}.{}", method.name),
                                param_modes_from_signature(&method.params),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    map
}

/// Map each parameter's syntactic type to its declared ownership mode.
/// `ref T` → `Ref`; `mut ref T` / `mut Slice[T]` → `MutRef`; everything
/// else (bare `T`, including `T` that's a type-param, owned struct, etc.)
/// → `Own`.
fn param_modes_from_signature(params: &[Param]) -> Vec<OwnershipMode> {
    params
        .iter()
        .map(|p| match &p.ty.kind {
            TypeKind::Ref(_) => OwnershipMode::Ref,
            TypeKind::MutRef(_) | TypeKind::MutSlice(_) => OwnershipMode::MutRef,
            _ => OwnershipMode::Own,
        })
        .collect()
}

/// Return `Some(mutable)` if the formal-param type is a slice — `Slice[T]`
/// (mutable=false) or `mut Slice[T]` (mutable=true). `None` for any
/// non-slice formal. Drives Slice 1's call-arg coercion site detection:
/// when an arg expression of type `Vec[T]` / `Array[T, N]` / `Slice[T]`
/// flows into one of these slots, the implicit coercion creates a slice
/// view whose source attribution must be recorded.
fn slice_kind_from_type(ty: &TypeExpr) -> Option<bool> {
    match &ty.kind {
        TypeKind::MutSlice(_) => Some(true),
        TypeKind::Path(path) => {
            if path.segments.last().map(|s| s.as_str()) == Some("Slice") {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Result of comparing a new borrow against an existing one against the
/// same source. `None` for compatible pairs (two immutable views, etc.).
pub(crate) enum BorrowConflict {
    None,
    SliceShape(SliceConflictShape),
    CrossForm,
}

/// Slice 2 polish (b) — receiver-mode lookup for stdlib methods. The
/// user-impl `method_self_modes` map only covers methods declared in
/// user source; built-in `Vec` / `Map` / `Set` / `String` / `Slice`
/// methods are resolved by the typechecker but have no `impl` block to
/// scan. This table returns the receiver `BorrowKind` for stdlib
/// method keys (`"Vec.push"`, etc.) so cross-borrow detection still
/// fires when calling stdlib methods on a binding with a live slice.
/// Returns `None` for owned (consume) receivers and any key not in the
/// table — keeping unrecognized methods read-only-equivalent (no push)
/// is the conservative default.
pub(crate) fn stdlib_method_self_borrow_kind(key: &str) -> Option<BorrowKind> {
    use BorrowKind::*;
    let kind = match key {
        // Vec[T] mutating methods — `mut ref self` / write borrow.
        "Vec.push" | "Vec.pop" | "Vec.insert" | "Vec.remove" | "Vec.swap_remove" | "Vec.clear"
        | "Vec.truncate" | "Vec.resize" | "Vec.retain" | "Vec.extend" | "Vec.sort"
        | "Vec.sort_by" | "Vec.reverse" | "Vec.fill" | "Vec.swap" | "Vec.as_slice_mut" => MutRef,
        // Vec[T] read methods — `ref self` / read borrow.
        "Vec.len" | "Vec.is_empty" | "Vec.first" | "Vec.last" | "Vec.get" | "Vec.get_unchecked"
        | "Vec.contains" | "Vec.iter" | "Vec.binary_search" | "Vec.split_at" | "Vec.chunks"
        | "Vec.windows" | "Vec.as_slice" | "Vec.sorted" | "Vec.sorted_by" | "Vec.clone" => ImmRef,
        // Map[K, V] mutating methods.
        "Map.insert" | "Map.remove" | "Map.clear" | "Map.merge" => MutRef,
        // Map[K, V] read methods.
        "Map.len" | "Map.is_empty" | "Map.contains_key" | "Map.get" | "Map.get_or" | "Map.iter"
        | "Map.keys" | "Map.values" | "Map.entries" | "Map.clone" => ImmRef,
        // Set[T] / SortedSet[T] mutating methods.
        "Set.insert" | "Set.remove" | "Set.clear" | "SortedSet.insert" | "SortedSet.remove"
        | "SortedSet.clear" => MutRef,
        // Set[T] / SortedSet[T] read methods.
        "Set.len" | "Set.is_empty" | "Set.contains" | "Set.iter" | "Set.clone"
        | "SortedSet.len" | "SortedSet.is_empty" | "SortedSet.contains" | "SortedSet.iter"
        | "SortedSet.clone" => ImmRef,
        // String mutating methods.
        "String.push" | "String.push_str" | "String.clear" | "String.insert_str" => MutRef,
        // String read methods.
        "String.len" | "String.is_empty" | "String.contains" | "String.starts_with"
        | "String.ends_with" | "String.bytes" | "String.chars" | "String.clone"
        | "String.to_string" => ImmRef,
        // Array[T, N] / Slice[T] read methods (the snapshot fence in the
        // interpreter materializes Slice → Array, so the same method
        // names appear under both type prefixes).
        "Array.len" | "Array.is_empty" | "Array.first" | "Array.last" | "Array.get"
        | "Array.iter" | "Slice.len" | "Slice.is_empty" | "Slice.first" | "Slice.last"
        | "Slice.get" | "Slice.iter" => ImmRef,
        // Mutating methods on Slice / Array — only `mut Slice[T]` carries
        // these in v1; the receiver-side push fires regardless because
        // mut Slice has its own slice-form borrow tracking.
        "Slice.swap" => MutRef,
        _ => return None,
    };
    Some(kind)
}

/// Render a `BorrowKind` for diagnostic messages.
pub(crate) fn borrow_kind_display(kind: &BorrowKind) -> &'static str {
    match kind {
        BorrowKind::ImmRef => "ref T",
        BorrowKind::MutRef => "mut ref T",
        BorrowKind::ImmSlice => "Slice[T]",
        BorrowKind::MutSlice => "mut Slice[T]",
    }
}

/// Render the leading message for a slice-vs-slice / source-state-change
/// conflict. The caller appends the secondary borrow's span.
pub(crate) fn slice_conflict_message(shape: &SliceConflictShape, root: &str) -> String {
    match shape {
        SliceConflictShape::ImmSliceVsMutSlice => format!(
            "cannot create a `mut Slice[T]` of `{}` while another slice borrow is live",
            root
        ),
        SliceConflictShape::MutSliceVsMutSlice => format!(
            "cannot create a second `mut Slice[T]` of `{}` while one is already live",
            root
        ),
        SliceConflictShape::MoveOfBorrowed => format!(
            "cannot move `{}` while a slice borrow into it is live",
            root
        ),
        SliceConflictShape::DropOfBorrowed => format!(
            "slice into `{}` outlives its source: source dropped while borrow is still live",
            root
        ),
    }
}

/// Per-callee, per-position "is the formal a slice?" map. Same key
/// convention as `collect_callee_param_modes`. Free fns keyed by name;
/// static methods keyed by `"Type.method"`.
pub(crate) fn collect_callee_param_slice_kind(
    program: &Program,
) -> HashMap<String, Vec<Option<bool>>> {
    let mut map = HashMap::new();
    for item in &program.items {
        match item {
            Item::Function(f) => {
                map.insert(
                    f.name.clone(),
                    f.params
                        .iter()
                        .map(|p| slice_kind_from_type(&p.ty))
                        .collect(),
                );
            }
            Item::ImplBlock(impl_block) => {
                let Some(target_name) = impl_target_name(&impl_block.target_type) else {
                    continue;
                };
                for impl_item in &impl_block.items {
                    if let ImplItem::Method(method) = impl_item {
                        if method.self_param.is_none() {
                            map.insert(
                                format!("{target_name}.{}", method.name),
                                method
                                    .params
                                    .iter()
                                    .map(|p| slice_kind_from_type(&p.ty))
                                    .collect(),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    map
}

/// Merge two branch states into the parent (`target`). For move
/// tracking the merge is "any branch Moved → parent Moved" — round
/// 12.42 collapsed the former `MoveKind` distinction (Direct /
/// BranchMerged / ContainerStore) into a single state because the
/// predicate pipeline now drives every diagnostic and rc_values
/// flavor. If `target` already records a Moved (a sequential consume
/// before the branch), keep it so the consume-site span doesn't drift
/// to the branch's later span — `handle_moved_use`'s short-circuit and
/// closure-capture-mode classification both only inspect Moved
/// presence, but the `at` span is still surfaced through
/// `OwnershipError::span` indirectly via legacy paths and reported
/// span stability is desirable.
pub(crate) fn merge_states(
    target: &mut HashMap<String, ValueState>,
    branch_a: &HashMap<String, ValueState>,
    branch_b: &HashMap<String, ValueState>,
) {
    for (name, state_a) in branch_a {
        let state_b = branch_b.get(name);
        let moved_at = match (state_a, state_b) {
            (ValueState::Moved { at }, _) | (_, Some(ValueState::Moved { at })) => Some(at.clone()),
            _ => None,
        };
        let Some(at) = moved_at else { continue };
        if matches!(target.get(name), Some(ValueState::Moved { .. })) {
            continue;
        }
        target.insert(name.clone(), ValueState::Moved { at });
    }
    // DA promotion: a binding that was Uninit pre-branch becomes initialized
    // iff *both* branches assigned to it. If even one branch left it Uninit,
    // the merged state stays Uninit (next read errors).
    let to_check: Vec<String> = target
        .iter()
        .filter(|(_, s)| matches!(s, ValueState::Uninit { .. }))
        .map(|(n, _)| n.clone())
        .collect();
    for name in to_check {
        if let Some(merged) = merge_init_states(branch_a.get(&name), branch_b.get(&name)) {
            target.insert(name, merged);
        }
    }
}

/// Decide the post-branch init state for a binding that was Uninit before
/// the branch. Returns `Some(state)` only if every branch path initialized
/// it; otherwise `None` (caller should leave Uninit untouched).
///
/// Each input slot corresponds to one branch: `Live` / `InitOnce` mean that
/// branch initialized; anything else (including `Uninit`) means it didn't.
/// `Live` wins over `InitOnce` because `let mut` can only be mut on one
/// declaration, so a `Live` here would imply the binding was declared
/// `let mut`, in which case the InitOnce path can't actually arise.
fn merge_init_states(a: Option<&ValueState>, b: Option<&ValueState>) -> Option<ValueState> {
    let init_or = |s: Option<&ValueState>| -> Option<ValueState> {
        match s {
            Some(v @ ValueState::Live) | Some(v @ ValueState::InitOnce { .. }) => Some(v.clone()),
            _ => None,
        }
    };
    let (Some(a_state), Some(b_state)) = (init_or(a), init_or(b)) else {
        return None;
    };
    Some(match (&a_state, &b_state) {
        (ValueState::Live, _) | (_, ValueState::Live) => ValueState::Live,
        _ => a_state,
    })
}

/// Extract the head (outermost Named) type name from a TypeExpr, peeling
/// `ref`/`mut ref`/`weak` wrappers. Returns None if the head isn't a named type.
fn type_expr_head(te: &TypeExpr) -> Option<String> {
    match &te.kind {
        TypeKind::Path(p) => p.segments.last().cloned(),
        TypeKind::Ref(inner) | TypeKind::MutRef(inner) | TypeKind::Weak(inner) => {
            type_expr_head(inner)
        }
        _ => None,
    }
}

/// Extract the owned type name from a Type (returns None for ref/weak/primitive).
fn owned_type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Named { name, .. } => Some(name.clone()),
        // Shared structs participate in the cycle graph the same way Named
        // does — `shared struct A { b: B }` still creates an A → B edge.
        // The shared-vs-mixed cycle classification happens downstream in
        // `dfs_cycle` via `is_shared_type`.
        Type::Shared(name) => Some(name.clone()),
        // ref, mut ref, weak fields don't create ownership edges
        Type::Ref(_) | Type::MutRef(_) | Type::Weak(_) => None,
        // Rc/Arc wrappers behave like the legacy `Type::Named { name: "Rc", … }`
        // form did — the wrapper name has no entry in the user-type graph,
        // so the edge is effectively absent. (Cycle detection via the inner
        // type is out of scope for sub-item 2's behavior-preserving refactor.)
        Type::Rc(_) | Type::Arc(_) => None,
        // Primitives, tuples, arrays, etc. don't create type graph edges
        _ => None,
    }
}

/// Top-level type name (peeling refs/weak), used for `@no_rc` lookup.
fn type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Named { name, .. } => Some(name.clone()),
        Type::Shared(name) => Some(name.clone()),
        Type::Ref(inner) | Type::MutRef(inner) | Type::Weak(inner) => type_name(inner),
        _ => None,
    }
}

/// Snapshot every binding currently in the Uninit state. Returned map is
/// used by `restore_uninit_after_loop` to revert any same-iteration
/// promotions back to Uninit, preserving the "loop body might run zero
/// times" invariant for definite-assignment.
pub(crate) fn snapshot_uninit(states: &HashMap<String, ValueState>) -> HashMap<String, ValueState> {
    states
        .iter()
        .filter(|(_, s)| matches!(s, ValueState::Uninit { .. }))
        .map(|(n, s)| (n.clone(), s.clone()))
        .collect()
}

/// For each binding that was Uninit before the loop, reset it back to
/// Uninit if the loop body promoted it. Bindings that the loop body
/// transitioned to Moved are left alone — the move side of the existing
/// analysis is preserved, only DA is rolled back.
pub(crate) fn restore_uninit_after_loop(
    pre_uninit: HashMap<String, ValueState>,
    states: &mut HashMap<String, ValueState>,
) {
    for (name, original) in pre_uninit {
        match states.get(&name) {
            Some(ValueState::Uninit { .. }) | Some(ValueState::Moved { .. }) => {}
            _ => {
                states.insert(name, original);
            }
        }
    }
}

/// Apply `branch_states` to `target` for the side-of-an-if / one-arm-of-match
/// case where only one path conditionally consumed values. Round 12.42
/// collapsed the former `MoveKind::BranchMerged` retag — see `merge_states`
/// for the rationale. Any Moved in the branch propagates to the parent
/// unless the parent is already Moved (sequential consume preservation).
pub(crate) fn merge_branch_into(
    target: &mut HashMap<String, ValueState>,
    branch_states: &HashMap<String, ValueState>,
) {
    for (name, state) in branch_states {
        let ValueState::Moved { at } = state else {
            continue;
        };
        if matches!(target.get(name), Some(ValueState::Moved { .. })) {
            continue;
        }
        if !target.contains_key(name) {
            continue;
        }
        target.insert(name.clone(), ValueState::Moved { at: at.clone() });
    }
}
