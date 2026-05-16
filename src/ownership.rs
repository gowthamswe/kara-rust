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

mod closure_escape;
mod par_helpers;

use par_helpers::{collect_channel_param_types, has_attr, scan_block_for_par_uses};

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

    // ── Block / Statement / Expression Walking ──────────────────

    fn check_block(
        &mut self,
        block: &Block,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        // Slice 2 — bracket the block walk with scope-depth tracking. On
        // exit, drain any active borrows whose `scope_depth` is at or
        // beyond this block's depth.
        self.current_scope_depth += 1;
        let entered_depth = self.current_scope_depth;
        for stmt in &block.stmts {
            self.check_stmt(stmt, states, param_types, param_usage);
        }
        if let Some(ref expr) = block.final_expr {
            self.check_expr_consuming(expr, states, param_types, param_usage);
        }
        self.drain_borrows_at_depth(entered_depth);
        self.current_scope_depth = entered_depth - 1;
    }

    fn check_stmt(
        &mut self,
        stmt: &Stmt,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        match &stmt.kind {
            StmtKind::Let { pattern, value, .. } => {
                // If the RHS is a closure, detect once-callability before
                // processing so we can check which outer bindings it consumed.
                // Value is consumed by the let binding
                self.check_expr_consuming(value, states, param_types, param_usage);

                // Define bindings as Live
                self.define_pattern_states(pattern, states);

                // Record the binding's type from the RHS span. The RHS's
                // span is unaliased (unlike LHS chains), so this is the
                // reliable source of binding types for later consume sites
                // that walk through chained accesses (`c.inner.unwrap()`).
                if let Some(rhs_ty) = self
                    .typecheck_result
                    .expr_types
                    .get(&SpanKey::from_span(&value.span))
                {
                    for name in pattern.binding_names() {
                        self.binding_types.insert(name.clone(), rhs_ty.clone());
                    }
                }

                // Slice 1: escape-from-temp detection. When the RHS is a
                // direct slice creation (`.as_slice()` / `.as_slice_mut()` /
                // range-indexing) whose source has no rooted attribution
                // (the receiver is a function call result, composite
                // literal, etc.), the slice's storage is dropped at end of
                // statement — binding it to a name that outlives the
                // statement points at freed memory. In-statement uses
                // (`make_vec().as_slice().len()`) are not let-RHS so they
                // accept. Future expansions (return-of-temp-slice, escape
                // through call-arg-into-borrow) ride on Slice 2's conflict
                // detector.
                if let Some((source, _)) = Self::slice_creation_source(value) {
                    if self.place_expr_root(source).is_none() {
                        self.errors.push(OwnershipError {
                            message: "slice from temporary value escapes the enclosing statement"
                                .to_string(),
                            span: value.span.clone(),
                            kind: OwnershipErrorKind::SliceFromTemporaryEscapes,
                            suggestion: Some(
                                "bind the receiver to a local first, then take a slice into it"
                                    .to_string(),
                            ),
                            replacement: None,
                            consume_span: None,
                        });
                    }
                }

                // Slice 1: chain-through population for slice-of-slice. If
                // the RHS produced a `slice_borrow_sources` entry (any of
                // the four creation sites fired), propagate it to each
                // binding name introduced by the pattern. A later slice
                // creation whose source is the binding name walks through
                // this map in `place_expr_root` so the recorded root is
                // the original storage (`v`), not the intermediate slice.
                if let Some(entry) = self
                    .slice_borrow_sources
                    .get(&SpanKey::from_span(&value.span))
                    .cloned()
                {
                    for name in pattern.binding_names() {
                        self.slice_binding_sources
                            .insert(name.clone(), entry.clone());
                        // Slice 2 — record the scope at which this slice
                        // binding lives, keyed by the source's root. Used
                        // by drop-of-borrowed detection at the source's
                        // scope-exit drain.
                        self.slice_binding_scope_depth
                            .insert(entry.0.root.clone(), self.current_scope_depth);
                        let _ = name;
                    }
                }

                // Slice 2 — record this binding's scope depth so the
                // drop-of-borrowed trigger can detect "source going out
                // of scope while a slice into it is still bound" cases.
                for name in pattern.binding_names() {
                    self.binding_scope_depth
                        .insert(name, self.current_scope_depth);
                }
            }
            StmtKind::LetUninit {
                is_mut,
                name,
                name_span,
                ..
            } => {
                states.insert(
                    name.clone(),
                    ValueState::Uninit {
                        let_span: stmt.span.clone(),
                        is_mut: *is_mut,
                    },
                );
                // Pull the declared type from the typechecker's expr_types
                // map (recorded at the binding's name span). Lets later
                // consume sites classify Copy-vs-non-Copy without a real RHS
                // span to look up.
                if let Some(t) = self
                    .typecheck_result
                    .expr_types
                    .get(&SpanKey::from_span(name_span))
                {
                    self.binding_types.insert(name.clone(), t.clone());
                }
                // Slice 2 polish (D5) — record scope depth so the
                // shape D drain can match the LHS's "outer" scope
                // against the source's "inner" scope at exit.
                self.binding_scope_depth
                    .insert(name.clone(), self.current_scope_depth);
            }
            StmtKind::LetElse {
                pattern,
                value,
                else_block,
                ..
            } => {
                self.check_expr_consuming(value, states, param_types, param_usage);
                self.define_pattern_states(pattern, states);
                let mut else_states = states.clone();
                self.check_block(else_block, &mut else_states, param_types, param_usage);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                let mut defer_states = states.clone();
                self.check_block(body, &mut defer_states, param_types, param_usage);
            }
            StmtKind::Assign { target, value } => {
                // Check if target is a variable — reassignment resets state
                if let ExprKind::Identifier(name) = &target.kind {
                    // Process the RHS first so reads of `name` in the RHS see
                    // the pre-assignment state. (e.g. `let x: T; x = f(x);`
                    // — the `x` inside `f(x)` is still Uninit and errors.)
                    self.check_expr_consuming(value, states, param_types, param_usage);
                    let pre = states.get(name).cloned();
                    match pre {
                        // First assignment to a `let mut x: T;` — promote.
                        Some(ValueState::Uninit { is_mut: true, .. }) => {
                            states.insert(name.clone(), ValueState::Live);
                        }
                        // First assignment to a `let x: T;` (non-mut) — this
                        // counts as initialization, not reassignment, so it
                        // succeeds without `mut`. Subsequent assigns will fail.
                        Some(ValueState::Uninit { is_mut: false, .. }) => {
                            states.insert(
                                name.clone(),
                                ValueState::InitOnce {
                                    first_assign: target.span.clone(),
                                },
                            );
                        }
                        // Second-and-beyond assignment to a non-mut LetUninit
                        // binding. Per design.md "first assignment is
                        // initialization, not reassignment" — anything more
                        // requires `let mut`.
                        Some(ValueState::InitOnce { first_assign }) => {
                            self.errors.push(OwnershipError {
                                message: format!(
                                    "cannot reassign `{}` — declared without `mut` (first assignment at line {}:{})",
                                    name, first_assign.line, first_assign.column
                                ),
                                span: target.span.clone(),
                                kind: OwnershipErrorKind::ReassignToImmutable,
                                suggestion: Some(format!(
                                    "change the declaration to `let mut {}: ...;`",
                                    name
                                )),
                                replacement: None,
                                consume_span: None,
                            });
                            // Leave state as InitOnce — further reads still
                            // succeed, further reassigns still fire.
                        }
                        // Live / Moved / not-yet-bound: existing behavior —
                        // reassignment resets to Live.
                        _ => {
                            states.insert(name.clone(), ValueState::Live);
                        }
                    }
                    // Track mutation of parameters
                    if let Some(usage) = param_usage.get_mut(name) {
                        *usage = ParamUsage::Mutated;
                    }
                    // Slice 2 polish (D5) — when the LHS is a slice-typed
                    // binding (typically a `LetUninit` outer binding) and
                    // the RHS produced a slice creation, record the LHS's
                    // scope depth against the source root so block-exit
                    // drain can detect "slice outlives source" cases.
                    // The LHS's scope depth is already captured in
                    // `binding_scope_depth` from the `LetUninit` arm.
                    if let Some(entry) = self
                        .slice_borrow_sources
                        .get(&SpanKey::from_span(&value.span))
                        .cloned()
                    {
                        if let Some(&lhs_depth) = self.binding_scope_depth.get(name) {
                            self.slice_binding_sources
                                .insert(name.clone(), entry.clone());
                            self.slice_binding_scope_depth
                                .insert(entry.0.root.clone(), lhs_depth);
                        }
                    }
                } else {
                    // Field/index assignment — track mutation on the root object
                    if let Some(root) = Self::root_identifier(target) {
                        if let Some(usage) = param_usage.get_mut(&root) {
                            *usage = ParamUsage::Mutated;
                        }
                    }
                    self.check_expr_reading(target, states, param_types, param_usage);
                    self.check_expr_consuming(value, states, param_types, param_usage);
                }
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                // Compound assignment (+=, -=, etc.) mutates the target
                if let ExprKind::Identifier(name) = &target.kind {
                    if let Some(usage) = param_usage.get_mut(name) {
                        *usage = ParamUsage::Mutated;
                    }
                } else if let Some(root) = Self::root_identifier(target) {
                    if let Some(usage) = param_usage.get_mut(&root) {
                        *usage = ParamUsage::Mutated;
                    }
                }
                self.check_expr_reading(target, states, param_types, param_usage);
                self.check_expr_consuming(value, states, param_types, param_usage);
            }
            StmtKind::Expr(expr) => {
                self.check_expr_reading(expr, states, param_types, param_usage);
            }
        }
    }

    /// Extract the root identifier from a field/index access chain.
    /// e.g., `obj.field.sub` → Some("obj"), `arr[0]` → Some("arr")
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
    /// Look up the callee's parameter modes for a free-function or static-
    /// method `Call` expression. Returns `None` for callees we can't name
    /// (function-typed values, complex expressions); those fall back to
    /// the prior conservative consume-everything behavior.
    fn callee_modes_for_call(&self, callee: &Expr) -> Option<&Vec<OwnershipMode>> {
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
    fn arg_is_borrow_position(&self, callee: &Expr, arg_index: usize) -> bool {
        self.callee_modes_for_call(callee)
            .and_then(|modes| modes.get(arg_index))
            .is_some_and(|m| matches!(m, OwnershipMode::Ref | OwnershipMode::MutRef))
    }

    /// Returns `Some(mutable)` if the formal at `arg_index` of `callee` is a
    /// slice type (`Slice[T]` or `mut Slice[T]`); `None` for non-slice
    /// formals or unresolvable callees. Drives Slice 1's call-arg coercion
    /// site detection.
    fn arg_formal_slice_kind(&self, callee: &Expr, arg_index: usize) -> Option<bool> {
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
    fn arg_formal_ref_borrow_kind(&self, callee: &Expr, arg_index: usize) -> Option<BorrowKind> {
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
    fn place_expr_root(&self, expr: &Expr) -> Option<PlaceExpr> {
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
    fn record_slice_creation(&mut self, slice_span: &Span, source: &Expr, mutable: bool) {
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
    fn slice_creation_source(expr: &Expr) -> Option<(&Expr, bool)> {
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
    fn push_active_borrow(&mut self, kind: BorrowKind, source: PlaceExpr, span: Span) {
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
    fn classify_borrow_conflict(&self, existing: &BorrowKind, new: &BorrowKind) -> BorrowConflict {
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
    fn drain_borrows_at_depth(&mut self, exit_depth: usize) {
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
    fn snapshot_active_borrow_lens(&self) -> HashMap<String, usize> {
        self.active_borrows
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect()
    }

    fn restore_active_borrows_to_snapshot(&mut self, snapshot: &HashMap<String, usize>) {
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
    fn check_move_of_borrowed(&mut self, name: &str, move_span: &Span) -> bool {
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

    fn method_call_consumes_receiver(&self, method_call: &Expr) -> bool {
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
    fn method_self_borrow_kind(&self, method_call: &Expr) -> Option<BorrowKind> {
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
    fn method_call_receiver_is_mut_ref(&self, method_call: &Expr) -> bool {
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

    /// Walk `body` once and classify each pre-live capture's usage as
    /// `referenced` (any read of the bare identifier or a place expression
    /// rooted at it) and `mutated` (assignment-target root, `mut`-marker
    /// arg root, or `mut ref self` method-call receiver root). Used by the
    /// `mut ref` capture-mode unused-mut-capture perf note (Rule 2½ K2 row
    /// "mut ref + reads only").
    fn classify_capture_body_uses(
        &self,
        body: &Expr,
        pre_live: &[String],
    ) -> HashMap<String, CaptureBodyUsage> {
        let mut usage: HashMap<String, CaptureBodyUsage> = pre_live
            .iter()
            .map(|n| (n.clone(), CaptureBodyUsage::default()))
            .collect();
        self.walk_capture_body_expr(body, &mut usage);
        usage
    }

    fn walk_capture_body_expr(&self, expr: &Expr, usage: &mut HashMap<String, CaptureBodyUsage>) {
        match &expr.kind {
            ExprKind::Identifier(n) => {
                if let Some(u) = usage.get_mut(n) {
                    u.referenced = true;
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                if let Some(root) = Self::root_identifier(object) {
                    if let Some(u) = usage.get_mut(&root) {
                        u.referenced = true;
                        if self.method_call_receiver_is_mut_ref(expr) {
                            u.mutated = true;
                        }
                    }
                }
                self.walk_capture_body_expr(object, usage);
                for arg in args {
                    if arg.mut_marker {
                        if let Some(root) = Self::root_identifier(&arg.value) {
                            if let Some(u) = usage.get_mut(&root) {
                                u.mutated = true;
                            }
                        }
                    }
                    self.walk_capture_body_expr(&arg.value, usage);
                }
            }
            ExprKind::Call { callee, args } => {
                self.walk_capture_body_expr(callee, usage);
                for arg in args {
                    if arg.mut_marker {
                        if let Some(root) = Self::root_identifier(&arg.value) {
                            if let Some(u) = usage.get_mut(&root) {
                                u.mutated = true;
                            }
                        }
                    }
                    self.walk_capture_body_expr(&arg.value, usage);
                }
            }
            ExprKind::Binary { left, right, .. } => {
                self.walk_capture_body_expr(left, usage);
                self.walk_capture_body_expr(right, usage);
            }
            ExprKind::Unary { operand, .. } => {
                self.walk_capture_body_expr(operand, usage);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_capture_body_expr(object, usage);
            }
            ExprKind::Index { object, index } => {
                self.walk_capture_body_expr(object, usage);
                self.walk_capture_body_expr(index, usage);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_capture_body_expr(condition, usage);
                self.walk_capture_body_block(then_block, usage);
                if let Some(eb) = else_branch {
                    self.walk_capture_body_expr(eb, usage);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_block(then_block, usage);
                if let Some(eb) = else_branch {
                    self.walk_capture_body_expr(eb, usage);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_capture_body_expr(scrutinee, usage);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.walk_capture_body_expr(g, usage);
                    }
                    self.walk_capture_body_expr(&arm.body, usage);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_capture_body_expr(condition, usage);
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::For { iterable, body, .. } => {
                self.walk_capture_body_expr(iterable, usage);
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::Loop { body, .. } => {
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::Closure { body, .. } => {
                // Recurse into nested closure bodies — a mutation of an
                // outer capture inside a nested closure still counts as a
                // mutation from this closure's perspective.
                self.walk_capture_body_expr(body, usage);
            }
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Try(block)
            | ExprKind::Seq(block)
            | ExprKind::Par(block)
            | ExprKind::Lock { body: block, .. } => {
                self.walk_capture_body_block(block, usage);
            }
            ExprKind::Question(inner)
            | ExprKind::OptionalChain { object: inner, .. }
            | ExprKind::Cast { expr: inner, .. } => {
                self.walk_capture_body_expr(inner, usage);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.walk_capture_body_expr(left, usage);
                self.walk_capture_body_expr(right, usage);
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    self.walk_capture_body_expr(e, usage);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_capture_body_expr(e, usage);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_expr(count, usage);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.walk_capture_body_expr(k, usage);
                    self.walk_capture_body_expr(v, usage);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    self.walk_capture_body_expr(&field.value, usage);
                }
                if let Some(s) = spread {
                    self.walk_capture_body_expr(s, usage);
                }
            }
            ExprKind::Pipe { left, right } => {
                self.walk_capture_body_expr(left, usage);
                self.walk_capture_body_expr(right, usage);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_capture_body_expr(s, usage);
                }
                if let Some(e) = end {
                    self.walk_capture_body_expr(e, usage);
                }
            }
            ExprKind::Return(Some(inner))
            | ExprKind::Break {
                value: Some(inner), ..
            } => {
                self.walk_capture_body_expr(inner, usage);
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.walk_capture_body_expr(&b.value, usage);
                }
                self.walk_capture_body_block(body, usage);
            }
            // Leaves and other forms have no captures of interest.
            _ => {}
        }
    }

    fn walk_capture_body_block(
        &self,
        block: &Block,
        usage: &mut HashMap<String, CaptureBodyUsage>,
    ) {
        for stmt in &block.stmts {
            self.walk_capture_body_stmt(stmt, usage);
        }
        if let Some(expr) = &block.final_expr {
            self.walk_capture_body_expr(expr, usage);
        }
    }

    fn walk_capture_body_stmt(&self, stmt: &Stmt, usage: &mut HashMap<String, CaptureBodyUsage>) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => {
                self.walk_capture_body_expr(value, usage);
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_block(else_block, usage);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.walk_capture_body_block(body, usage);
            }
            StmtKind::Assign { target, value } => {
                if let Some(root) = Self::root_identifier(target) {
                    if let Some(u) = usage.get_mut(&root) {
                        u.mutated = true;
                    }
                }
                self.walk_capture_body_expr(target, usage);
                self.walk_capture_body_expr(value, usage);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                if let Some(root) = Self::root_identifier(target) {
                    if let Some(u) = usage.get_mut(&root) {
                        u.mutated = true;
                    }
                }
                self.walk_capture_body_expr(target, usage);
                self.walk_capture_body_expr(value, usage);
            }
            StmtKind::Expr(e) => self.walk_capture_body_expr(e, usage),
        }
    }

    /// Mark a named binding as consumed at `use_span`. Used by the
    /// MethodCall receiver-consume path (step 1) so the consume does
    /// not depend on `expr_types[span]`, which is unreliable at the
    /// root of a chained access (`c.inner.unwrap()` aliases all spans
    /// to `c`'s span and the typechecker's last-write-wins puts the
    /// method's return type there). Reads the binding's actual type
    /// from `param_types` (params) or `binding_types` (locals); both
    /// are keyed by name and so are immune to the span aliasing.
    fn consume_named_binding(
        &mut self,
        name: &str,
        use_span: &Span,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        if self.handle_uninit_read(name, use_span, states) {
            return;
        }
        if self.handle_moved_use(name, use_span, states) {
            return;
        }
        let is_copy = if let Some(t) = param_types.get(name) {
            self.is_copy_type(t)
        } else if let Some(t) = self.binding_types.get(name) {
            self.is_copy_type(t)
        } else {
            // Unknown — conservative default: assume non-Copy so the
            // consume actually fires. False-positive Copy classification
            // here (a "consume" of a Copy local that the table missed)
            // would silently miss real moves; default to non-Copy is the
            // safer error mode.
            false
        };
        if !is_copy {
            states.insert(
                name.to_string(),
                ValueState::Moved {
                    at: use_span.clone(),
                },
            );
            if let Some(usage) = param_usage.get_mut(name) {
                *usage = ParamUsage::Consumed;
            }
        } else if let Some(usage) = param_usage.get_mut(name) {
            if *usage == ParamUsage::Unused {
                *usage = ParamUsage::Read;
            }
        }
    }

    /// Whether `name` is a unit-variant of any known enum. The parser cannot
    /// distinguish `None` (variant ref) from `let None = ...` (fresh binding),
    /// so both reach ownership as `PatternKind::Binding(name)`. The typechecker
    /// disambiguates per-arm against the scrutinee's type; for pattern-binding
    /// classification in match scrutinee analysis we use a coarser global
    /// check — matching any unit variant by name. Over-permissive only in the
    /// pathological case of a real binding shadowing a known variant name,
    /// which is non-idiomatic.
    fn is_unit_variant_name(&self, name: &str) -> bool {
        self.typecheck_result
            .enum_info
            .values()
            .any(|info| info.variants.iter().any(|(vn, _)| vn == name))
    }

    /// Whether the pattern binds at least one fresh value-name. Wildcards,
    /// literal patterns, range patterns, and pure unit-variant references
    /// don't bind. Used by step 4 of the consume predicate (match scrutinee
    /// classification): if any arm pattern binds anything, the scrutinee
    /// is consumed (subject to Copy).
    fn pattern_binds_anything(&self, pattern: &Pattern) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {
                false
            }
            PatternKind::Binding(name) => !self.is_unit_variant_name(name),
            PatternKind::AtBinding { .. } => true,
            PatternKind::Tuple(patterns) | PatternKind::TupleVariant { patterns, .. } => {
                patterns.iter().any(|p| self.pattern_binds_anything(p))
            }
            PatternKind::Struct { fields, .. } => fields.iter().any(|f| match &f.pattern {
                Some(sub) => self.pattern_binds_anything(sub),
                // Shorthand `Container { name }` binds `name`.
                None => true,
            }),
            PatternKind::Or(alts) => alts.iter().any(|p| self.pattern_binds_anything(p)),
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                matches!(rest, Some(RestPattern::Bound(_)))
                    || prefix
                        .iter()
                        .chain(suffix.iter())
                        .any(|p| self.pattern_binds_anything(p))
            }
        }
    }

    /// Whether `scrutinee`'s type is `ref T` / `mut ref T` — match
    /// ergonomics treat such scrutinees as always-read regardless of
    /// what the arms bind, since the typechecker has wrapped each arm
    /// binding's type in the corresponding borrow form (design.md
    /// § Match Arm Binding Modes). Looks up the scrutinee's type via
    /// the per-span `expr_types` table the typechecker populates;
    /// falls back to `param_types` when the scrutinee is a bare
    /// parameter identifier (the span table can lag in synthesised
    /// expressions, but param_types is authoritative for parameters).
    fn is_borrow_typed_scrutinee(
        &self,
        scrutinee: &Expr,
        param_types: &HashMap<String, Type>,
    ) -> bool {
        let ty = self
            .typecheck_result
            .expr_types
            .get(&SpanKey::from_span(&scrutinee.span))
            .cloned()
            .or_else(|| match &scrutinee.kind {
                ExprKind::Identifier(name) => param_types.get(name).cloned(),
                _ => None,
            });
        matches!(ty, Some(Type::Ref(_)) | Some(Type::MutRef(_)))
    }

    fn root_identifier(expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(name) => Some(name.clone()),
            ExprKind::FieldAccess { object, .. }
            | ExprKind::TupleIndex { object, .. }
            | ExprKind::Index { object, .. } => Self::root_identifier(object),
            // `*r` — the root being mutated is the reference variable `r` itself.
            ExprKind::Unary {
                op: crate::ast::UnaryOp::Deref,
                operand,
            } => Self::root_identifier(operand),
            _ => None,
        }
    }

    /// Check an expression in a "consuming" context (e.g., passed to a function,
    /// returned, assigned to a variable). Non-Copy values are moved.
    fn check_expr_consuming(
        &mut self,
        expr: &Expr,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                if self.handle_uninit_read(name, &expr.span, states) {
                    return;
                }
                let is_copy = if let Some(t) = param_types.get(name) {
                    self.is_copy_type(t)
                } else {
                    // Local binding not in param_types — consult typecheck result
                    self.typecheck_result
                        .expr_types
                        .get(&SpanKey::from_span(&expr.span))
                        .map(|t| self.is_copy_type(t))
                        .unwrap_or(false)
                };

                if self.handle_moved_use(name, &expr.span, states) {
                    return;
                }

                if !is_copy {
                    // Slice 2 — before the move proceeds, check whether
                    // any slice borrow into this binding is live. If so,
                    // emit shape C (move-of-borrowed). The move itself
                    // still proceeds so downstream state stays consistent.
                    self.check_move_of_borrowed(name, &expr.span);
                    // Non-copy value is consumed → mark as moved.
                    states.insert(
                        name.clone(),
                        ValueState::Moved {
                            at: expr.span.clone(),
                        },
                    );
                    if let Some(usage) = param_usage.get_mut(name) {
                        *usage = ParamUsage::Consumed;
                    }
                } else if let Some(usage) = param_usage.get_mut(name) {
                    if *usage == ParamUsage::Unused {
                        *usage = ParamUsage::Read;
                    }
                }
            }
            ExprKind::Call { callee, args } => {
                // Slice 2 — call-statement-scoped slice borrows; see
                // `check_expr_reading`'s Call arm for rationale.
                let snapshot = self.snapshot_active_borrow_lens();
                self.check_call_callee(callee, states, param_types, param_usage);
                for (i, arg) in args.iter().enumerate() {
                    // Step 2 (consume-predicate): the arg's classification
                    // is driven by the callee's declared parameter mode.
                    // `ref T` / `mut ref T` / `mut Slice[T]` slots are
                    // borrow positions — read, not consume — regardless of
                    // whether the call-site `mut <expr>` marker is present
                    // (the marker is required by Part 1½ for `MutRef` slots
                    // but is itself a borrow signal, not a move signal).
                    // Bare-T slots consume per the existing rule. Unknown
                    // callees (function-typed values, etc.) fall back to
                    // the prior consume-on-no-marker default.
                    let is_borrow = arg.mut_marker || self.arg_is_borrow_position(callee, i);
                    if is_borrow {
                        self.check_expr_reading(&arg.value, states, param_types, param_usage);
                    } else {
                        self.check_expr_consuming(&arg.value, states, param_types, param_usage);
                    }
                    // Slice 1: site (iii) call-arg coercion — see
                    // `check_expr_reading`'s Call arm for rationale.
                    if let Some(formal_mutable) = self.arg_formal_slice_kind(callee, i) {
                        self.record_slice_creation(&arg.value.span, &arg.value, formal_mutable);
                    } else if let Some(borrow_kind) = self.arg_formal_ref_borrow_kind(callee, i) {
                        // Slice 2 follow-up — non-slice ref formal cross-
                        // borrow push; see `check_expr_reading`'s Call arm
                        // for rationale.
                        if let Some(place) = self.place_expr_root(&arg.value) {
                            self.push_active_borrow(borrow_kind, place, arg.value.span.clone());
                        }
                    }
                }
                self.restore_active_borrows_to_snapshot(&snapshot);
            }
            ExprKind::Return(Some(inner)) => {
                self.check_expr_consuming(inner, states, param_types, param_usage);
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    self.check_expr_consuming(&field.value, states, param_types, param_usage);
                }
                if let Some(ref s) = spread {
                    self.check_expr_consuming(s, states, param_types, param_usage);
                }
            }
            // Partial move through field projection (design.md § Consume
            // Predicate step 3). Consume of `v.field` / `v.0` / `v.a.b` is a
            // consume of the root binding `v`. Walk the projection chain by
            // recursing on `object` until the base `Identifier` fires the
            // standard consume logic. Copy fields short-circuit through the
            // reading path so the root is not falsely moved.
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                let field_is_copy = self
                    .typecheck_result
                    .expr_types
                    .get(&SpanKey::from_span(&expr.span))
                    .map(|t| self.is_copy_type(t))
                    .unwrap_or(false);
                if field_is_copy {
                    self.check_expr_reading(expr, states, param_types, param_usage);
                } else {
                    self.check_expr_consuming(object, states, param_types, param_usage);
                }
            }
            // For compound expressions, delegate to reading (they don't consume at top level)
            _ => self.check_expr_reading(expr, states, param_types, param_usage),
        }
    }

    /// If `name`'s state is Uninit at this read, push a UseOfUninitialized
    /// error and return `true` (caller should bail out — no point trying to
    /// classify the read further). Definite-assignment failure.
    ///
    /// When the binding's declared type is `Array[T, N]` the message and
    /// suggestion are array-specific: per design.md §1097 the v1 DA analyser
    /// tracks whole-value assignment only — per-slot fills like `arr[0] = ...`
    /// do not satisfy DA — so the suggestion points users at the canonical
    /// fully-initialized constructors (`Array[v; N]` literal, `Array.from_fn`).
    fn handle_uninit_read(
        &mut self,
        name: &str,
        use_span: &Span,
        states: &HashMap<String, ValueState>,
    ) -> bool {
        let Some(ValueState::Uninit { let_span, .. }) = states.get(name) else {
            return false;
        };
        let is_array = matches!(self.binding_types.get(name), Some(Type::Array { .. }));
        let (message, suggestion) = if is_array {
            (
                format!(
                    "read of uninitialized array `{}` (declared at line {}:{})",
                    name, let_span.line, let_span.column
                ),
                format!(
                    "assign the whole value first — try `{} = Array[v; N]` or `{} = Array.from_fn(N, |i| ...)`",
                    name, name
                ),
            )
        } else {
            (
                format!(
                    "use of uninitialized binding `{}` (declared at line {}:{})",
                    name, let_span.line, let_span.column
                ),
                format!("assign to `{}` before reading it", name),
            )
        };
        self.errors.push(OwnershipError {
            message,
            span: use_span.clone(),
            kind: OwnershipErrorKind::UseOfUninitialized,
            suggestion: Some(suggestion),
            replacement: None,
            consume_span: None,
        });
        true
    }

    /// Examine `states[name]`. Returns `true` when the binding is in
    /// `Moved` state (so the caller should bail out of further
    /// processing of this expression). All UAM and RC fallback
    /// diagnostic emission is driven by the predicate pre-pass in
    /// `populate_predicate_outputs` — round 12.17 collapsed the RC
    /// kinds; round 12.21 collapsed the `Direct` UAM kind; round
    /// 12.42 collapsed `MoveKind` into the binary
    /// `ValueState::Moved`. The legacy state machine's only remaining
    /// jobs are this short-circuit (so descendant expressions inside
    /// an already-moved identifier don't emit cascading reads) and
    /// closure-capture mode classification in `check_expr_consuming`'s
    /// `Closure` arm.
    #[allow(clippy::unused_self)]
    fn handle_moved_use(
        &mut self,
        name: &str,
        _use_span: &Span,
        states: &HashMap<String, ValueState>,
    ) -> bool {
        matches!(states.get(name), Some(ValueState::Moved { .. }))
    }

    /// Handle the callee position of a `Call` expression.
    ///
    /// For once-callable closure bindings (those whose body consumed
    /// at least one captured owned non-Copy value), calling the
    /// closure is itself a consuming operation. Per round 12.38, the
    /// once-callable state-machine bookkeeping moved to the predicate
    /// pipeline: `UseClassifier` (round 12.20) tags every call site
    /// of a once-callable binding as `UseKind::Consume`, the predicate
    /// pairs the first/second call as a UAM witness (or as an RC
    /// witness when the calls are dominance-incomparable), and
    /// `populate_predicate_outputs` emits the diagnostic. The legacy
    /// state-machine still walks the body for parent-state propagation
    /// and the K2 closure-capture retag, but it no longer mutates
    /// `states` for the closure binding itself on call — the predicate
    /// owns that. The callee is walked through the regular reading
    /// path so any nested non-callee subexpressions (turbofish,
    /// receiver projections) still record their use sites for inference.
    fn check_call_callee(
        &mut self,
        callee: &Expr,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        // Normal callee: just read it (functions are not consumed by being called).
        self.check_expr_reading(callee, states, param_types, param_usage);
    }

    /// Check an expression in a "reading" context. Values are not moved.
    fn check_expr_reading(
        &mut self,
        expr: &Expr,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                if self.handle_uninit_read(name, &expr.span, states) {
                    return;
                }
                if self.handle_moved_use(name, &expr.span, states) {
                    return;
                }
                // Track as read for param mode inference
                if let Some(usage) = param_usage.get_mut(name) {
                    if *usage == ParamUsage::Unused {
                        *usage = ParamUsage::Read;
                    }
                }
            }
            ExprKind::SelfValue => {
                self.handle_moved_use("self", &expr.span, states);
            }
            ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
                self.check_expr_reading(left, states, param_types, param_usage);
                self.check_expr_reading(right, states, param_types, param_usage);
            }
            ExprKind::Unary { operand, .. } => {
                self.check_expr_reading(operand, states, param_types, param_usage);
            }
            ExprKind::Call { callee, args } => {
                // Slice 2 — snapshot before arg walking so call-arg-
                // coerced slice borrows AND ref-formal transient borrows
                // are call-statement-scoped (drop at call return).
                // Conflicts mid-call still fire because the push side-
                // effects the diagnostic.
                let snapshot = self.snapshot_active_borrow_lens();
                self.check_call_callee(callee, states, param_types, param_usage);
                for (i, arg) in args.iter().enumerate() {
                    // Step 2 (consume-predicate): see the analogous arm in
                    // `check_expr_consuming` for the rationale.
                    let is_borrow = arg.mut_marker || self.arg_is_borrow_position(callee, i);
                    if is_borrow {
                        self.check_expr_reading(&arg.value, states, param_types, param_usage);
                    } else {
                        self.check_expr_consuming(&arg.value, states, param_types, param_usage);
                    }
                    // Slice 1: site (iii) call-arg coercion. When the
                    // formal slot is `Slice[T]` / `mut Slice[T]` and the
                    // arg flows in as a `Vec` / `Array` / `Slice`, the
                    // typechecker inserts an implicit slice view. Record
                    // the source attribution against the arg's span.
                    if let Some(formal_mutable) = self.arg_formal_slice_kind(callee, i) {
                        self.record_slice_creation(&arg.value.span, &arg.value, formal_mutable);
                    } else if let Some(borrow_kind) = self.arg_formal_ref_borrow_kind(callee, i) {
                        // Slice 2 follow-up — non-slice `ref T` / `mut ref T`
                        // formal at this position. Push a transient borrow
                        // on the arg's root so cross-borrow detection fires
                        // against any live slice on the same source. The
                        // borrow drops at the call's snapshot-restore.
                        if let Some(place) = self.place_expr_root(&arg.value) {
                            self.push_active_borrow(borrow_kind, place, arg.value.span.clone());
                        }
                    }
                }
                self.restore_active_borrows_to_snapshot(&snapshot);
            }
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                // Step 1 (consume-predicate): receiver mode comes from the
                // resolved method's `self_param`, not from a name heuristic.
                // `bare self` → consume the receiver; `ref self` /
                // `mut ref self` → read. Falls back to read when the method
                // can't be resolved (e.g. typecheck error upstream).
                //
                // For a projection receiver like `c.inner.unwrap()`, walking
                // to the root identifier and consuming *that* is necessary
                // because the parser aliases `MethodCall.span == receiver
                // .span`, so the round-11.2 `expr_types`-driven Copy check
                // on the FieldAccess receiver would see the method's return
                // type instead of the field's type. Going via the root
                // identifier sidesteps the alias entirely.
                if self.method_call_consumes_receiver(expr) {
                    if let Some(root_name) = Self::root_identifier(object) {
                        self.consume_named_binding(
                            &root_name,
                            &object.span,
                            states,
                            param_types,
                            param_usage,
                        );
                    } else {
                        self.check_expr_consuming(object, states, param_types, param_usage);
                    }
                } else {
                    self.check_expr_reading(object, states, param_types, param_usage);
                }
                // Slice 1: `.as_slice()` / `.as_slice_mut()` are slice
                // creation site (i). Record the source attribution so
                // Slice 2's conflict detector can match later uses against
                // the original storage binding. No-op when the receiver
                // is a temporary (function call result, etc.). Recorded
                // BEFORE the receiver-side push snapshot so the slice
                // borrow persists past the method call.
                if (method == "as_slice" || method == "as_slice_mut") && args.is_empty() {
                    self.record_slice_creation(&expr.span, object, method == "as_slice_mut");
                }
                // Slice 2 — receiver-side borrow push for instance methods.
                // The push fires the conflict matrix against any existing
                // borrows on the receiver's root, surfacing CrossBorrowConflict
                // when a slice into the receiver is already live (e.g.,
                // `let _s = h.v.as_slice_mut(); h.modify();`). Skipped for
                // `.as_slice` / `.as_slice_mut` (the slice creation push
                // above is the correct representation; a redundant
                // receiver-side ref push would false-positive on the
                // method's own slice result). Snapshot taken AFTER the
                // slice creation push so persistent slice borrows survive
                // the restore at end-of-call. Static methods, bare-self
                // consumes, and unresolved methods (stdlib impls etc.) all
                // return None from `method_self_borrow_kind` and skip.
                let receiver_snapshot = self.snapshot_active_borrow_lens();
                if !matches!(method.as_str(), "as_slice" | "as_slice_mut") {
                    if let Some(borrow_kind) = self.method_self_borrow_kind(expr) {
                        if let Some(place) = self.place_expr_root(object) {
                            self.push_active_borrow(borrow_kind, place, expr.span.clone());
                        }
                    }
                }
                // Trigger 3 (container store + subsequent use) was
                // formerly routed by snapshotting Live arg-rooted
                // bindings, walking the args, and retagging any that
                // flipped to `MoveKind::Direct` as `ContainerStore` so
                // a later sequential use landed in RC fallback. Round
                // 12.42 removed the retag — the predicate pipeline's
                // `use_classifier` already tags each owned (no
                // `mut`-marker) arg of a `mut ref self` method call as
                // `ConsumeOrigin::ContainerStore` (round 12.12), and
                // `populate_predicate_outputs` emits the flavor-correct
                // `RcEntry` directly. The call-arg consume walk below
                // is now the only ownership-side action.
                for arg in args {
                    if arg.mut_marker {
                        self.check_expr_reading(&arg.value, states, param_types, param_usage);
                    } else {
                        self.check_expr_consuming(&arg.value, states, param_types, param_usage);
                    }
                }
                // Slice 2 — drop the receiver-side ref borrow + any call-
                // arg-coerced slice borrows added during the args walk.
                // The `.as_slice` / `.as_slice_mut` slice creation push
                // happens BEFORE the snapshot so it's preserved.
                self.restore_active_borrows_to_snapshot(&receiver_snapshot);
            }
            ExprKind::FieldAccess { object, .. } => {
                self.check_expr_reading(object, states, param_types, param_usage);
            }
            ExprKind::TupleIndex { object, .. } => {
                self.check_expr_reading(object, states, param_types, param_usage);
            }
            ExprKind::Index { object, index } => {
                self.check_expr_reading(object, states, param_types, param_usage);
                self.check_expr_reading(index, states, param_types, param_usage);
                // Slice 1: range-indexing produces a slice view (site (ii)).
                // The typechecker types `v[a..b]` as `Slice[T]` (immutable
                // — see typechecker.rs:7244-7282); record the source
                // attribution against the index expression's span.
                if matches!(&index.kind, ExprKind::Range { .. }) {
                    self.record_slice_creation(&expr.span, object, false);
                }
            }
            ExprKind::Block(block) => {
                self.check_block(block, states, param_types, param_usage);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.check_expr_reading(condition, states, param_types, param_usage);
                // Clone states for branches — conservative: if moved in either branch,
                // consider moved after the if
                let mut then_states = states.clone();
                self.check_block(then_block, &mut then_states, param_types, param_usage);
                if let Some(ref else_expr) = else_branch {
                    let mut else_states = states.clone();
                    self.check_expr_reading(else_expr, &mut else_states, param_types, param_usage);
                    // Merge: if moved in EITHER branch, it's moved
                    merge_states(states, &then_states, &else_states);
                } else {
                    // Only then branch ran — promote any conditional move
                    // to BranchMerged so the next use lands in RC fallback
                    // rather than firing a use-after-move error.
                    merge_branch_into(states, &then_states);
                }
            }
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.check_expr_reading(value, states, param_types, param_usage);
                let mut then_states = states.clone();
                self.define_pattern_states(pattern, &mut then_states);
                self.check_block(then_block, &mut then_states, param_types, param_usage);
                if let Some(ref else_expr) = else_branch {
                    let mut else_states = states.clone();
                    self.check_expr_reading(else_expr, &mut else_states, param_types, param_usage);
                    merge_states(states, &then_states, &else_states);
                } else {
                    merge_branch_into(states, &then_states);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                // Step 4 (consume-predicate): classify the scrutinee as
                // consume iff *any* arm pattern binds at least one name
                // by-move. All Kāra pattern bindings are by-move, so a
                // pattern that binds anything pulls part of the scrutinee
                // out. Wildcard / literal / range / pure unit-variant
                // arms read only. `pattern_binds_anything` filters unit
                // variants like `None` (parsed as `Binding("None")`) so
                // an all-`Some(_) | None`-style match doesn't false-
                // positive consume.
                //
                // Match-ergonomics exception (design.md § Match Arm
                // Binding Modes): when the scrutinee's type is
                // `ref T` / `mut ref T`, bindings borrow rather than
                // move, so the scrutinee is always read — never
                // consumed — regardless of what the arms bind. The
                // typechecker has already wrapped each arm binding's
                // type in the corresponding borrow form (see
                // `ScrutineeMode::wrap_binding_ty` in
                // `typechecker.rs`); we mirror that decision here so
                // the scrutinee's source binding (typically a `ref T`
                // parameter) doesn't get falsely marked `Moved`.
                let scrut_is_borrow = self.is_borrow_typed_scrutinee(scrutinee, param_types);
                let any_arm_binds = arms
                    .iter()
                    .any(|arm| self.pattern_binds_anything(&arm.pattern));
                if any_arm_binds && !scrut_is_borrow {
                    self.check_expr_consuming(scrutinee, states, param_types, param_usage);
                } else {
                    self.check_expr_reading(scrutinee, states, param_types, param_usage);
                }
                let mut all_arm_states: Vec<HashMap<String, ValueState>> = Vec::new();
                for arm in arms {
                    let mut arm_states = states.clone();
                    self.define_pattern_states(&arm.pattern, &mut arm_states);
                    if let Some(guard) = &arm.guard {
                        self.check_expr_reading(guard, &mut arm_states, param_types, param_usage);
                    }
                    self.check_expr_reading(&arm.body, &mut arm_states, param_types, param_usage);
                    all_arm_states.push(arm_states);
                }
                // Merge all arm states — moved in any arm → BranchMerged.
                for arm_states in &all_arm_states {
                    merge_branch_into(states, arm_states);
                }
                // DA promotion across an exhaustive match: if every arm
                // initialized a previously-Uninit binding, the join is
                // initialized. Match exhaustiveness is enforced by the
                // typechecker, so all reachable arms run at least one path.
                let to_check: Vec<String> = states
                    .iter()
                    .filter(|(_, s)| matches!(s, ValueState::Uninit { .. }))
                    .map(|(n, _)| n.clone())
                    .collect();
                for name in to_check {
                    if all_arm_states.is_empty() {
                        break;
                    }
                    let mut merged: Option<ValueState> = None;
                    let mut all_init = true;
                    for arm_states in &all_arm_states {
                        match arm_states.get(&name) {
                            Some(v @ ValueState::Live) | Some(v @ ValueState::InitOnce { .. }) => {
                                merged = Some(match (&merged, v) {
                                    (Some(ValueState::Live), _) | (_, ValueState::Live) => {
                                        ValueState::Live
                                    }
                                    _ => v.clone(),
                                });
                            }
                            _ => {
                                all_init = false;
                                break;
                            }
                        }
                    }
                    if all_init {
                        if let Some(state) = merged {
                            states.insert(name, state);
                        }
                    }
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.check_expr_reading(condition, states, param_types, param_usage);
                let pre_uninit = snapshot_uninit(states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::WhileLet {
                value,
                pattern,
                body,
                ..
            } => {
                self.check_expr_reading(value, states, param_types, param_usage);
                let pre_uninit = snapshot_uninit(states);
                self.define_pattern_states(pattern, states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                self.check_expr_reading(iterable, states, param_types, param_usage);
                let pre_uninit = snapshot_uninit(states);
                self.define_pattern_states(pattern, states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::Loop { body, .. } => {
                let pre_uninit = snapshot_uninit(states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::LabeledBlock { body, .. } => {
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Unsafe(body)
            | ExprKind::Try(body)
            | ExprKind::Seq(body)
            | ExprKind::Par(body) => {
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Lock { body, .. } => {
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Closure {
                params: closure_params,
                body,
                capture_mode,
                prefix_span,
            } => {
                // Snapshot live bindings so we can identify which captures
                // the body consumed and retag them as ClosureCapture moves.
                // This is what routes "consume inside closure body + outer
                // use" to RC trigger 2 instead of a use-after-move error.
                let pre_live: Vec<String> = states
                    .iter()
                    .filter(|(_, s)| matches!(s, ValueState::Live))
                    .map(|(n, _)| n.clone())
                    .collect();

                // Round 12.23 — Closure ownership Step 1: bind closure
                // parameters into `states` / `param_usage` for the
                // duration of the body walk so the same use-predicate
                // scan that infers fn-param modes classifies closure
                // params too. Snapshot any pre-existing entries with
                // the same name so shadowing of an outer binding is
                // reversible at the end of the walk. Build a fresh
                // `param_types` map for the body walk so the
                // copy-vs-non-copy gate at `check_expr_consuming` reads
                // the closure-local parameter type, not a shadowed
                // outer-scope type.
                let closure_param_names: Vec<String> = closure_params
                    .iter()
                    .flat_map(|cp| cp.pattern.binding_names())
                    .collect();
                let mut prev_states: Vec<(String, Option<ValueState>)> = Vec::new();
                let mut prev_usage: Vec<(String, Option<ParamUsage>)> = Vec::new();
                for name in &closure_param_names {
                    prev_states.push((name.clone(), states.remove(name)));
                    prev_usage.push((name.clone(), param_usage.remove(name)));
                    states.insert(name.clone(), ValueState::Live);
                    param_usage.insert(name.clone(), ParamUsage::Unused);
                }
                let mut closure_param_types: HashMap<String, Type> = param_types.clone();
                let closure_fn_type = self
                    .typecheck_result
                    .expr_types
                    .get(&SpanKey::from_span(&expr.span))
                    .cloned();
                let inferred_param_types: Vec<Option<Type>> = match &closure_fn_type {
                    Some(Type::Function { params, .. })
                    | Some(Type::OnceFunction { params, .. }) => {
                        params.iter().cloned().map(Some).collect()
                    }
                    _ => vec![None; closure_params.len()],
                };
                for (i, cp) in closure_params.iter().enumerate() {
                    let ty = if let Some(annot) = &cp.ty {
                        self.lower_type_for_ownership(annot)
                    } else if let Some(Some(t)) = inferred_param_types.get(i) {
                        t.clone()
                    } else {
                        Type::Error
                    };
                    for name in cp.pattern.binding_names() {
                        closure_param_types.insert(name, ty.clone());
                    }
                }

                self.check_expr_reading(body, states, &closure_param_types, param_usage);

                // Harvest closure-param mode classifications. Each
                // `param_usage` entry was zeroed before the walk, so
                // its post-walk state reflects only the closure body's
                // contribution. Map to `OwnershipMode` with the same
                // rule used for fn-param inference at `check_function`.
                let mut closure_modes: Vec<(String, OwnershipMode)> = Vec::new();
                for cp in closure_params {
                    for name in cp.pattern.binding_names() {
                        let usage = param_usage
                            .get(&name)
                            .cloned()
                            .unwrap_or(ParamUsage::Unused);
                        let mode = match usage {
                            ParamUsage::Unused | ParamUsage::Read => OwnershipMode::Ref,
                            ParamUsage::Mutated => OwnershipMode::MutRef,
                            ParamUsage::Consumed => OwnershipMode::Own,
                        };
                        closure_modes.push((name, mode));
                    }
                }
                let closure_key = SpanKey::from_span(&expr.span);
                self.closure_param_modes.insert(closure_key, closure_modes);
                // Round 12.25: record the enclosing function so
                // `karac query ownership <fn>` can filter to
                // closures created inside that function. Also stash
                // the full span so consumers can render line/column.
                self.closure_function
                    .insert(closure_key, self.current_function.clone());
                self.closure_spans.insert(closure_key, expr.span.clone());

                // Restore the outer scope: drop closure-param entries
                // that didn't pre-exist and reinstate any shadowed
                // outer bindings.
                for (name, prev) in prev_states {
                    match prev {
                        Some(s) => {
                            states.insert(name, s);
                        }
                        None => {
                            states.remove(&name);
                        }
                    }
                }
                for (name, prev) in prev_usage {
                    match prev {
                        Some(u) => {
                            param_usage.insert(name, u);
                        }
                        None => {
                            param_usage.remove(&name);
                        }
                    }
                }

                // Round 12.24 — Closure ownership Step 2: identify
                // captures. A capture is an outer-scope binding that
                // the closure body references. Names lexically
                // shadowed by the closure's own parameter list are
                // excluded — body references to those names are to
                // the closure-local, not the outer binding. Detection
                // runs after the outer-scope restore so `states[N]`
                // for non-shadowed names reflects the body walk's
                // effect (consumed → `Moved`); shadowed names'
                // outer-scope state was restored to its pre-walk
                // value, which is what we want (body did not consume
                // the outer binding, the closure-local has gone out
                // of scope). Read/mutate signals come from
                // `classify_capture_body_uses`'s AST walk; consume
                // signals come from `states[N] == Moved`. Detection
                // happens before the K2 retag loop so the legacy
                // retag behavior (Direct → ClosureCapture state
                // transition for non-K2-error captures) does not
                // confuse the consume check — the kind variant
                // doesn't matter, only that the state is `Moved`.
                let captures_usage = self.classify_capture_body_uses(body, &pre_live);
                let closure_param_set: HashSet<String> =
                    closure_param_names.iter().cloned().collect();
                let mut captures: Vec<(String, OwnershipMode)> = Vec::new();
                for name in &pre_live {
                    if closure_param_set.contains(name) {
                        continue;
                    }
                    let consumed = matches!(states.get(name), Some(ValueState::Moved { .. }));
                    let body_usage = captures_usage.get(name).copied().unwrap_or_default();
                    if !body_usage.referenced && !consumed {
                        continue;
                    }
                    let mode = if consumed {
                        OwnershipMode::Own
                    } else if body_usage.mutated {
                        OwnershipMode::MutRef
                    } else {
                        OwnershipMode::Ref
                    };
                    captures.push((name.clone(), mode));
                }
                captures.sort_by(|a, b| a.0.cmp(&b.0));
                self.closure_captures
                    .insert(SpanKey::from_span(&expr.span), captures);

                // K2 conflict-table row "mut ref + reads only" (Rule 2½):
                // if the closure declared `mut ref` but the body never
                // mutates a referenced capture, emit a Tier 2 perf note.
                // Done before the consume-pass below so a body that *also*
                // consumes a different capture (which fires the K2 error
                // path) still emits the unused-mut note for any read-only
                // siblings.
                if matches!(capture_mode, Some(CaptureMode::MutRef)) {
                    let usage = self.classify_capture_body_uses(body, &pre_live);
                    for name in &pre_live {
                        let u = match usage.get(name) {
                            Some(u) => u,
                            None => continue,
                        };
                        if u.referenced && !u.mutated {
                            // The parser stored the prefix span on the
                            // Closure expression — when present, attach a
                            // machine-applicable rewrite that swaps `mut ref`
                            // for `ref` over exactly those tokens. Multiple
                            // unused-mut captures on the same closure
                            // produce one note per capture, each carrying
                            // the same edit (the dispatcher in `cmd_fix`
                            // dedupes overlapping edits before applying).
                            let replacement = prefix_span.as_ref().map(|sp| {
                                Box::new(crate::resolver::TextEdit {
                                    offset: sp.offset,
                                    length: sp.length,
                                    replacement: "ref".to_string(),
                                })
                            });
                            self.notes.push(OwnershipError {
                                message: format!(
                                    "capture `{name}` declared `mut ref` but never mutated — consider `ref`",
                                ),
                                span: expr.span.clone(),
                                kind: OwnershipErrorKind::UnusedMutCaptureNote,
                                suggestion: Some(
                                    "change the closure prefix from `mut ref` to `ref`"
                                        .to_string(),
                                ),
                                replacement,
                                consume_span: None,
                            });
                        }
                    }
                }
                for name in pre_live {
                    if let Some(ValueState::Moved { at }) = states.get(&name) {
                        // A consume that happened inside the closure body
                        // is a closure-capture-by-move from the outer
                        // function's perspective. Round 12.42 removed
                        // the post-K2 retag (formerly Direct / ContainerStore
                        // → ClosureCapture) — RC trigger 2 routing now
                        // lives entirely in the predicate pipeline:
                        // `use_classifier` tags capture-position
                        // identifier-leaves with
                        // `ConsumeOrigin::ClosureCapture` (round 12.14)
                        // and `populate_predicate_outputs` emits the
                        // flavor-correct `RcEntry`. The K2 enforcement
                        // below fires the explicit-ref / mut-ref-mode
                        // diagnostic, which is the only ownership-side
                        // action remaining for this pre-live walk.
                        let at = at.clone();
                        // K2 enforcement (design.md § Closure Behavior,
                        // Rule 2½): an explicit `ref` / `mut ref` prefix
                        // forbids consume of any captured name. Fire the
                        // error at the closure expression, naming the
                        // capture and the consume site. `own` declares
                        // consume, so a consuming body is consistent.
                        if let Some(declared @ (CaptureMode::Ref | CaptureMode::MutRef)) =
                            capture_mode
                        {
                            let declared_str = match declared {
                                CaptureMode::Ref => "ref",
                                CaptureMode::MutRef => "mut ref",
                                CaptureMode::Own => unreachable!(),
                            };
                            let fix = match declared {
                                CaptureMode::Ref => {
                                    "drop the `ref` prefix (use `own` or bare) or remove the consume"
                                }
                                CaptureMode::MutRef => {
                                    "drop the `mut ref` prefix and use `own`"
                                }
                                CaptureMode::Own => unreachable!(),
                            };
                            self.errors.push(OwnershipError {
                                message: format!(
                                    "capture `{name}` declared `{declared_str}` but consumed in closure body at {}:{} — {fix}",
                                    at.line, at.column,
                                ),
                                span: expr.span.clone(),
                                kind: OwnershipErrorKind::CaptureModeViolation,
                                suggestion: Some(fix.to_string()),
                                replacement: None,
                                consume_span: None,
                            });
                        }
                    }
                }
            }
            ExprKind::Return(Some(inner)) => {
                self.check_expr_consuming(inner, states, param_types, param_usage);
            }
            ExprKind::Break {
                value: Some(inner), ..
            }
            | ExprKind::Question(inner)
            | ExprKind::OptionalChain { object: inner, .. } => {
                self.check_expr_reading(inner, states, param_types, param_usage);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.check_expr_reading(left, states, param_types, param_usage);
                self.check_expr_reading(right, states, param_types, param_usage);
            }
            ExprKind::Tuple(exprs) => {
                for e in exprs {
                    self.check_expr_consuming(e, states, param_types, param_usage);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    self.check_expr_consuming(&field.value, states, param_types, param_usage);
                }
                if let Some(ref s) = spread {
                    self.check_expr_consuming(s, states, param_types, param_usage);
                }
            }
            ExprKind::Cast { expr: inner, .. } => {
                self.check_expr_reading(inner, states, param_types, param_usage);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.check_expr_reading(s, states, param_types, param_usage);
                }
                if let Some(e) = end {
                    self.check_expr_reading(e, states, param_types, param_usage);
                }
            }
            ExprKind::ArrayLiteral(elements) => {
                for elem in elements {
                    self.check_expr_reading(elem, states, param_types, param_usage);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.check_expr_reading(value, states, param_types, param_usage);
                self.check_expr_reading(count, states, param_types, param_usage);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for elem in items {
                    self.check_expr_reading(elem, states, param_types, param_usage);
                }
            }
            ExprKind::MapLiteral(entries) => {
                for (key, val) in entries {
                    self.check_expr_reading(key, states, param_types, param_usage);
                    self.check_expr_reading(val, states, param_types, param_usage);
                }
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.check_expr_reading(&b.value, states, param_types, param_usage);
                }
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Path { .. }
            | ExprKind::SelfType
            | ExprKind::Integer(_, _)
            | ExprKind::Float(_, _)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_)
            | ExprKind::InterpolatedStringLit(_)
            | ExprKind::Bool(_)
            | ExprKind::Continue { .. }
            | ExprKind::Return(None)
            | ExprKind::Break { value: None, .. }
            | ExprKind::PipePlaceholder
            | ExprKind::OffsetOf { .. }
            | ExprKind::Error => {}
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

    // ── RC Fallback Notes (emitted after Phase 2) ────────────────

    /// Emit one `RcFallbackNote` per RC binding, with the flavor determined
    /// by Phase 2: bindings in `arc_values` get "shared (Arc) — promoted:
    /// value crosses a parallel region"; others get "shared (Rc) — value
    /// does not cross a parallel region".
    fn emit_rc_fallback_notes(&mut self) {
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
                notes.push(OwnershipError {
                    message: format!(
                        "RC fallback inserted for '{}' ({}); {}; consume at line {}:{}, other use at line {}:{}",
                        entry.binding,
                        entry.trigger.label(),
                        flavor,
                        entry.consume_span.line,
                        entry.consume_span.column,
                        entry.other_use_span.line,
                        entry.other_use_span.column,
                    ),
                    span: entry.other_use_span.clone(),
                    kind: OwnershipErrorKind::RcFallbackNote,
                    suggestion: Some(
                        "restructure to a single ownership path, or accept the RC and silence with #[allow(rc_fallback)]"
                            .to_string(),
                    ),
                    replacement: None,
                    consume_span: Some(entry.consume_span.clone()),
                });
            }
        }
        self.notes.extend(notes);
    }

    // ── Phase 2: Rc → Arc Promotion ─────────────────────────────

    /// For each function with RC bindings, walk its body looking for any
    /// use of those bindings that lies inside a `par {}` block. Each
    /// such binding is promoted from Rc to Arc.
    ///
    /// Conservative: a binding whose live range overlaps any parallel
    /// region is Arc for its entire live range (one decision per value,
    /// matching design.md § Rc vs Arc — Two-Phase Algorithm).
    fn promote_rc_to_arc(&mut self) {
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

    fn enforce_no_rc_attrs(&mut self) {
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
fn stdlib_method_self_borrow_kind(key: &str) -> Option<BorrowKind> {
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
fn borrow_kind_display(kind: &BorrowKind) -> &'static str {
    match kind {
        BorrowKind::ImmRef => "ref T",
        BorrowKind::MutRef => "mut ref T",
        BorrowKind::ImmSlice => "Slice[T]",
        BorrowKind::MutSlice => "mut Slice[T]",
    }
}

/// Render the leading message for a slice-vs-slice / source-state-change
/// conflict. The caller appends the secondary borrow's span.
fn slice_conflict_message(shape: &SliceConflictShape, root: &str) -> String {
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
fn merge_states(
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
fn snapshot_uninit(states: &HashMap<String, ValueState>) -> HashMap<String, ValueState> {
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
fn restore_uninit_after_loop(
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
fn merge_branch_into(
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
