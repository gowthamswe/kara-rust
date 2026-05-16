//! Value model: the `Value` enum and its impls, the supporting carrier
//! types (`EnumData`, `IteratorSource`, `IteratorStep`, `FieldCell`,
//! `SharedStructInner`, `OrdValue`), the runtime error / test outcome
//! types (`ErrorTraceFrame`, `RuntimeError`, `TestOutcome`), and the
//! free helpers `try_write_or_panic` / `primitive_const_to_value`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

use crate::ast::*;
use crate::token::Span;

use super::helpers::value_compare;

// ── Error Return Trace ─────────────────────────────────────────

pub(crate) const ERROR_TRACE_MAX_DEPTH: usize = 64;

#[derive(Debug, Clone)]
pub struct ErrorTraceFrame {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub expr: String,
}

/// A user-triggered runtime error raised during interpretation (division by
/// zero, integer overflow, index out of bounds, unwrap of None/Err, etc.).
/// Distinct from compiler-invariant panics — those stay as `unreachable!`
/// because they indicate a bug in an earlier phase, not in user code.
#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
    /// `assert_eq` / `assert_ne` failures populate these with the formatted
    /// left and right values so the test runner can surface them in
    /// structured `test_fail` events. `None` for any other runtime error.
    pub left: Option<String>,
    pub right: Option<String>,
}

/// Outcome of a single test invocation, produced by
/// [`Interpreter::run_test_function`]. The runner translates this into a
/// `test_pass` or `test_fail` JSONL event.
#[derive(Debug, Clone)]
pub struct TestOutcome {
    pub passed: bool,
    pub message: Option<String>,
    pub span: Option<Span>,
    pub left: Option<String>,
    pub right: Option<String>,
}

// ── Runtime Values ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    String(String),
    Unit,
    Tuple(Vec<Value>),
    /// Sequence storage shared between the source binding and any live
    /// slice views. `Arc<RwLock<...>>` is universal — every Array
    /// allocation carries the shared cell whether or not it ever gets
    /// sliced, because retroactive upgrade when slice creation finds the
    /// source in another binding / struct field is significantly more
    /// complex. Tree-walk perf is irrelevant for v1; the extra
    /// `Arc::clone` + `RwLock::read/write` per op is the design's
    /// accepted cost. (`Arc<RwLock<>>` rather than the slice-plan-
    /// suggested `Rc<RefCell<>>` so `Value: Send + Sync` — the
    /// par-block branch evaluator uses `thread::scope` and shares
    /// captured Values across worker threads.) See Phase-5 § Slice
    /// borrow-tracking parity § sub-item 3 "Aliased interpreter
    /// representation".
    Array(Arc<RwLock<Vec<Value>>>),
    /// `Slice[T]` / `mut Slice[T]` runtime value — a window into shared
    /// storage. Created at `.as_slice()` / `.as_slice_mut()` /
    /// range-indexing / call-arg coercion sites; cloned by sharing the
    /// `Arc<RwLock<...>>` storage. Index reads / writes go through the
    /// same `try_write_or_panic` helper as direct array writes, so the
    /// runtime guard fires on aliased writes the borrow checker would
    /// otherwise reject.
    Slice {
        storage: Arc<RwLock<Vec<Value>>>,
        start: usize,
        len: usize,
        mutable: bool,
    },
    Map(Vec<(Value, Value)>),
    Struct {
        name: String,
        fields: HashMap<String, Value>,
    },
    /// A `shared struct` allocation — RC-backed, multi-holder, with
    /// per-field interior mutability for `mut` fields per design.md
    /// § Part 5: Shared Types. Aliasing a binding clones the `Arc`
    /// (refcount bump); mutations through any holder are visible to
    /// all holders. Immutable fields are stored once at construction;
    /// `mut` fields each carry their own borrow flag (RwLock here as
    /// a semantic stand-in — the codegen lowers to a 1-byte flag per
    /// design.md § cost notes).
    SharedStruct(Arc<SharedStructInner>),
    EnumVariant {
        enum_name: String,
        variant: String,
        data: EnumData,
    },
    Function {
        name: String,
        param_patterns: Vec<Pattern>,
        /// Default value expressions, aligned with `param_patterns`.
        /// `None` means the parameter has no default; `Some(expr)` is
        /// evaluated at call time when the caller omits the argument.
        param_defaults: Vec<Option<crate::ast::Expr>>,
        body: Block,
        /// Captured environment for closures
        closure_env: Option<HashMap<String, Value>>,
    },
    /// F32 total-order wrapper: NaN sorts last, implements Eq/Ord/Hash
    TotalFloat32(f32),
    /// F64 total-order wrapper: NaN sorts last, implements Eq/Ord/Hash
    TotalFloat64(f64),
    /// Atomic[T] runtime value (single-threaded: plain value)
    Atomic(Box<Value>),
    /// SortedSet[T: Ord] — B-tree–backed ordered set keyed by OrdValue.
    /// BTreeMap provides O(log n) insert/remove/contains with iteration in
    /// ascending key order. The () value makes it a set (not a map).
    SortedSet(BTreeMap<OrdValue, ()>),
    /// Set[T: Hash + Eq] — hash set backed by a Vec for interpreter simplicity.
    /// O(n) lookup is fine for testing; the typechecker enforces Hash + Eq.
    Set(Vec<Value>),
    /// Iterator value produced by `.iter()` / `.into_iter()` on a
    /// collection or by adaptor calls. `source` produces raw items
    /// (eager snapshot, chained sequence, or zipped pair); `steps` is
    /// the lazy adaptor chain applied per `next()` pull. The
    /// `IteratorSource` and `IteratorStep` enums grow as adaptors land.
    /// Tracked in `wip-list2.md` § Iterator trait — full adaptor surface.
    Iterator {
        source: IteratorSource,
        steps: Vec<IteratorStep>,
    },
    /// Sender[T] end of a Channel[T]. Wraps a shared queue so that cloning a
    /// Sender creates an additional producer that shares the same buffer.
    Sender(Arc<Mutex<VecDeque<Value>>>),
    /// Receiver[T] end of a Channel[T]. `recv()` blocks until an item is
    /// available; `try_recv()` returns immediately as `Option[T]`. In the
    /// single-threaded tree-walk interpreter the test pattern is always
    /// send-before-recv, so the queue already has items when recv fires.
    Receiver(Arc<Mutex<VecDeque<Value>>>),
    /// Aliasing slot used to back a `mut ref |...|` closure capture.
    /// Lives only inside an `Env` scope or a closure's captured-env map;
    /// never reaches user expressions because every path that reads a
    /// value goes through `Env::get`, which auto-derefs. Writes via
    /// `Env::set` propagate through the cell so mutations made inside one
    /// closure invocation are visible to the outer binding and to
    /// subsequent invocations. `Arc<Mutex<...>>` rather than
    /// `Rc<RefCell<...>>` so `par {}` can clone branch envs across thread
    /// boundaries (single-threaded mutation in practice — `par` branches
    /// run in independent envs).
    SharedCell(Arc<Mutex<Value>>),
    /// `Entry[K, V]` view returned by `Map.entry(k)` for in-place insert-or-
    /// modify. Spec at design.md § Entry[K, V].
    ///
    /// `map_var` names the original Map binding so `or_insert`,
    /// `or_insert_with`, and `and_modify` can write the mutation back via
    /// `env.set` — the interpreter's idiomatic mut-ref-self path. `None`
    /// when the entry was produced from a non-identifier receiver (rare;
    /// the chain still evaluates but mutations are dropped).
    ///
    /// `slot_idx` is the index of the `(key, value)` pair in the map's Vec
    /// when `Some` (Occupied); `None` means Vacant. The interpreter never
    /// hands a stale slot_idx to chain consumers — each method that mutates
    /// the map (or_insert / or_insert_with) refreshes the index before
    /// returning a fresh `Entry`.
    Entry {
        map_var: Option<String>,
        key: Box<Value>,
        slot_idx: Option<usize>,
    },
}

/// One mutable field on a `shared struct` instance. The spec
/// (design.md § Part 5: Shared Types) requires per-field borrow
/// tracking: reads are shared (multiple simultaneous readers OK),
/// writes are exclusive — if any other borrow (read or write) is
/// active when a write begins, the runtime panics. Tracking is
/// per field so mutating `node.left` does not conflict with reading
/// `node.right`. `RwLock::try_read` / `try_write` mirror these
/// semantics directly. Codegen lowers this to a 1-byte borrow flag
/// per the cost notes; the interpreter uses `RwLock<Value>` as a
/// semantic stand-in.
#[derive(Debug)]
pub struct FieldCell {
    pub value: RwLock<Value>,
}

impl FieldCell {
    pub fn new(v: Value) -> Self {
        FieldCell {
            value: RwLock::new(v),
        }
    }
}

/// Allocation backing a `shared struct` instance. Multiple holders
/// (each a `Value::SharedStruct(Arc::clone(...))`) share one inner;
/// mutation through any holder is visible to all. Aliasing is by
/// `Arc` clone — `let b = a` bumps the refcount, no deep copy.
///
/// Weak fields (declared `weak T` or `mut weak T`) live in dedicated
/// `weak_*_fields` maps backed by `std::sync::Weak<SharedStructInner>`
/// per design.md § Shared Types — Weak references. They never surface
/// to user code as a "raw weak" — field reads auto-upgrade and yield
/// `Option[T]`; writes accept a strong reference and downgrade.
#[derive(Debug)]
pub struct SharedStructInner {
    pub name: String,
    /// Fields without `mut` — fixed at construction, never replaced.
    pub immutable_fields: HashMap<String, Value>,
    /// Fields declared `mut` — each carries its own borrow flag.
    pub mut_fields: HashMap<String, FieldCell>,
    /// Fields declared `weak T` (no `mut`) — set at construction,
    /// not reassignable. `std::sync::Weak` mirrors the spec's storage
    /// model: assignment downgrades a strong reference; reads upgrade
    /// to `Option[T]`. Empty in v1 codegen — interpreter only.
    pub weak_immutable_fields: HashMap<String, std::sync::Weak<SharedStructInner>>,
    /// Fields declared `mut weak T` — set at construction or later
    /// via field assignment. The `RwLock` only guards the `Weak`
    /// handle itself (assignment vs concurrent read of the slot);
    /// upgrade to `Arc` is atomic via `Weak::upgrade`.
    pub weak_mut_fields: HashMap<String, RwLock<std::sync::Weak<SharedStructInner>>>,
}

/// Newtype wrapping [`Value`] that implements [`Ord`] via [`value_compare`]
/// so `Value` elements can key a `BTreeMap` without `Value` itself needing
/// to implement `Ord` globally (NaN semantics on floats make global Ord
/// unsound). Used exclusively by `Value::SortedSet`.
#[derive(Debug, Clone)]
pub struct OrdValue(pub Value);

impl PartialEq for OrdValue {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for OrdValue {}
impl PartialOrd for OrdValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        value_compare(&self.0, &other.0)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Char(a), Value::Char(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => {
                Arc::ptr_eq(a, b) || *a.read().unwrap() == *b.read().unwrap()
            }
            (
                Value::Slice {
                    storage: sa,
                    start: ssa,
                    len: la,
                    ..
                },
                Value::Slice {
                    storage: sb,
                    start: ssb,
                    len: lb,
                    ..
                },
            ) => {
                if la != lb {
                    return false;
                }
                let va = sa.read().unwrap();
                let vb = sb.read().unwrap();
                va[*ssa..*ssa + *la] == vb[*ssb..*ssb + *lb]
            }
            (
                Value::EnumVariant {
                    enum_name: a1,
                    variant: a2,
                    data: a3,
                },
                Value::EnumVariant {
                    enum_name: b1,
                    variant: b2,
                    data: b3,
                },
            ) => a1 == b1 && a2 == b2 && a3 == b3,
            (
                Value::Struct {
                    name: a1,
                    fields: a2,
                },
                Value::Struct {
                    name: b1,
                    fields: b2,
                },
            ) => a1 == b1 && a2 == b2,
            // `shared struct` equality is structural per design.md
            // § Equality Semantics — the `Eq` impl is dispatched
            // regardless of representation. `Arc::ptr_eq` is the
            // fast path for identical allocations (always equal).
            (Value::SharedStruct(a), Value::SharedStruct(b)) => {
                if Arc::ptr_eq(a, b) {
                    return true;
                }
                if a.name != b.name {
                    return false;
                }
                if a.immutable_fields != b.immutable_fields {
                    return false;
                }
                if a.mut_fields.len() != b.mut_fields.len() {
                    return false;
                }
                let mut_eq = a.mut_fields.iter().all(|(k, fa)| {
                    b.mut_fields
                        .get(k)
                        .map(|fb| {
                            let va = fa.value.try_read().ok();
                            let vb = fb.value.try_read().ok();
                            match (va, vb) {
                                (Some(x), Some(y)) => *x == *y,
                                _ => false,
                            }
                        })
                        .unwrap_or(false)
                });
                if !mut_eq {
                    return false;
                }
                // Weak fields: compare by referent identity (Arc::ptr_eq
                // on upgraded handles). Two dangling weaks are equal;
                // a dangling weak is not equal to a live weak.
                if a.weak_immutable_fields.len() != b.weak_immutable_fields.len()
                    || a.weak_mut_fields.len() != b.weak_mut_fields.len()
                {
                    return false;
                }
                let weak_imm_eq = a.weak_immutable_fields.iter().all(|(k, wa)| {
                    b.weak_immutable_fields
                        .get(k)
                        .map(|wb| weak_referent_eq(wa, wb))
                        .unwrap_or(false)
                });
                if !weak_imm_eq {
                    return false;
                }
                a.weak_mut_fields.iter().all(|(k, sa)| {
                    b.weak_mut_fields
                        .get(k)
                        .map(|sb| {
                            let wa = sa.try_read().ok();
                            let wb = sb.try_read().ok();
                            match (wa, wb) {
                                (Some(x), Some(y)) => weak_referent_eq(&x, &y),
                                _ => false,
                            }
                        })
                        .unwrap_or(false)
                })
            }
            // TotalFloat uses total ordering: NaN == NaN, -0.0 < +0.0
            (Value::TotalFloat32(a), Value::TotalFloat32(b)) => a.total_cmp(b).is_eq(),
            (Value::TotalFloat64(a), Value::TotalFloat64(b)) => a.total_cmp(b).is_eq(),
            (Value::Atomic(a), Value::Atomic(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, v)| b.iter().any(|(bk, bv)| bk == k && bv == v))
            }
            (Value::SortedSet(a), Value::SortedSet(b)) => {
                a.len() == b.len() && a.keys().zip(b.keys()).all(|(x, y)| x == y)
            }
            (Value::Set(a), Value::Set(b)) => a.len() == b.len() && a.iter().all(|x| b.contains(x)),
            // Channel ends compare by pointer identity — two Senders are equal
            // only when they wrap the exact same Arc allocation.
            (Value::Sender(a), Value::Sender(b)) => Arc::ptr_eq(a, b),
            (Value::Receiver(a), Value::Receiver(b)) => Arc::ptr_eq(a, b),
            (Value::Function { .. }, Value::Function { .. }) => false,
            // Iterators have no meaningful equality — like closures, two
            // iterator values aren't compared structurally.
            (Value::Iterator { .. }, Value::Iterator { .. }) => false,
            // Entry values aren't compared structurally either — they're
            // chain-locals returned only from Map.entry(k).
            (Value::Entry { .. }, Value::Entry { .. }) => false,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EnumData {
    Unit,
    Tuple(Vec<Value>),
    Struct(HashMap<String, Value>),
}

/// The raw-item supplier behind a `Value::Iterator`. Eager handles the
/// usual `coll.iter()` snapshot path; Chain and Zip support the
/// multi-source combinators landed in `wip-list2.md` subtask 7. Pulling
/// from an iterator goes: `pull_source` (this enum) → apply each
/// `IteratorStep` in `steps` → yield (or reject and retry).
#[derive(Debug, Clone, PartialEq)]
pub enum IteratorSource {
    /// Pre-extracted items walked by cursor — Vec/Set/SortedSet/Map/
    /// Array.iter() use this. Map yields `(K, V)` tuples, SortedSet
    /// flattens to ascending order.
    Eager { items: Vec<Value>, cursor: usize },
    /// Sequential concatenation — drive each part fully (through its
    /// own step chain) before moving to the next. Each part is itself
    /// a `Value::Iterator`; `current` is the part being drained.
    Chain { parts: Vec<Value>, current: usize },
    /// Synchronous pair — pull from `left` and `right` in lockstep,
    /// yield `(a, b)` tuples until either side ends. Each side is a
    /// `Value::Iterator`.
    Zip { left: Box<Value>, right: Box<Value> },
    /// `.flat_map(f)` — pull an outer item, apply `f` to get an inner
    /// iterator, drain the inner before pulling the next outer. The
    /// closure is `Fn(T) -> Iterator[U]`. `current_inner` holds the
    /// in-flight inner iterator across multiple `next()` pulls; `None`
    /// means we need to advance the outer on the next pull. `f` is
    /// boxed because `Value::Iterator` embeds this enum inline; the
    /// closure (`Value::Function`) lives in `f`, so without indirection
    /// `Value`'s size would recurse through itself.
    FlatMap {
        outer: Box<Value>,
        f: Box<Value>,
        current_inner: Option<Box<Value>>,
    },
    /// `.cycle()` — restart on exhaustion. `template` is the snapshot
    /// taken at construction (cloned again on each restart);
    /// `current` is the in-flight clone being drained. `exhausted`
    /// flips to true when the template itself is empty (so we don't
    /// loop forever resetting an empty source). Each cycle through
    /// the template re-runs adaptor closures held in template's own
    /// `steps`, with their stateful counters reset to construction
    /// state.
    Cycle {
        template: Box<Value>,
        current: Box<Value>,
        exhausted: bool,
    },
    /// `.chunks(n)` — non-overlapping groups of up to `n` consecutive
    /// items. Each pull collects the next `n` items into a fresh
    /// `Vec[T]` (`allocates(Heap)`); the trailing group may be
    /// shorter than `n` if the source length isn't a multiple. `n`
    /// is clamped to `n.max(1)` at the dispatch site. `exhausted`
    /// flips sticky-true once the inner exhausts AND the trailing
    /// group has been emitted.
    Chunks {
        inner: Box<Value>,
        n: usize,
        exhausted: bool,
    },
    /// `.windows(n)` — sliding view of size `n` over the source,
    /// advancing one item per pull. Each pull yields a fresh
    /// `Vec[T]` clone of the buffer (`allocates(Heap)`). The first
    /// pull primes the buffer by collecting `n` items; subsequent
    /// pulls drop the front and push one new item. If the source
    /// has fewer than `n` items, the iterator yields nothing
    /// (matches Rust's `[T].windows(n)` semantics). `primed` is
    /// false on the first pull.
    Windows {
        inner: Box<Value>,
        n: usize,
        buffer: Vec<Value>,
        primed: bool,
        exhausted: bool,
    },
    /// `.chunk_by(key_fn)` — buffering adaptor that groups consecutive
    /// elements where `key_fn(item)` produces equal keys. Each pull
    /// yields one `Vec[T]` group; allocates a fresh Vec per group
    /// (effect-checker carries `allocates(Heap)` for
    /// `Iterator.chunk_by`). Modeled as a Source rather than a Step
    /// because one outer pull can consume many inner items, and the
    /// boundary between groups requires a one-item lookahead — when
    /// the key changes, the trailing item that triggered the change
    /// becomes the seed of the NEXT group, so we stash it in
    /// `pending_item` (with its already-computed `pending_key` so we
    /// don't re-fire the closure) until the following pull.
    /// `exhausted` flips after the inner returns None and the final
    /// in-flight group has been drained. `key_fn` is boxed for the
    /// same reason FlatMap's `f` is — without indirection
    /// `Value::Iterator → IteratorSource::ChunkBy → Value::Function`
    /// would make `Value`'s size cycle through itself.
    ChunkBy {
        inner: Box<Value>,
        key_fn: Box<Value>,
        pending_item: Option<Box<Value>>,
        pending_key: Option<Box<Value>>,
        exhausted: bool,
    },
    /// `.peekable()` — single-element lookahead buffer. `inner` is the
    /// underlying iterator (with all its own steps); `buffered` holds
    /// the next element if `peek()` has been called and not yet
    /// consumed by `next()`. Pulls drain from the buffer first; when
    /// empty, fall through to `iterator_step(inner)`. The wrapping
    /// `Value::Iterator`'s `steps` is always empty in well-typed
    /// programs because adaptors after `.peekable()` return
    /// `Iterator[U]` (not `Peekable[U]`), so `peek()` becomes
    /// type-unavailable downstream — meaning peek and next agree on
    /// the item type without needing to walk steps.
    Peekable {
        inner: Box<Value>,
        buffered: Option<Box<Value>>,
    },
}

/// One step in a `Value::Iterator`'s lazy adaptor chain. Each step is a
/// transform applied per `next()` pull. Some steps carry mutable state
/// (positional counters for `enumerate` / `take` / `skip`); the per-call
/// state is mutated on the cloned chain inside `iterator_step` and the
/// updated chain is written back to the iterator value before return.
#[derive(Debug, Clone, PartialEq)]
pub enum IteratorStep {
    /// `.map(f)` — apply `f` to each item before yielding.
    /// The Value is a `Value::Function` (closure).
    Map(Value),
    /// `.filter(pred)` — yield only items where `pred(item)` is `true`.
    /// The Value is a `Value::Function` (closure returning `bool`).
    Filter(Value),
    /// `.enumerate()` — wrap each item into `(idx, item)`. The `usize`
    /// is the index of the *next* yielded item (incremented after wrap).
    Enumerate(usize),
    /// `.take(n)` — yield at most `n` items. The `usize` is the number
    /// of items remaining to yield; once it hits 0, the step signals
    /// "stop" and the iterator's cursor is advanced past end.
    Take(usize),
    /// `.skip(n)` — drop the first `n` items the step sees. The `usize`
    /// is the number of items still to skip; while > 0, the step
    /// rejects the item and decrements.
    Skip(usize),
    /// `.take_while(pred)` — yield items while `pred(item)` returns
    /// true; on the first false, signal stop (drain the source) and
    /// remain stopped on every subsequent pull. The `bool` flag tracks
    /// whether we've already seen the trip element so future pulls go
    /// straight to "stop" without re-firing the predicate.
    TakeWhile { pred: Value, done: bool },
    /// `.skip_while(pred)` — drop items while `pred(item)` returns
    /// true; on the first false, yield that element AND every
    /// subsequent element unconditionally. The `bool` flag flips once
    /// the predicate fails so future pulls bypass it entirely.
    SkipWhile { pred: Value, done: bool },
    /// `.step_by(n)` — yield every n-th item (n ≥ 1). The first item
    /// is always yielded; `remaining_skip` tracks how many items to
    /// reject before the next yield. Construction guarantees n ≥ 1
    /// (clamped at the dispatch site); n = 0 would underflow on the
    /// post-yield reset.
    StepBy { n: usize, remaining_skip: usize },
    /// `.inspect(f)` — invoke `f` on each item for its side effects,
    /// then pass the item through unchanged. The closure's return
    /// value is discarded.
    Inspect(Value),
    /// `.scan(init, f)` — thread mutable state through the iterator.
    /// `f` has signature `Fn(A, T) -> Option<(A, U)>`: returns
    /// `Some((new_state, yielded))` to advance and yield, or `None`
    /// to stop. The `done` flag flips sticky-true after the first
    /// `None` so subsequent pulls short-circuit without re-firing
    /// the closure. Note: this departs from Rust's
    /// `Fn(&mut St, T) -> Option<B>` because tree-walk closures
    /// snapshot captures and there's no `mut ref` parameter mode at
    /// the value layer; threading state via the return tuple is
    /// the simplest fix and matches the existing fold pattern
    /// (closure returns the new accumulator).
    Scan { f: Value, state: Value, done: bool },
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(v) => write!(f, "{}", v),
            Value::Float(v) => write!(f, "{}", v),
            Value::Bool(v) => write!(f, "{}", v),
            Value::Char(v) => write!(f, "{}", v),
            Value::String(v) => write!(f, "{}", v),
            Value::Unit => write!(f, "()"),
            Value::Tuple(vals) => {
                write!(f, "(")?;
                for (i, v) in vals.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, ")")
            }
            Value::Array(rc) => {
                let vals = rc.read().unwrap();
                write!(f, "[")?;
                for (i, v) in vals.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
            Value::Slice {
                storage,
                start,
                len,
                ..
            } => {
                let vals = storage.read().unwrap();
                write!(f, "[")?;
                for (i, v) in vals[*start..*start + *len].iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
            Value::Map(entries) => {
                write!(f, "{{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, "}}")
            }
            Value::Struct { name, fields } => {
                write!(f, "{} {{ ", name)?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, " }}")
            }
            Value::SharedStruct(inner) => {
                write!(f, "{} {{ ", inner.name)?;
                let mut first = true;
                for (k, v) in &inner.immutable_fields {
                    if !first {
                        write!(f, ", ")?;
                    }
                    first = false;
                    write!(f, "{}: {}", k, v)?;
                }
                for (k, cell) in &inner.mut_fields {
                    if !first {
                        write!(f, ", ")?;
                    }
                    first = false;
                    let v = cell.value.try_read().expect(
                        "shared struct field write-locked during Display — unreachable in single-task interpreter",
                    );
                    write!(f, "{}: {}", k, *v)?;
                }
                for (k, weak) in &inner.weak_immutable_fields {
                    if !first {
                        write!(f, ", ")?;
                    }
                    first = false;
                    write!(f, "{}: {}", k, upgrade_weak_to_option(weak))?;
                }
                for (k, slot) in &inner.weak_mut_fields {
                    if !first {
                        write!(f, ", ")?;
                    }
                    first = false;
                    let weak = slot.try_read().expect(
                        "shared struct weak field write-locked during Display — unreachable in single-task interpreter",
                    );
                    write!(f, "{}: {}", k, upgrade_weak_to_option(&weak))?;
                }
                write!(f, " }}")
            }
            Value::EnumVariant { variant, data, .. } => match data {
                EnumData::Unit => write!(f, "{}", variant),
                EnumData::Tuple(vals) => {
                    write!(f, "{}(", variant)?;
                    for (i, v) in vals.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", v)?;
                    }
                    write!(f, ")")
                }
                EnumData::Struct(fields) => {
                    write!(f, "{} {{ ", variant)?;
                    for (i, (k, v)) in fields.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}: {}", k, v)?;
                    }
                    write!(f, " }}")
                }
            },
            Value::Function { name, .. } => write!(f, "<fn {}>", name),
            Value::TotalFloat32(v) => write!(f, "F32({})", v),
            Value::TotalFloat64(v) => write!(f, "F64({})", v),
            Value::Atomic(v) => write!(f, "Atomic({})", v),
            Value::SortedSet(set) => {
                write!(f, "SortedSet{{")?;
                for (i, k) in set.keys().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", k.0)?;
                }
                write!(f, "}}")
            }
            Value::Set(elems) => {
                write!(f, "Set{{")?;
                for (i, v) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "}}")
            }
            Value::Sender(_) => write!(f, "<Sender>"),
            Value::Receiver(_) => write!(f, "<Receiver>"),
            Value::Iterator { source, .. } => match source {
                IteratorSource::Eager { items, cursor } => {
                    write!(f, "<iter {}/{}>", cursor, items.len())
                }
                IteratorSource::Chain { parts, current } => {
                    write!(f, "<iter chain {}/{}>", current, parts.len())
                }
                IteratorSource::Zip { .. } => write!(f, "<iter zip>"),
                IteratorSource::FlatMap { .. } => write!(f, "<iter flat_map>"),
                IteratorSource::Cycle { .. } => write!(f, "<iter cycle>"),
                IteratorSource::Peekable { .. } => write!(f, "<iter peekable>"),
                IteratorSource::Chunks { .. } => write!(f, "<iter chunks>"),
                IteratorSource::Windows { .. } => write!(f, "<iter windows>"),
                IteratorSource::ChunkBy { .. } => write!(f, "<iter chunk_by>"),
            },
            Value::SharedCell(cell) => write!(f, "{}", cell.lock().unwrap()),
            Value::Entry {
                map_var,
                key,
                slot_idx,
            } => {
                let occ = if slot_idx.is_some() {
                    "Occupied"
                } else {
                    "Vacant"
                };
                let mv = map_var.as_deref().unwrap_or("?");
                write!(f, "<{} entry for {} in {}>", occ, key, mv)
            }
        }
    }
}

/// Slice 3 runtime guard — write-lock the shared array storage,
/// panicking with an aliased-write message if another reader or writer
/// is currently holding it. Centralized at every mutating array / slice
/// site (push / pop / insert / remove / set_element / index-assignment)
/// so the `panic_on_aliased_write` rule has one structural enforcement
/// point. The `source_label` is best-effort context — derived from the
/// active expression's place-expression root when available, else
/// `"<value>"`.
pub(crate) fn try_write_or_panic<'a>(
    storage: &'a Arc<RwLock<Vec<Value>>>,
    source_label: &str,
) -> std::sync::RwLockWriteGuard<'a, Vec<Value>> {
    storage.try_write().unwrap_or_else(|_| {
        panic!(
            "aliased write detected: {} mutated while a borrow is live",
            source_label
        )
    })
}

/// Coerce a primitive-type associated constant to the type-erased
/// runtime value the interpreter uses. Signed and unsigned integer
/// constants share `Value::Int(i64)`; both float widths share
/// `Value::Float(f64)`. The codegen path uses the same `ConstValue`
/// table but emits the correct LLVM constant width per variant.
pub(crate) fn primitive_const_to_value(cv: &crate::prelude::ConstValue) -> Value {
    use crate::prelude::ConstValue::*;
    match cv {
        I8(v) => Value::Int(*v as i64),
        I16(v) => Value::Int(*v as i64),
        I32(v) => Value::Int(*v as i64),
        I64(v) => Value::Int(*v),
        // Const generics slice 2b: i128 / u128 coercion to Value::Int(i64)
        // is lossy — values that overflow i64 are silently truncated.
        // The slice 2 plan's hard-stop fallback acknowledged this:
        // i128 const-args evaluate cleanly at the typechecker (compile-
        // time fold) but the interpreter's runtime Value can't hold
        // 128-bit values. A future Value::Int128 widening replaces this
        // truncation; today the only path that reaches here is the
        // primitive-table coercion for `i128.MAX` / `i128.MIN` style
        // associated constants — none are defined in PRIMITIVE_CONSTS
        // for the 128-bit widths.
        I128(v) => Value::Int(*v as i64),
        U8(v) => Value::Int(*v as i64),
        U16(v) => Value::Int(*v as i64),
        U32(v) => Value::Int(*v as i64),
        U64(v) => Value::Int(*v as i64),
        U128(v) => Value::Int(*v as i64),
        Usize(v) => Value::Int(*v as i64),
        F32(v) => Value::Float(*v as f64),
        F64(v) => Value::Float(*v),
        Bool(b) => Value::Bool(*b),
        Char(c) => Value::Char(*c),
        // Fieldless-enum constants surface as a unit variant; the
        // interpreter's enum-variant representation carries the parent
        // enum + variant name as strings.
        EnumVariant {
            enum_name,
            variant_name,
            ..
        } => Value::EnumVariant {
            enum_name: enum_name.clone(),
            variant: variant_name.clone(),
            data: EnumData::Unit,
        },
    }
}

impl Value {
    /// Slice 3 helper — wrap a fresh `Vec<Value>` in the shared
    /// `Arc<RwLock<>>` storage used for `Value::Array`. Every Array
    /// allocation goes through this so the rep upgrade stays uniform.
    pub fn array_of(items: Vec<Value>) -> Value {
        Value::Array(Arc::new(RwLock::new(items)))
    }

    /// Slice 3 helper — borrow the inner `Vec<Value>` for read-only access.
    /// Returns `None` for non-array values so callers can fall through to
    /// other arms cleanly. The guard is held for the lifetime of the
    /// returned `RwLockReadGuard`, so callers should keep it scoped.
    pub fn as_array_borrow(&self) -> Option<RwLockReadGuard<'_, Vec<Value>>> {
        match self {
            Value::Array(rc) => Some(rc.read().unwrap()),
            _ => None,
        }
    }

    /// Format for programmer-facing debug output.
    /// Strings are quoted, chars are single-quoted; compound values recurse.
    pub fn debug_fmt(&self) -> String {
        match self {
            Value::String(v) => format!("{:?}", v),
            Value::Char(v) => format!("{:?}", v),
            Value::Tuple(vals) => {
                let inner: Vec<String> = vals.iter().map(|v| v.debug_fmt()).collect();
                format!("({})", inner.join(", "))
            }
            Value::Array(rc) => {
                let vals = rc.read().unwrap();
                let inner: Vec<String> = vals.iter().map(|v| v.debug_fmt()).collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Slice {
                storage,
                start,
                len,
                ..
            } => {
                let vals = storage.read().unwrap();
                let inner: Vec<String> = vals[*start..*start + *len]
                    .iter()
                    .map(|v| v.debug_fmt())
                    .collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Map(entries) => {
                let pairs: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.debug_fmt(), v.debug_fmt()))
                    .collect();
                format!("{{{}}}", pairs.join(", "))
            }
            Value::Struct { name, fields } => {
                let field_strs: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.debug_fmt()))
                    .collect();
                format!("{} {{ {} }}", name, field_strs.join(", "))
            }
            Value::SharedStruct(inner) => {
                let mut parts: Vec<String> = inner
                    .immutable_fields
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.debug_fmt()))
                    .collect();
                for (k, cell) in &inner.mut_fields {
                    let v = cell.value.try_read().expect(
                        "shared struct field write-locked during debug_fmt — unreachable in single-task interpreter",
                    );
                    parts.push(format!("{}: {}", k, v.debug_fmt()));
                }
                for (k, weak) in &inner.weak_immutable_fields {
                    parts.push(format!(
                        "{}: {}",
                        k,
                        upgrade_weak_to_option(weak).debug_fmt()
                    ));
                }
                for (k, slot) in &inner.weak_mut_fields {
                    let weak = slot.try_read().expect(
                        "shared struct weak field write-locked during debug_fmt — unreachable in single-task interpreter",
                    );
                    parts.push(format!(
                        "{}: {}",
                        k,
                        upgrade_weak_to_option(&weak).debug_fmt()
                    ));
                }
                format!("{} {{ {} }}", inner.name, parts.join(", "))
            }
            Value::EnumVariant { variant, data, .. } => match data {
                EnumData::Unit => variant.clone(),
                EnumData::Tuple(vals) => {
                    let inner: Vec<String> = vals.iter().map(|v| v.debug_fmt()).collect();
                    format!("{}({})", variant, inner.join(", "))
                }
                EnumData::Struct(fields) => {
                    let field_strs: Vec<String> = fields
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k, v.debug_fmt()))
                        .collect();
                    format!("{} {{ {} }}", variant, field_strs.join(", "))
                }
            },
            Value::SortedSet(set) => {
                let inner: Vec<String> = set.keys().map(|k| k.0.debug_fmt()).collect();
                format!("SortedSet{{{}}}", inner.join(", "))
            }
            Value::Set(elems) => {
                let inner: Vec<String> = elems.iter().map(|v| v.debug_fmt()).collect();
                format!("Set{{{}}}", inner.join(", "))
            }
            Value::Sender(_) => "<Sender>".to_string(),
            Value::Receiver(_) => "<Receiver>".to_string(),
            other => format!("{}", other),
        }
    }
}

/// Identity comparison between two `Weak<SharedStructInner>` handles
/// for use in `Value::SharedStruct` PartialEq. Two weaks are equal iff
/// they point at the same allocation (`Arc::ptr_eq` after upgrade) or
/// both are dangling. A dangling weak is never equal to a live one.
pub(crate) fn weak_referent_eq(
    a: &std::sync::Weak<SharedStructInner>,
    b: &std::sync::Weak<SharedStructInner>,
) -> bool {
    match (a.upgrade(), b.upgrade()) {
        (None, None) => true,
        (Some(x), Some(y)) => Arc::ptr_eq(&x, &y),
        _ => false,
    }
}

/// Upgrade a stored `Weak<SharedStructInner>` to a runtime `Option[T]`
/// per design.md § Shared Types — Weak references. Returns
/// `Some(SharedStruct)` when the referent is still alive (the upgrade
/// bumps the strong RC), or `None` if every strong holder has been
/// dropped. Used at every `weak`-field read site and any `.upgrade()`
/// dispatch.
pub(crate) fn upgrade_weak_to_option(weak: &std::sync::Weak<SharedStructInner>) -> Value {
    match weak.upgrade() {
        Some(arc) => Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "Some".to_string(),
            data: EnumData::Tuple(vec![Value::SharedStruct(arc)]),
        },
        None => Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "None".to_string(),
            data: EnumData::Unit,
        },
    }
}
