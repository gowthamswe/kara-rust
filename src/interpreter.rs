use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use regex::Regex as RustRegex;
use ureq;

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;
use crate::typechecker::TypeCheckResult;

// ── Error Return Trace ─────────────────────────────────────────

const ERROR_TRACE_MAX_DEPTH: usize = 64;

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
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Struct {
        name: String,
        fields: HashMap<String, Value>,
    },
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
    /// Iterator value produced by `.iter()` / `.into_iter()` on a collection.
    /// `items` holds the source elements eagerly snapshotted at construction
    /// (Array as-is, Map as `(K, V)` tuples, Set/SortedSet flattened);
    /// `cursor` is the next-yield index; `steps` is the lazy adaptor chain
    /// applied per `next()` pull. The `IteratorStep` enum grows as adaptors
    /// land. Tracked in `wip-list2.md` § Iterator trait — full adaptor surface.
    Iterator {
        items: Vec<Value>,
        cursor: usize,
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
            (Value::Array(a), Value::Array(b)) => a == b,
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

/// One step in a `Value::Iterator`'s lazy adaptor chain. Each step is a
/// closure-bearing transform applied per `next()` pull. New variants land
/// as the adaptor surface grows in `wip-list2.md` subtasks 6+.
#[derive(Debug, Clone, PartialEq)]
pub enum IteratorStep {
    /// `.map(f)` — apply `f` to each item before yielding.
    /// The Value is a `Value::Function` (closure).
    Map(Value),
    /// `.filter(pred)` — yield only items where `pred(item)` is `true`.
    /// The Value is a `Value::Function` (closure returning `bool`).
    Filter(Value),
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
            Value::Array(vals) => {
                write!(f, "[")?;
                for (i, v) in vals.iter().enumerate() {
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
            Value::Iterator { items, cursor, .. } => {
                write!(f, "<iter {}/{}>", cursor, items.len())
            }
            Value::SharedCell(cell) => write!(f, "{}", cell.lock().unwrap()),
        }
    }
}

impl Value {
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
            Value::Array(vals) => {
                let inner: Vec<String> = vals.iter().map(|v| v.debug_fmt()).collect();
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

// ── Control Flow Signals ────────────────────────────────────────

/// Signals for non-local control flow (return, break, continue, exit).
#[derive(Debug)]
enum ControlFlow {
    Return(Value),
    Break {
        label: Option<String>,
        value: Option<Value>,
    },
    Continue {
        label: Option<String>,
    },
    /// process::exit() — defer-respecting, uncatchable exit.
    /// Distinct from Return so future catch_panic cannot swallow it.
    ExitUnwind {
        code: i32,
    },
    /// A user-triggered runtime error. The error details are in
    /// `Interpreter::runtime_errors`; this variant is the unwind signal.
    RuntimeError,
}

type EvalResult = Result<Value, ControlFlow>;

// ── Scoped Environment ──────────────────────────────────────────

#[derive(Debug, Clone)]
struct Env {
    pub(crate) scopes: Vec<HashMap<String, Value>>,
}

impl Env {
    fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, val: Value) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, val);
        }
    }

    fn set(&mut self, name: &str, val: Value) {
        // Update in the nearest scope that has this name. If the existing
        // slot is a `SharedCell` (a `mut ref` closure capture aliased back
        // to the outer binding) the assignment writes through the cell so
        // the outer binding observes the mutation.
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                if let Value::SharedCell(cell) = slot {
                    *cell.lock().unwrap() = val;
                } else {
                    *slot = val;
                }
                return;
            }
        }
        // If not found, define in current scope
        self.define(name.to_string(), val);
    }

    /// Read a binding by name. Auto-derefs `SharedCell` so callers always
    /// see the underlying value rather than the aliasing slot.
    fn get(&self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(match v {
                    Value::SharedCell(cell) => cell.lock().unwrap().clone(),
                    other => other.clone(),
                });
            }
        }
        None
    }

    /// Snapshot current env for closure capture. Preserves `SharedCell`
    /// slots verbatim so a captured `mut ref` alias keeps pointing at the
    /// shared cell when the closure dispatches.
    fn snapshot(&self) -> HashMap<String, Value> {
        let mut all = HashMap::new();
        for scope in &self.scopes {
            for (k, v) in scope {
                all.insert(k.clone(), v.clone());
            }
        }
        all
    }

    /// Promote a binding's slot to `SharedCell`, if it isn't one already,
    /// and return a clone of the resulting cell value (also a `SharedCell`)
    /// so callers can install the same alias into a closure's captured-env
    /// map. Used at construction of a `mut ref |...|` closure to convert
    /// each captured outer binding into an aliased cell so mutations made
    /// inside the closure body propagate back.
    fn wrap_capture(&mut self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                if !matches!(slot, Value::SharedCell(_)) {
                    let inner = std::mem::replace(slot, Value::Unit);
                    *slot = Value::SharedCell(Arc::new(Mutex::new(inner)));
                }
                return Some(slot.clone());
            }
        }
        None
    }
}

// ── Free-variable analysis for `mut ref |...|` closures ────────
//
// Walks a closure body collecting every identifier that resolves outside
// the closure (i.e. is not introduced by a closure param, body-local
// `let`, pattern binding, or nested closure param). The interpreter uses
// this set to decide which outer-scope bindings to promote to
// `Value::SharedCell` so mutations propagate back. Conservative against
// shadowing: a name that appears in the body before a `let` of the same
// name is captured; a name that appears only after the `let` is treated
// as the inner shadow and not captured.
fn add_pattern_bindings(pat: &Pattern, out: &mut HashSet<String>) {
    for n in pat.binding_names() {
        out.insert(n);
    }
}

fn collect_free_idents_block(block: &Block, bound: &mut HashSet<String>, out: &mut Vec<String>) {
    let snapshot = bound.clone();
    for stmt in &block.stmts {
        match &stmt.kind {
            StmtKind::Let { pattern, value, .. } => {
                collect_free_idents_expr(value, bound, out);
                add_pattern_bindings(pattern, bound);
            }
            StmtKind::LetUninit { name, .. } => {
                bound.insert(name.clone());
            }
            StmtKind::LetElse {
                pattern,
                value,
                else_block,
                ..
            } => {
                collect_free_idents_expr(value, bound, out);
                let snap = bound.clone();
                collect_free_idents_block(else_block, bound, out);
                *bound = snap;
                add_pattern_bindings(pattern, bound);
            }
            StmtKind::Defer { body } => collect_free_idents_block(body, bound, out),
            StmtKind::ErrDefer { body, binding } => {
                let snap = bound.clone();
                if let Some(n) = binding {
                    bound.insert(n.clone());
                }
                collect_free_idents_block(body, bound, out);
                *bound = snap;
            }
            StmtKind::Assign { target, value } => {
                collect_free_idents_expr(target, bound, out);
                collect_free_idents_expr(value, bound, out);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                collect_free_idents_expr(target, bound, out);
                collect_free_idents_expr(value, bound, out);
            }
            StmtKind::Expr(e) => collect_free_idents_expr(e, bound, out),
        }
    }
    if let Some(final_expr) = &block.final_expr {
        collect_free_idents_expr(final_expr, bound, out);
    }
    *bound = snapshot;
}

fn collect_free_idents_expr(expr: &Expr, bound: &mut HashSet<String>, out: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Identifier(name) => {
            if !bound.contains(name) {
                out.push(name.clone());
            }
        }
        ExprKind::Path(_)
        | ExprKind::Integer(_, _)
        | ExprKind::Float(_, _)
        | ExprKind::Bool(_)
        | ExprKind::CharLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::MultiStringLit(_)
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::PipePlaceholder
        | ExprKind::Continue { .. }
        | ExprKind::Error => {}
        ExprKind::InterpolatedStringLit(parts) => {
            for part in parts {
                if let crate::ast::ParsedInterpolationPart::Expr(e) = part {
                    collect_free_idents_expr(e, bound, out);
                }
            }
        }
        ExprKind::Binary { left, right, .. } => {
            collect_free_idents_expr(left, bound, out);
            collect_free_idents_expr(right, bound, out);
        }
        ExprKind::Unary { operand, .. } => {
            collect_free_idents_expr(operand, bound, out);
        }
        ExprKind::Call { callee, args } => {
            collect_free_idents_expr(callee, bound, out);
            for arg in args {
                collect_free_idents_expr(&arg.value, bound, out);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            collect_free_idents_expr(object, bound, out);
            for arg in args {
                collect_free_idents_expr(&arg.value, bound, out);
            }
        }
        ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
            collect_free_idents_expr(object, bound, out);
        }
        ExprKind::OptionalChain { object, args, .. } => {
            collect_free_idents_expr(object, bound, out);
            if let Some(args) = args {
                for arg in args {
                    collect_free_idents_expr(&arg.value, bound, out);
                }
            }
        }
        ExprKind::NilCoalesce { left, right } => {
            collect_free_idents_expr(left, bound, out);
            collect_free_idents_expr(right, bound, out);
        }
        ExprKind::Index { object, index } => {
            collect_free_idents_expr(object, bound, out);
            collect_free_idents_expr(index, bound, out);
        }
        ExprKind::Block(b) => collect_free_idents_block(b, bound, out),
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            collect_free_idents_expr(condition, bound, out);
            collect_free_idents_block(then_block, bound, out);
            if let Some(eb) = else_branch {
                collect_free_idents_expr(eb, bound, out);
            }
        }
        ExprKind::IfLet {
            pattern,
            value,
            then_block,
            else_branch,
        } => {
            collect_free_idents_expr(value, bound, out);
            let snapshot = bound.clone();
            add_pattern_bindings(pattern, bound);
            collect_free_idents_block(then_block, bound, out);
            *bound = snapshot;
            if let Some(eb) = else_branch {
                collect_free_idents_expr(eb, bound, out);
            }
        }
        ExprKind::While {
            condition, body, ..
        } => {
            collect_free_idents_expr(condition, bound, out);
            collect_free_idents_block(body, bound, out);
        }
        ExprKind::WhileLet {
            pattern,
            value,
            body,
            ..
        } => {
            collect_free_idents_expr(value, bound, out);
            let snapshot = bound.clone();
            add_pattern_bindings(pattern, bound);
            collect_free_idents_block(body, bound, out);
            *bound = snapshot;
        }
        ExprKind::Loop { body, .. } => collect_free_idents_block(body, bound, out),
        ExprKind::For {
            pattern,
            iterable,
            body,
            ..
        } => {
            collect_free_idents_expr(iterable, bound, out);
            let snapshot = bound.clone();
            add_pattern_bindings(pattern, bound);
            collect_free_idents_block(body, bound, out);
            *bound = snapshot;
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_free_idents_expr(scrutinee, bound, out);
            for arm in arms {
                let snapshot = bound.clone();
                add_pattern_bindings(&arm.pattern, bound);
                if let Some(g) = &arm.guard {
                    collect_free_idents_expr(g, bound, out);
                }
                collect_free_idents_expr(&arm.body, bound, out);
                *bound = snapshot;
            }
        }
        ExprKind::Closure { params, body, .. } => {
            let snapshot = bound.clone();
            for p in params {
                add_pattern_bindings(&p.pattern, bound);
            }
            collect_free_idents_expr(body, bound, out);
            *bound = snapshot;
        }
        ExprKind::Tuple(items) | ExprKind::ArrayLiteral(items) => {
            for it in items {
                collect_free_idents_expr(it, bound, out);
            }
        }
        ExprKind::PrefixCollectionLiteral { items, .. } => {
            for it in items {
                collect_free_idents_expr(it, bound, out);
            }
        }
        ExprKind::RepeatLiteral { value, count, .. } => {
            collect_free_idents_expr(value, bound, out);
            collect_free_idents_expr(count, bound, out);
        }
        ExprKind::MapLiteral(entries) => {
            for (k, v) in entries {
                collect_free_idents_expr(k, bound, out);
                collect_free_idents_expr(v, bound, out);
            }
        }
        ExprKind::StructLiteral { fields, spread, .. } => {
            for f in fields {
                collect_free_idents_expr(&f.value, bound, out);
            }
            if let Some(s) = spread {
                collect_free_idents_expr(s, bound, out);
            }
        }
        ExprKind::Return(opt) => {
            if let Some(e) = opt {
                collect_free_idents_expr(e, bound, out);
            }
        }
        ExprKind::Break { value: opt, .. } => {
            if let Some(e) = opt {
                collect_free_idents_expr(e, bound, out);
            }
        }
        ExprKind::Question(inner) | ExprKind::Cast { expr: inner, .. } => {
            collect_free_idents_expr(inner, bound, out);
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                collect_free_idents_expr(s, bound, out);
            }
            if let Some(e) = end {
                collect_free_idents_expr(e, bound, out);
            }
        }
        ExprKind::Pipe { left, right } => {
            collect_free_idents_expr(left, bound, out);
            collect_free_idents_expr(right, bound, out);
        }
        ExprKind::Par(b) | ExprKind::Seq(b) | ExprKind::Unsafe(b) => {
            collect_free_idents_block(b, bound, out);
        }
        ExprKind::Lock { body, alias, .. } => {
            let snap = bound.clone();
            if let Some(a) = alias {
                bound.insert(a.clone());
            }
            collect_free_idents_block(body, bound, out);
            *bound = snap;
        }
        ExprKind::Providers { bindings, body } => {
            for b in bindings {
                collect_free_idents_expr(&b.value, bound, out);
            }
            collect_free_idents_block(body, bound, out);
        }
    }
}

// ── Interpreter ─────────────────────────────────────────────────

pub struct Interpreter<'a> {
    program: &'a Program,
    #[allow(dead_code)]
    typecheck_result: &'a TypeCheckResult,
    env: Env,
    /// Captured output for testing (when Some, print/println write here instead of stdout)
    pub captured_output: Option<Vec<String>>,
    /// Pending control flow signal (return/break/continue)
    pending_cf: Option<ControlFlow>,
    /// Runtime effect tracking: records effects performed during execution
    pub tracked_effects: Vec<String>,
    /// Tracks variables that have been moved (ownership simulation)
    #[allow(dead_code)]
    moved_vars: std::collections::HashSet<String>,
    /// Error return trace: ring buffer of (file, line, expr_text) for ? propagation
    error_trace: Vec<ErrorTraceFrame>,
    /// Whether oldest entries were dropped from the trace ring buffer
    error_trace_truncated: bool,
    /// Source filename for error trace frames
    source_filename: String,
    /// When true, par {} blocks execute sequentially (--sequential mode)
    pub sequential_mode: bool,
    /// User-triggered runtime errors collected during execution. Populated by
    /// `record_runtime_error`; inspected by tests / CLI to surface program-level
    /// failures (div by zero, overflow, unwrap of None, index out of bounds, etc.).
    pub runtime_errors: Vec<RuntimeError>,
    /// Per-task stack of provider maps for `with_provider` (design.md §
    /// Provider-Rooted Resources > Runtime mechanics). Each frame binds
    /// `effect resource R` names (keyed by the resolver's fully-qualified
    /// path — currently the bare name at the module-tree level) to an
    /// `Arc`-wrapped provider `Value`. `with_provider[R](p, closure)`
    /// pushes a frame, runs the closure, pops. Resource method calls
    /// `UserDB.foo(...)` resolve by top-down search for the resource name.
    /// The base frame (index 0) holds defaults for ambient program-rooted
    /// resources (planted by a later CR); the tree-walk interpreter is
    /// single-threaded so all frames live on one stack.
    provider_stack: Vec<HashMap<String, Arc<Value>>>,
    /// Names of `effect resource` declarations in the program, collected
    /// at [`register_items`] time. Used by [`eval_method_call`] to detect
    /// receivers of the form `UserDB.query(...)` — where `UserDB` is not
    /// a value binding — and dispatch via the provider stack instead of
    /// normal method lookup.
    effect_resources: HashSet<String>,
    /// Xorshift64 state backing the default `RandomSource` provider.
    /// Seeded once per [`Interpreter::new`] from the system clock's
    /// sub-second nanoseconds so repeated `cargo test` runs see fresh
    /// sequences. `with_provider[RandomSource](Fake…)` shadows this
    /// entirely; determinism-sensitive tests must opt in via a fake.
    rand_state: u64,
    /// Per-call frame of generic-param substitutions: name → concrete type
    /// name. Pushed at every generic call (using
    /// `TypeCheckResult.call_type_subs` keyed by call span); popped on
    /// return. `T.method()` and bare-call dispatch in trait associated
    /// function bodies look up `T` through this stack to find the concrete
    /// impl to dispatch to. Outer-frame entries are visible (transitive
    /// resolution: a callee's `T → "U"` where `U` is itself a generic param
    /// of the caller resolves via the next frame down).
    type_subs_stack: Vec<HashMap<String, String>>,
}

/// Convert a PascalCase identifier to lower_snake_case.
/// `InProgress` → `in_progress`, `Up` → `up`, `HTTPError` → `h_t_t_p_error`.
fn pascal_to_snake(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.extend(ch.to_lowercase());
    }
    result
}

/// Seed the per-interpreter xorshift state from the system clock's
/// sub-second nanoseconds, OR'd with `1` so the state can never be zero
/// (xorshift's fixed point).
fn seed_rand_state() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos | 1
}

impl<'a> Interpreter<'a> {
    pub fn new(program: &'a Program, typecheck_result: &'a TypeCheckResult) -> Self {
        Interpreter {
            program,
            typecheck_result,
            env: Env::new(),
            captured_output: None,
            pending_cf: None,
            tracked_effects: Vec::new(),
            moved_vars: std::collections::HashSet::new(),
            error_trace: Vec::new(),
            error_trace_truncated: false,
            source_filename: String::new(),
            sequential_mode: false,
            runtime_errors: Vec::new(),
            provider_stack: vec![HashMap::new()],
            effect_resources: HashSet::new(),
            rand_state: seed_rand_state(),
            type_subs_stack: Vec::new(),
        }
    }

    /// Record a user-triggered runtime error and begin unwinding. Returns
    /// `Value::Unit` so call sites can write `return self.record_runtime_error(...)`
    /// in `Value`-returning contexts; the pending `ControlFlow::RuntimeError`
    /// short-circuits subsequent evaluation.
    fn record_runtime_error(&mut self, message: impl Into<String>, span: &Span) -> Value {
        self.runtime_errors.push(RuntimeError {
            message: message.into(),
            span: span.clone(),
            left: None,
            right: None,
        });
        self.push_error_trace(span.line, span.column);
        self.pending_cf = Some(ControlFlow::RuntimeError);
        Value::Unit
    }

    /// Like [`record_runtime_error`] but also captures the left/right values
    /// that drove the failure. Used by the `assert_eq` / `assert_ne` builtins
    /// so the test runner can surface them in structured fail events.
    fn record_runtime_assertion(
        &mut self,
        message: impl Into<String>,
        left: String,
        right: String,
        span: &Span,
    ) -> Value {
        self.runtime_errors.push(RuntimeError {
            message: message.into(),
            span: span.clone(),
            left: Some(left),
            right: Some(right),
        });
        self.push_error_trace(span.line, span.column);
        self.pending_cf = Some(ControlFlow::RuntimeError);
        Value::Unit
    }

    /// Set the source filename used in error trace frames.
    pub fn set_source_filename(&mut self, filename: &str) {
        self.source_filename = filename.to_string();
    }

    /// Get the error return trace frames collected during execution.
    pub fn error_trace(&self) -> &[ErrorTraceFrame] {
        &self.error_trace
    }

    /// Whether the trace ring buffer overflowed and oldest entries were dropped.
    pub fn error_trace_truncated(&self) -> bool {
        self.error_trace_truncated
    }

    /// Push a frame to the error trace ring buffer (max 64 entries).
    fn push_error_trace(&mut self, line: usize, column: usize) {
        if self.error_trace.len() >= ERROR_TRACE_MAX_DEPTH {
            self.error_trace.remove(0);
            self.error_trace_truncated = true;
        }
        self.error_trace.push(ErrorTraceFrame {
            file: self.source_filename.clone(),
            line,
            column,
            expr: String::new(),
        });
    }

    /// Clear the error trace (called when ? encounters Ok/Some).
    fn clear_error_trace(&mut self) {
        self.error_trace.clear();
        self.error_trace_truncated = false;
    }

    fn track_effect(&mut self, effect: &str) {
        self.tracked_effects.push(effect.to_string());
    }

    /// Push an empty provider frame. Paired with [`pop_provider_frame`].
    fn push_provider_frame(&mut self) {
        self.provider_stack.push(HashMap::new());
    }

    /// Pop the topmost provider frame. Invariant: base frame (index 0) is
    /// installed by [`register_items`] and never popped.
    fn pop_provider_frame(&mut self) {
        debug_assert!(
            self.provider_stack.len() > 1,
            "cannot pop base provider frame"
        );
        self.provider_stack.pop();
    }

    /// Bind a provider value to a resource name in the topmost frame.
    fn bind_provider(&mut self, resource: String, provider: Value) {
        if let Some(frame) = self.provider_stack.last_mut() {
            frame.insert(resource, Arc::new(provider));
        }
    }

    /// Top-down lookup for a provider bound to the given resource name.
    /// Returns `None` if no frame has a binding — the runtime raises a
    /// runtime error at the call site (design.md § Provider-Rooted
    /// Resources: ambient defaults are installed for program-rooted
    /// resources so only `effect resource R: Trait` without an active
    /// `with_provider` should miss).
    fn lookup_provider(&self, resource: &str) -> Option<Arc<Value>> {
        self.provider_stack
            .iter()
            .rev()
            .find_map(|frame| frame.get(resource).cloned())
    }

    /// Check if there's a pending control flow signal. If so, return early.
    fn check_cf(&self) -> bool {
        self.pending_cf.is_some()
    }

    /// Register top-level items so [`run_test_function`] can subsequently
    /// invoke individual test functions by name. Idempotent only in the
    /// sense that callers should invoke it exactly once per `Interpreter`
    /// instance before any [`run_test_function`] calls — invoking it twice
    /// would re-register every item.
    ///
    /// Used by `karac test`, which calls [`register_for_tests`] once per
    /// module then drives a sequence of [`run_test_function`] calls — one
    /// per discovered `test_*` function.
    pub fn register_for_tests(&mut self) {
        self.register_items();
    }

    /// Push a provider frame and bind `resource → provider` in it.
    /// Paired with [`test_pop_provider_frame`]. Used by the test runner
    /// to install `#[with_provider(R, ...)]` fixtures via the same
    /// frame primitive hand-written `with_provider` / `providers { }`
    /// scopes use. See design.md § `#[with_provider]` fixture
    /// ("runner uses the interpreter's frame-push/pop primitive
    /// directly — no AST rewrite").
    pub fn test_push_provider(&mut self, resource: String, provider: Value) {
        self.push_provider_frame();
        self.bind_provider(resource, provider);
    }

    /// Pop the topmost provider frame. Matches each
    /// [`test_push_provider`] call.
    pub fn test_pop_provider_frame(&mut self) {
        self.pop_provider_frame();
    }

    /// Evaluate an expression for use as a test provider constructor.
    /// Returns `Ok(value)` on success, `Err(message)` if the expression
    /// raised a runtime error or any control-flow signal (exit, return,
    /// panic). The caller is responsible for draining error state before
    /// the next test — the method does not roll back on failure because
    /// the runner uses [`reset_test_state`] per test anyway.
    pub fn test_eval_provider_constructor(&mut self, expr: &Expr) -> Result<Value, String> {
        let errors_before = self.runtime_errors.len();
        let had_pending = self.pending_cf.is_some();
        let value = self.eval_expr(expr);
        if self.runtime_errors.len() > errors_before {
            return Err(self.runtime_errors[errors_before].message.clone());
        }
        if !had_pending && self.pending_cf.is_some() {
            return Err("constructor did not complete normally".to_string());
        }
        Ok(value)
    }

    /// Reset per-test mutable state (`pending_cf`, `runtime_errors`,
    /// `tracked_effects`). The test runner calls this before evaluating
    /// `#[with_provider(R, ...)]` constructors so a clean slate persists
    /// whether or not constructors succeed. [`run_test_function`] already
    /// performs the same reset on entry, so calling both is harmless; the
    /// separate method exists so the runner can reset *before* the
    /// interpreter is handed a constructor expression.
    pub fn reset_test_state(&mut self) {
        self.pending_cf = None;
        self.runtime_errors.clear();
        self.tracked_effects.clear();
    }

    /// Invoke a previously-registered top-level function as a test and
    /// report whether it passed. Resets per-test mutable state
    /// (`pending_cf`, `runtime_errors`, `tracked_effects`) so each test
    /// runs from a clean slate, then dispatches into [`call_function`]
    /// and inspects [`runtime_errors`] for failure details. The first
    /// recorded `RuntimeError` becomes the [`TestOutcome::message`]; any
    /// `left` / `right` payload set by `assert_eq` / `assert_ne` flows
    /// through unchanged.
    pub fn run_test_function(&mut self, name: &str) -> TestOutcome {
        self.pending_cf = None;
        self.runtime_errors.clear();
        self.tracked_effects.clear();

        let _ = self.call_function(name, &[]);
        // Drain any pending unwind so the next test starts clean. We don't
        // act on the unwind variant here — RuntimeError populated
        // `runtime_errors`, and ExitUnwind from a test means the test
        // body called `process::exit`, which we treat as a failure.
        let unwind = self.pending_cf.take();

        if let Some(err) = self.runtime_errors.first().cloned() {
            return TestOutcome {
                passed: false,
                message: Some(err.message),
                span: Some(err.span),
                left: err.left,
                right: err.right,
            };
        }
        if let Some(ControlFlow::ExitUnwind { code }) = unwind {
            return TestOutcome {
                passed: false,
                message: Some(format!("test called process::exit({})", code)),
                span: None,
                left: None,
                right: None,
            };
        }
        TestOutcome {
            passed: true,
            message: None,
            span: None,
            left: None,
            right: None,
        }
    }

    /// Run the program: register top-level items, then call main().
    pub fn run(&mut self) -> Value {
        self.register_items();
        // Look for main()
        if self.env.get("main").is_some() {
            let result = self.call_function("main", &[]);
            // Handle ExitUnwind from process::exit(). Runtime errors also
            // drain pending_cf here; the errors themselves are in
            // `self.runtime_errors` for callers to inspect.
            match self.pending_cf.take() {
                Some(ControlFlow::ExitUnwind { code }) => std::process::exit(code),
                Some(ControlFlow::RuntimeError) => Value::Unit,
                _ => result,
            }
        } else {
            Value::Unit
        }
    }

    /// Register all top-level functions, structs, enums in the environment.
    fn register_items(&mut self) {
        // Register prelude variants
        self.env.define(
            "None".to_string(),
            Value::EnumVariant {
                enum_name: "Option".to_string(),
                variant: "None".to_string(),
                data: EnumData::Unit,
            },
        );

        // Register built-in Ordering enum variants
        for variant in ["Relaxed", "Acquire", "Release", "AcqRel", "SeqCst"] {
            self.env.define(
                format!("Ordering.{}", variant),
                Value::EnumVariant {
                    enum_name: "Ordering".to_string(),
                    variant: variant.to_string(),
                    data: EnumData::Unit,
                },
            );
        }

        // Ambient program-rooted resources: register the names and install
        // a default provider in the base frame so `Clock.now()` etc. resolve
        // without any `with_provider` wrapping (design.md § Provider-Rooted
        // Resources "Scope of the rule"). The default provider is a
        // zero-field `Value::Struct` whose name encodes the resource;
        // `eval_resource_method` recognizes the `BuiltinDefault` prefix and
        // dispatches to a Rust handler.
        for name in crate::prelude::PRELUDE_EFFECT_RESOURCES {
            self.effect_resources.insert((*name).to_string());
            let default_provider = Value::Struct {
                name: format!("BuiltinDefault{}", name),
                fields: HashMap::new(),
            };
            self.bind_provider((*name).to_string(), default_provider);
        }

        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            if let Item::EffectResource(r) = item {
                self.effect_resources.insert(r.name.clone());
            }
        }
        for item in &items {
            match item {
                Item::Function(f) => {
                    let val = Value::Function {
                        name: f.name.clone(),
                        param_patterns: f.params.iter().map(|p| p.pattern.clone()).collect(),
                        param_defaults: f.params.iter().map(|p| p.default_value.clone()).collect(),
                        body: f.body.clone(),
                        closure_env: None,
                    };
                    self.env.define(f.name.clone(), val);
                }
                Item::EnumDef(e) => {
                    // Register unit variants as values, tuple/struct variants as constructor functions
                    for variant in &e.variants {
                        match &variant.kind {
                            VariantKind::Unit => {
                                self.env.define(
                                    variant.name.clone(),
                                    Value::EnumVariant {
                                        enum_name: e.name.clone(),
                                        variant: variant.name.clone(),
                                        data: EnumData::Unit,
                                    },
                                );
                            }
                            _ => {
                                // Tuple/struct variants are handled at call sites
                            }
                        }
                    }
                }
                Item::ConstDecl(c) => {
                    let val = self.eval_expr_inner(&c.value);
                    self.env.define(c.name.clone(), val);
                }
                Item::ImplBlock(imp) => {
                    let type_name = match &imp.target_type.kind {
                        TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                        _ => continue,
                    };
                    for item in &imp.items {
                        if let ImplItem::Method(method) = item {
                            let method_key = format!("{}.{}", type_name, method.name);
                            // For methods with a receiver, prepend a `self`
                            // binding pattern so the unified Call dispatch can
                            // bind args[0] to `self` without special-casing.
                            // Associated functions (no self_param) stay as-is.
                            let mut patterns: Vec<Pattern> = Vec::new();
                            if method.self_param.is_some() {
                                patterns.push(Pattern {
                                    span: method.span.clone(),
                                    kind: PatternKind::Binding("self".to_string()),
                                });
                            }
                            patterns.extend(method.params.iter().map(|p| p.pattern.clone()));
                            // `self` has no default; align defaults with the
                            // extended pattern list (None for the self slot).
                            let mut defaults: Vec<Option<crate::ast::Expr>> = Vec::new();
                            if method.self_param.is_some() {
                                defaults.push(None);
                            }
                            defaults.extend(method.params.iter().map(|p| p.default_value.clone()));
                            let val = Value::Function {
                                name: method.name.clone(),
                                param_patterns: patterns,
                                param_defaults: defaults,
                                body: method.body.clone(),
                                closure_env: None,
                            };
                            self.env.define(method_key, val);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn call_function(&mut self, name: &str, args: &[Value]) -> Value {
        let func = self.env.get(name);
        match func {
            Some(Value::Function {
                param_patterns,
                body,
                closure_env,
                ..
            }) => {
                self.env.push_scope();
                if let Some(ref captured) = closure_env {
                    for (k, v) in captured {
                        self.env.define(k.clone(), v.clone());
                    }
                }
                for (i, pat) in param_patterns.iter().enumerate() {
                    if let Some(val) = args.get(i) {
                        self.bind_pattern(pat, val.clone());
                    }
                }
                let result = self.eval_block_inner(&body);
                self.env.pop_scope();
                match result {
                    Ok(v) => v,
                    Err(ControlFlow::Return(v)) => v,
                    Err(ControlFlow::Break { .. }) => {
                        unreachable!("break outside of loop; should be caught by resolver")
                    }
                    Err(ControlFlow::Continue { .. }) => {
                        unreachable!("continue outside of loop; should be caught by resolver")
                    }
                    Err(cf @ (ControlFlow::ExitUnwind { .. } | ControlFlow::RuntimeError)) => {
                        // Propagate unwind up the stack (defers already ran in eval_block_inner)
                        self.pending_cf = Some(cf);
                        Value::Unit
                    }
                }
            }
            _ => unreachable!(
                "'{}' is not a function; should be caught by typechecker",
                name
            ),
        }
    }

    // ── Expression evaluation ───────────────────────────────────

    /// Public API: evaluate an expression (panics on control flow signals).
    pub fn eval_expr(&mut self, expr: &Expr) -> Value {
        self.eval_expr_inner(expr)
    }

    fn eval_expr_inner(&mut self, expr: &Expr) -> Value {
        // If a control flow signal is pending, short-circuit
        if self.check_cf() {
            return Value::Unit;
        }
        match &expr.kind {
            // Literals
            ExprKind::Integer(i, _) => Value::Int(*i),
            ExprKind::Float(f, _) => Value::Float(*f),
            ExprKind::Bool(b) => Value::Bool(*b),
            ExprKind::CharLit(c) => Value::Char(*c),
            ExprKind::StringLit(s) => Value::String(s.clone()),
            ExprKind::MultiStringLit(s) => Value::String(s.clone()),
            ExprKind::InterpolatedStringLit(parts) => {
                let mut result = String::new();
                for part in parts {
                    match part {
                        crate::ast::ParsedInterpolationPart::Text(t) => result.push_str(t),
                        crate::ast::ParsedInterpolationPart::Expr(e) => {
                            let val = self.eval_expr_inner(e);
                            result.push_str(&format!("{}", val));
                        }
                    }
                }
                Value::String(result)
            }

            // Operators
            ExprKind::Binary { op, left, right } => {
                let l = self.eval_expr_inner(left);
                let r = self.eval_expr_inner(right);
                self.eval_binary(op, l, r, &expr.span)
            }
            ExprKind::Unary { op, operand } => {
                let val = self.eval_expr_inner(operand);
                self.eval_unary(op, val, &expr.span)
            }

            ExprKind::Identifier(name) => self.env.get(name).unwrap_or_else(|| {
                unreachable!(
                    "variable '{}' not found at {}:{}; should be caught by resolver",
                    name, expr.span.line, expr.span.column
                )
            }),

            ExprKind::Path(segments) => {
                let full = segments.join(".");
                if let Some(v) = self.env.get(&full) {
                    return v;
                }
                // Type-parameter dispatch: `T.method` where `T` is bound to a
                // concrete type at the current call frame's substitution
                // stack. Look up `<concrete>.method` instead.
                if segments.len() == 2 {
                    if let Some(concrete) = self.resolve_type_param(&segments[0]) {
                        let key = format!("{}.{}", concrete, segments[1]);
                        if let Some(v) = self.env.get(&key) {
                            return v;
                        }
                    }
                }
                // Try just the last segment (enum variant, etc.)
                let last = segments.last().cloned().unwrap_or_default();
                self.env.get(&last).unwrap_or_else(|| {
                    unreachable!(
                        "path '{}' not found at {}:{}; should be caught by resolver",
                        full, expr.span.line, expr.span.column
                    )
                })
            }

            ExprKind::SelfValue => self.env.get("self").unwrap_or_else(|| {
                unreachable!(
                    "'self' not found at {}:{}; should be caught by resolver",
                    expr.span.line, expr.span.column
                )
            }),

            ExprKind::Block(block) => match self.eval_block_inner(block) {
                Ok(v) => v,
                Err(cf) => self.set_cf(cf),
            },

            // Tuple
            ExprKind::Tuple(exprs) => {
                let vals: Vec<Value> = exprs.iter().map(|e| self.eval_expr_inner(e)).collect();
                Value::Tuple(vals)
            }

            // Array literal — synthesis mode produces Vec[T] in the type system;
            // both Array and Vec are represented as Value::Array at runtime.
            ExprKind::ArrayLiteral(elements) => {
                let vals: Vec<Value> = elements.iter().map(|e| self.eval_expr_inner(e)).collect();
                Value::Array(vals)
            }

            // Prefix collection literal: `Vec[e1, e2, ...]` / `Array[e1, ...]`
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                let vals: Vec<Value> = items.iter().map(|e| self.eval_expr_inner(e)).collect();
                Value::Array(vals)
            }

            // Repeat literal: `[v; n]` / `Vec[v; n]` / `Array[v; n]`. Value
            // is evaluated once; the resulting `n` clones share the value's
            // structure (consistent with Rust's `[v; n]` semantics).
            ExprKind::RepeatLiteral { value, count, .. } => {
                let v = self.eval_expr_inner(value);
                let n = match self.eval_expr_inner(count) {
                    Value::Int(n) if n >= 0 => n as usize,
                    _ => 0,
                };
                Value::Array(vec![v; n])
            }

            // Map literal
            ExprKind::MapLiteral(entries) => {
                let vals: Vec<(Value, Value)> = entries
                    .iter()
                    .map(|(k, v)| (self.eval_expr_inner(k), self.eval_expr_inner(v)))
                    .collect();
                Value::Map(vals)
            }

            // Struct literal
            ExprKind::StructLiteral {
                path,
                fields,
                spread,
            } => {
                let name = path.last().cloned().unwrap_or_default();
                let mut field_vals = HashMap::new();
                if let Some(ref spread_expr) = spread {
                    if let Value::Struct {
                        fields: base_fields,
                        ..
                    } = self.eval_expr_inner(spread_expr)
                    {
                        field_vals = base_fields;
                    }
                }
                for field in fields {
                    let val = self.eval_expr_inner(&field.value);
                    field_vals.insert(field.name.clone(), val);
                }
                Value::Struct {
                    name,
                    fields: field_vals,
                }
            }

            // Field access
            ExprKind::FieldAccess { object, field } => {
                let obj = self.eval_expr_inner(object);
                match obj {
                    Value::Struct { fields, .. } => {
                        fields.get(field).cloned().unwrap_or_else(|| {
                            unreachable!(
                                "field '{}' not found at {}:{}; should be caught by typechecker",
                                field, expr.span.line, expr.span.column
                            )
                        })
                    }
                    _ => unreachable!(
                        "field access on non-struct at {}:{}; should be caught by typechecker",
                        expr.span.line, expr.span.column
                    ),
                }
            }

            // Tuple index
            ExprKind::TupleIndex { object, index } => {
                let obj = self.eval_expr_inner(object);
                match obj {
                    Value::Tuple(vals) => vals.get(*index as usize).cloned().unwrap_or_else(|| {
                        unreachable!(
                            "tuple index out of bounds at {}:{}; should be caught by typechecker",
                            expr.span.line, expr.span.column
                        )
                    }),
                    _ => unreachable!(
                        "tuple index on non-tuple at {}:{}; should be caught by typechecker",
                        expr.span.line, expr.span.column
                    ),
                }
            }

            // Array/map index
            ExprKind::Index { object, index } => {
                // Range indexing: `v[a..b]` — produce a Slice[T] (interpreter
                // models this as a Value::Array copy of the sub-range; the
                // type-erased interpreter doesn't distinguish slice vs. array
                // at runtime). Mutation through a mutable slice in the
                // interpreter does not propagate back to the source — the
                // compiled codegen has full aliasing semantics.
                if let ExprKind::Range {
                    start,
                    end,
                    inclusive,
                } = &index.kind
                {
                    let obj = self.eval_expr_inner(object);
                    // Evaluate optional bounds; absent start defaults to 0,
                    // absent end is resolved after we know the array length.
                    let start_i = if let Some(s) = start {
                        match self.eval_expr_inner(s) {
                            Value::Int(n) if n >= 0 => n as usize,
                            Value::Int(n) => {
                                return self.record_runtime_error(
                                    format!("range start must be non-negative, got {}", n),
                                    &expr.span,
                                );
                            }
                            _ => unreachable!(
                                "non-int range start at {}:{}; should be caught by typechecker",
                                expr.span.line, expr.span.column
                            ),
                        }
                    } else {
                        0
                    };
                    let source = match &obj {
                        Value::Array(vals) => vals.clone(),
                        _ => unreachable!(
                            "range-indexing on non-array at {}:{}; should be caught by typechecker",
                            expr.span.line, expr.span.column
                        ),
                    };
                    let raw_end = if let Some(e) = end {
                        match self.eval_expr_inner(e) {
                            Value::Int(n) if n >= 0 => n as usize,
                            Value::Int(n) => {
                                return self.record_runtime_error(
                                    format!("range end must be non-negative, got {}", n),
                                    &expr.span,
                                );
                            }
                            _ => unreachable!(
                                "non-int range end at {}:{}; should be caught by typechecker",
                                expr.span.line, expr.span.column
                            ),
                        }
                    } else {
                        source.len()
                    };
                    let end_i = if *inclusive { raw_end + 1 } else { raw_end };
                    if start_i > end_i || end_i > source.len() {
                        return self.record_runtime_error(
                            format!(
                                "slice bounds {}..{} out of range (len {})",
                                start_i,
                                end_i,
                                source.len(),
                            ),
                            &expr.span,
                        );
                    }
                    return Value::Array(source[start_i..end_i].to_vec());
                }
                let obj = self.eval_expr_inner(object);
                let idx = self.eval_expr_inner(index);
                match (&obj, &idx) {
                    (Value::Array(vals), Value::Int(i)) => {
                        let i = *i as usize;
                        let len = vals.len();
                        vals.get(i).cloned().unwrap_or_else(|| {
                            self.record_runtime_error(
                                format!("index {} out of bounds (len {})", i, len),
                                &expr.span,
                            )
                        })
                    }
                    _ => unreachable!(
                        "non-array/non-int index at {}:{}; should be caught by typechecker",
                        expr.span.line, expr.span.column
                    ),
                }
            }

            // Function calls
            ExprKind::Call { callee, args } => self.eval_call(callee, args, &expr.span),

            // Method calls
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => self.eval_method_call(object, method, args, &expr.span),

            // If/else
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                let cond = self.eval_expr_inner(condition);
                if self.is_truthy(&cond) {
                    match self.eval_block_inner(then_block) {
                        Ok(v) => v,
                        Err(cf) => self.set_cf(cf),
                    }
                } else if let Some(ref else_expr) = else_branch {
                    self.eval_expr_inner(else_expr)
                } else {
                    Value::Unit
                }
            }

            // If-let
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                let val = self.eval_expr_inner(value);
                if self.try_match_pattern(pattern, &val) {
                    self.env.push_scope();
                    self.bind_pattern(pattern, val);
                    let result = self.eval_block_inner(then_block);
                    self.env.pop_scope();
                    match result {
                        Ok(v) => v,
                        Err(cf) => self.set_cf(cf),
                    }
                } else if let Some(ref else_expr) = else_branch {
                    self.eval_expr_inner(else_expr)
                } else {
                    Value::Unit
                }
            }

            // Match
            ExprKind::Match { scrutinee, arms } => {
                let val = self.eval_expr_inner(scrutinee);
                self.eval_match(&val, arms, &expr.span)
            }

            // While loop
            ExprKind::While {
                condition,
                body,
                label,
            } => {
                loop {
                    let cond = self.eval_expr_inner(condition);
                    if self.check_cf() || !self.is_truthy(&cond) {
                        break;
                    }
                    match self.eval_block_inner(body) {
                        Ok(_) => {}
                        Err(ControlFlow::Break {
                            label: ref bl,
                            value: ref v,
                        }) => {
                            if bl.is_none() || bl.as_deref() == label.as_deref() {
                                return v.clone().unwrap_or(Value::Unit);
                            } else {
                                return self.set_cf(ControlFlow::Break {
                                    label: bl.clone(),
                                    value: v.clone(),
                                });
                            }
                        }
                        Err(ControlFlow::Continue { label: ref cl }) => {
                            if cl.is_none() || cl.as_deref() == label.as_deref() {
                                continue;
                            } else {
                                return self.set_cf(ControlFlow::Continue { label: cl.clone() });
                            }
                        }
                        Err(cf) => return self.set_cf(cf),
                    }
                }
                Value::Unit
            }

            // For loop
            ExprKind::For {
                pattern,
                iterable,
                body,
                label,
            } => {
                let iter_val = self.eval_expr_inner(iterable);
                let items = match iter_val {
                    Value::Array(v) => v,
                    Value::Tuple(v) => v,
                    // SortedSet iterates in ascending key order
                    Value::SortedSet(s) => s.into_keys().map(|k| k.0).collect(),
                    // Set iterates in insertion order
                    Value::Set(s) => s,
                    // Map iterates as (key, value) tuples in insertion order
                    Value::Map(m) => m
                        .into_iter()
                        .map(|(k, v)| Value::Tuple(vec![k, v]))
                        .collect(),
                    // Iterator: drain via repeated `iterator_step` so adaptor
                    // closures (Map / Filter / future) fire per element. The
                    // for-loop walks the resulting Vec uniformly with the
                    // raw-collection arms above.
                    iter @ Value::Iterator { .. } => {
                        let mut it = iter;
                        let mut drained = Vec::new();
                        while let Some(v) = self.iterator_step(&mut it) {
                            drained.push(v);
                        }
                        drained
                    }
                    _ => vec![iter_val],
                };
                for item in items {
                    self.env.push_scope();
                    self.bind_pattern(pattern, item);
                    match self.eval_block_inner(body) {
                        Ok(_) => {}
                        Err(ControlFlow::Break {
                            label: ref bl,
                            value: ref v,
                        }) => {
                            self.env.pop_scope();
                            if bl.is_none() || bl.as_deref() == label.as_deref() {
                                return v.clone().unwrap_or(Value::Unit);
                            } else {
                                return self.set_cf(ControlFlow::Break {
                                    label: bl.clone(),
                                    value: v.clone(),
                                });
                            }
                        }
                        Err(ControlFlow::Continue { label: ref cl }) => {
                            self.env.pop_scope();
                            if cl.is_none() || cl.as_deref() == label.as_deref() {
                                continue;
                            } else {
                                return self.set_cf(ControlFlow::Continue { label: cl.clone() });
                            }
                        }
                        Err(cf) => {
                            self.env.pop_scope();
                            return self.set_cf(cf);
                        }
                    }
                    self.env.pop_scope();
                }
                Value::Unit
            }

            // Loop
            ExprKind::Loop { body, label } => loop {
                match self.eval_block_inner(body) {
                    Ok(_) => {}
                    Err(ControlFlow::Break {
                        label: ref bl,
                        value: ref v,
                    }) => {
                        if bl.is_none() || bl.as_deref() == label.as_deref() {
                            return v.clone().unwrap_or(Value::Unit);
                        } else {
                            return self.set_cf(ControlFlow::Break {
                                label: bl.clone(),
                                value: v.clone(),
                            });
                        }
                    }
                    Err(ControlFlow::Continue { label: ref cl }) => {
                        if cl.is_none() || cl.as_deref() == label.as_deref() {
                            continue;
                        } else {
                            return self.set_cf(ControlFlow::Continue { label: cl.clone() });
                        }
                    }
                    Err(cf) => return self.set_cf(cf),
                }
            },

            // Return
            ExprKind::Return(val) => {
                let v = val
                    .as_ref()
                    .map(|e| self.eval_expr_inner(e))
                    .unwrap_or(Value::Unit);
                self.set_cf(ControlFlow::Return(v))
            }

            // Break
            ExprKind::Break { label, value } => {
                let v = value.as_ref().map(|e| self.eval_expr_inner(e));
                self.set_cf(ControlFlow::Break {
                    label: label.clone(),
                    value: v,
                })
            }

            // Continue
            ExprKind::Continue { label } => self.set_cf(ControlFlow::Continue {
                label: label.clone(),
            }),

            // Closure
            ExprKind::Closure {
                params,
                capture_mode,
                prefix_span: _,
                body,
            } => {
                // For `mut ref |...|` closures, promote each captured outer
                // binding's slot to a `Value::SharedCell` so mutations made
                // inside the body propagate back to the outer binding and
                // are visible to subsequent invocations of the closure.
                if matches!(capture_mode, Some(CaptureMode::MutRef)) {
                    let mut bound: HashSet<String> = HashSet::new();
                    for p in params {
                        add_pattern_bindings(&p.pattern, &mut bound);
                    }
                    let mut idents: Vec<String> = Vec::new();
                    collect_free_idents_expr(body, &mut bound, &mut idents);
                    for name in idents {
                        // Skip globals (functions, enum variants, type ctors,
                        // etc.) — they live in scope[0] and never need to
                        // alias back through a cell.
                        if self
                            .env
                            .scopes
                            .first()
                            .is_some_and(|s| s.contains_key(&name))
                        {
                            continue;
                        }
                        let _ = self.env.wrap_capture(&name);
                    }
                }
                let captured = self.env.snapshot();
                let closure_body = Block {
                    stmts: Vec::new(),
                    final_expr: Some(Box::new(body.as_ref().clone())),
                    span: body.span.clone(),
                };
                Value::Function {
                    name: "<closure>".to_string(),
                    param_patterns: params.iter().map(|p| p.pattern.clone()).collect(),
                    param_defaults: params.iter().map(|_| None).collect(),
                    body: closure_body,
                    closure_env: Some(captured),
                }
            }

            // Cast
            ExprKind::Cast { expr: inner, .. } => {
                // Simplified: just evaluate the inner expression
                self.eval_expr_inner(inner)
            }

            // Range — evaluates to an Array of integers for bounded ranges,
            // or a runtime error for unbounded forms used as values.
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let s = start.as_deref().map(|e| self.eval_expr_inner(e));
                let e = end.as_deref().map(|e| self.eval_expr_inner(e));
                match (s, e) {
                    (Some(Value::Int(a)), Some(Value::Int(b))) => {
                        let items: Vec<Value> = if *inclusive {
                            (a..=b).map(Value::Int).collect()
                        } else {
                            (a..b).map(Value::Int).collect()
                        };
                        Value::Array(items)
                    }
                    (None, None) => {
                        // RangeFull used as a value — only valid as a slice index
                        self.record_runtime_error(
                            "RangeFull (..) cannot be used as a standalone value".to_string(),
                            &expr.span,
                        )
                    }
                    (Some(_), None) | (None, Some(_)) => {
                        // RangeFrom / RangeTo used as a value outside of index context
                        self.record_runtime_error(
                            "unbounded ranges cannot be used as standalone values".to_string(),
                            &expr.span,
                        )
                    }
                    _ => unreachable!(
                        "non-integer range bounds at {}:{}; should be caught by typechecker",
                        expr.span.line, expr.span.column
                    ),
                }
            }

            // Pipe
            ExprKind::Pipe { left, right } => self.eval_pipe(left, right),

            // Question mark (? operator)
            // On Err(e) → return Err(e) from enclosing function
            // On Ok(v) → unwrap to v
            // On None → return None from enclosing function
            ExprKind::Question(inner) => {
                let val = self.eval_expr_inner(inner);
                match &val {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } if variant == "Ok" => {
                        self.clear_error_trace();
                        vals.first().cloned().unwrap_or(Value::Unit)
                    }
                    Value::EnumVariant { variant, .. } if variant == "Err" => {
                        // Record trace frame before propagating
                        self.push_error_trace(expr.span.line, expr.span.column);
                        // Cross-error conversion: typechecker recorded a target
                        // type at this `?` span if `From` conversion is needed.
                        let span_key = SpanKey::from_span(&expr.span);
                        let propagated = if let Some(target) = self
                            .typecheck_result
                            .question_conversions
                            .get(&span_key)
                            .cloned()
                        {
                            let inner_err = match &val {
                                Value::EnumVariant {
                                    data: EnumData::Tuple(vs),
                                    ..
                                } => vs.first().cloned().unwrap_or(Value::Unit),
                                _ => Value::Unit,
                            };
                            let converted =
                                self.call_function(&format!("{}.from", target), &[inner_err]);
                            Value::EnumVariant {
                                enum_name: "Result".to_string(),
                                variant: "Err".to_string(),
                                data: EnumData::Tuple(vec![converted]),
                            }
                        } else {
                            val
                        };
                        // Propagate Err by returning from enclosing function
                        self.set_cf(ControlFlow::Return(propagated))
                    }
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } if variant == "Some" => {
                        self.clear_error_trace();
                        vals.first().cloned().unwrap_or(Value::Unit)
                    }
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Unit,
                        ..
                    } if variant == "None" => {
                        // Record trace frame before propagating
                        self.push_error_trace(expr.span.line, expr.span.column);
                        self.set_cf(ControlFlow::Return(val))
                    }
                    // Not a Result/Option — pass through
                    _ => val,
                }
            }

            // Optional chaining (?.)
            ExprKind::OptionalChain {
                object,
                field_or_method: field,
                args: _,
            } => {
                let obj = self.eval_expr_inner(object);
                match &obj {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Unit,
                        ..
                    } if variant == "None" => {
                        obj // propagate None
                    }
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } if variant == "Some" => {
                        let inner = vals.first().cloned().unwrap_or(Value::Unit);
                        match inner {
                            Value::Struct { fields, .. } => {
                                let val = fields.get(field).cloned().unwrap_or(Value::Unit);
                                Value::EnumVariant {
                                    enum_name: "Option".to_string(),
                                    variant: "Some".to_string(),
                                    data: EnumData::Tuple(vec![val]),
                                }
                            }
                            _ => Value::EnumVariant {
                                enum_name: "Option".to_string(),
                                variant: "None".to_string(),
                                data: EnumData::Unit,
                            },
                        }
                    }
                    _ => {
                        // Not an Option, just do field access
                        match obj {
                            Value::Struct { fields, .. } => {
                                fields.get(field).cloned().unwrap_or(Value::Unit)
                            }
                            _ => Value::Unit,
                        }
                    }
                }
            }

            // NilCoalesce (??)
            ExprKind::NilCoalesce { left, right } => {
                let l = self.eval_expr_inner(left);
                match &l {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Unit,
                        ..
                    } if variant == "None" => self.eval_expr_inner(right),
                    _ => l,
                }
            }

            ExprKind::Unsafe(block) => match self.eval_block_inner(block) {
                Ok(v) => v,
                Err(cf) => self.set_cf(cf),
            },

            ExprKind::Seq(block) => match self.eval_block_inner(block) {
                Ok(v) => v,
                Err(cf) => self.set_cf(cf),
            },

            ExprKind::Par(block) => {
                if self.sequential_mode {
                    // In sequential mode, par {} is just a regular block
                    match self.eval_block_inner(block) {
                        Ok(v) => v,
                        Err(cf) => self.set_cf(cf),
                    }
                } else {
                    match self.eval_par_block(block) {
                        Ok(v) => v,
                        Err(cf) => self.set_cf(cf),
                    }
                }
            }

            ExprKind::Providers { bindings, body } => self.eval_providers_block(bindings, body),

            ExprKind::SelfType | ExprKind::PipePlaceholder | ExprKind::Error => Value::Unit,

            _ => todo!(
                "Interpreter: unhandled expr {:?}",
                std::mem::discriminant(&expr.kind)
            ),
        }
    }

    // ── Block & Statement evaluation ──────────────────────────────

    #[allow(clippy::result_large_err)]
    fn eval_block_inner(&mut self, block: &Block) -> EvalResult {
        self.env.push_scope();
        let mut defers: Vec<Block> = Vec::new();
        let mut errdefers: Vec<Block> = Vec::new();
        let mut is_err = false;

        for stmt in &block.stmts {
            // Collect defer/errdefer blocks
            match &stmt.kind {
                StmtKind::Defer { body } => {
                    defers.push(body.clone());
                    continue;
                }
                StmtKind::ErrDefer { body, .. } => {
                    errdefers.push(body.clone());
                    continue;
                }
                _ => {}
            }
            self.eval_stmt_cf(stmt)?;
            if let Some(cf) = self.pending_cf.take() {
                is_err = matches!(&cf, ControlFlow::Return(Value::EnumVariant { variant, .. }) if variant == "Err");
                // Run defers before leaving scope
                self.run_defers(&defers, &errdefers, is_err);
                self.env.pop_scope();
                return Err(cf);
            }
        }
        let result = if let Some(ref expr) = block.final_expr {
            let v = self.eval_expr_inner(expr);
            if let Some(cf) = self.pending_cf.take() {
                is_err = matches!(&cf, ControlFlow::Return(Value::EnumVariant { variant, .. }) if variant == "Err");
                self.run_defers(&defers, &errdefers, is_err);
                self.env.pop_scope();
                return Err(cf);
            }
            v
        } else {
            Value::Unit
        };
        // Run defers on normal exit
        self.run_defers(&defers, &errdefers, is_err);
        self.env.pop_scope();
        Ok(result)
    }

    /// Execute a `par {}` block with parallel execution.
    /// Each top-level statement in the block becomes a concurrent branch.
    /// Fail-fast: first error cancels all siblings.
    #[allow(clippy::result_large_err)]
    fn eval_par_block(&mut self, block: &Block) -> EvalResult {
        let stmts = &block.stmts;

        // Single or zero statements — no parallelism needed
        if stmts.len() <= 1 {
            return self.eval_block_inner(block);
        }

        // Snapshot current environment for all branches
        let env_snapshot = self.env.snapshot();
        let cancel_flag = AtomicBool::new(false);
        let program = self.program;
        let typecheck_result = self.typecheck_result;
        let sequential_mode = self.sequential_mode;
        let source_filename = &self.source_filename;

        // Collect results from each branch
        // Each branch result: (index, defined_vars, output_lines, control_flow_or_value)
        type BranchResult = (
            usize,
            HashMap<String, Value>,
            Vec<String>,
            Result<Value, ControlFlow>,
        );
        let results: Mutex<Vec<BranchResult>> = Mutex::new(Vec::new());

        std::thread::scope(|s| {
            for (i, stmt) in stmts.iter().enumerate() {
                let env_snap = &env_snapshot;
                let cancel = &cancel_flag;
                let prog = &program;
                let tc = &typecheck_result;
                let results_ref = &results;
                let stmt_clone = stmt.clone();
                s.spawn(move || {
                    // Check cancel before starting
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }

                    // Create a branch interpreter with the shared env snapshot
                    let mut branch_interp = Interpreter::new(prog, tc);
                    branch_interp.captured_output = Some(Vec::new());
                    branch_interp.sequential_mode = sequential_mode;
                    branch_interp.source_filename = source_filename.clone();

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

                    results_ref
                        .lock()
                        .unwrap()
                        .push((i, defined_vars, output, cf_result));
                });
            }
        });

        // Sort results by source order (deterministic)
        let mut branch_results = results.into_inner().unwrap();
        branch_results.sort_by_key(|(i, _, _, _)| *i);

        // Merge results back into the parent interpreter
        // 1. Merge output in source order
        for (_, _, output, _) in &branch_results {
            for line in output {
                if let Some(ref mut cap) = self.captured_output {
                    cap.push(line.clone());
                } else {
                    print!("{}", line);
                }
            }
        }

        // 2. Merge defined variables
        self.env.push_scope();
        for (_, vars, _, _) in &branch_results {
            for (name, val) in vars {
                // Skip prelude/function definitions
                if matches!(val, Value::Function { .. } | Value::EnumVariant { .. }) {
                    continue;
                }
                self.env.define(name.clone(), val.clone());
            }
        }

        // 3. Check for errors (fail-fast: first error in source order)
        for (_, _, _, result) in branch_results {
            if let Err(cf) = result {
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

    fn run_defers(&mut self, defers: &[Block], errdefers: &[Block], is_err: bool) {
        // On error: errdefers first (LIFO), then defers (LIFO)
        // On normal: defers only (LIFO)
        if is_err {
            for block in errdefers.iter().rev() {
                let _ = self.eval_block_inner(block);
            }
        }
        for block in defers.iter().rev() {
            let _ = self.eval_block_inner(block);
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
    fn dispatch_lowered_op(
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

    /// Push a generic-param substitution frame for the call at `span` if the
    /// typechecker recorded one. Each entry is fully resolved against the
    /// current top-of-stack frame so transitive bindings (`make`'s `T → "T"`
    /// where the caller's `T` is `"Wrapper"`) flatten to a concrete type
    /// before the callee body executes. Returns true when a frame was
    /// pushed; the call site uses that to know whether to pop.
    fn push_type_subs_for_call(&mut self, span: &Span) -> bool {
        let frame = self
            .typecheck_result
            .call_type_subs
            .get(&crate::resolver::SpanKey::from_span(span));
        let frame = match frame {
            Some(f) => f,
            None => return false,
        };
        let mut resolved: HashMap<String, String> = HashMap::new();
        for (name, target) in frame {
            // Transitively resolve the target through the current top frame
            // (parent's bindings) so abstract-name propagation collapses to
            // the concrete dispatch target by the time the callee body runs.
            let mut current = target.clone();
            for _ in 0..16 {
                let next = self
                    .type_subs_stack
                    .last()
                    .and_then(|f| f.get(&current).cloned());
                match next {
                    Some(n) if n != current => current = n,
                    _ => break,
                }
            }
            resolved.insert(name.clone(), current);
        }
        self.type_subs_stack.push(resolved);
        true
    }

    /// Look up `name` through the runtime type-substitution stack from top
    /// to bottom and return the resolved concrete type name when found.
    /// Returns `None` if `name` is not bound in any visible frame.
    fn resolve_type_param(&self, name: &str) -> Option<String> {
        for frame in self.type_subs_stack.iter().rev() {
            if let Some(v) = frame.get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    fn eval_call(&mut self, callee: &Expr, args: &[CallArg], span: &Span) -> Value {
        // `with_provider[R](provider, closure)` — surface for scoped provider
        // injection (design.md § Provider-Rooted Resources). Parses today as
        // `Call(Index(Ident("with_provider"), <R>), [provider, closure])`
        // because the current parser treats `[...]` at expression position as
        // indexing; we pattern-match that shape and extract the resource name
        // from the bracket operand. A future parser slice that recognizes
        // `IDENT[TYPE_ARGS](` as a generic call will feed through the same
        // intercept via the new Call shape.
        //
        // TODO(auto-traits): the typechecker should verify `Send + Sync` on
        // the concrete provider type `P` here — deferred until Kāra's
        // auto-trait / concurrency work lands. See
        // `docs/deferred.md § Send + Sync Enforcement on with_provider
        // Concrete Provider Type`. The single-threaded tree-walk interpreter
        // has no Send/Sync failure modes to catch until then.
        if let Some((resource, provider_expr, closure_expr)) =
            Self::match_with_provider(callee, args)
        {
            return self.eval_with_provider(&resource, provider_expr, closure_expr, span);
        }

        // Effect-resource method call — `UserDB.query(...)` parses as
        // `Call(Path(["UserDB", "query"]), args)` because `starts_upper(&name)`
        // roots a Path in `parse_primary`. Dispatch through the provider
        // stack instead of normal path-call resolution when the head segment
        // names an `effect resource` (design.md § Provider-Rooted Resources).
        if let ExprKind::Path(segments) = &callee.kind {
            if segments.len() == 2 && self.effect_resources.contains(&segments[0]) {
                return self.eval_resource_method(&segments[0], &segments[1], args, span);
            }
        }

        // Built-in path-qualified functions (e.g. process.exit, Ordering.Relaxed, F64.from)
        if let ExprKind::Path(segments) = &callee.kind {
            let path_str = segments.join(".");
            match path_str.as_str() {
                "process.exit" => {
                    self.track_effect("panics");
                    let code = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::Int(v) => v as i32,
                            _ => 1,
                        }
                    } else {
                        0
                    };
                    // Run all pending defers via ExitUnwind propagation
                    self.pending_cf = Some(ControlFlow::ExitUnwind { code });
                    return Value::Unit;
                }
                "Atomic.new" => {
                    let val = if let Some(arg) = args.first() {
                        self.eval_expr_inner(&arg.value)
                    } else {
                        Value::Int(0)
                    };
                    return Value::Atomic(Box::new(val));
                }
                "Map.new" => {
                    return Value::Map(Vec::new());
                }
                "SortedSet.new" => {
                    return Value::SortedSet(BTreeMap::new());
                }
                "Set.new" => {
                    return Value::Set(Vec::new());
                }
                "Client.new" => {
                    return Value::Struct {
                        name: "Client".to_string(),
                        fields: HashMap::new(),
                    };
                }
                "Client.get" => {
                    let url = args
                        .first()
                        .map(|a| match self.eval_expr_inner(&a.value) {
                            Value::String(s) => s,
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    return eval_http_get(&url);
                }
                "Client.post" => {
                    let mut arg_iter = args.iter();
                    let url = arg_iter
                        .next()
                        .map(|a| match self.eval_expr_inner(&a.value) {
                            Value::String(s) => s,
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    let body = arg_iter
                        .next()
                        .map(|a| match self.eval_expr_inner(&a.value) {
                            Value::String(s) => s,
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    return eval_http_post(&url, &body);
                }
                "Channel.new" => {
                    let queue: Arc<Mutex<VecDeque<Value>>> = Arc::new(Mutex::new(VecDeque::new()));
                    let sender = Value::Sender(Arc::clone(&queue));
                    let receiver = Value::Receiver(queue);
                    return Value::Tuple(vec![sender, receiver]);
                }
                "F32.from" => {
                    let val = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::Float(v) => v as f32,
                            Value::Int(v) => v as f32,
                            _ => 0.0,
                        }
                    } else {
                        0.0
                    };
                    return Value::TotalFloat32(val);
                }
                "F64.from" => {
                    let val = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::Float(v) => v,
                            Value::Int(v) => v as f64,
                            _ => 0.0,
                        }
                    } else {
                        0.0
                    };
                    return Value::TotalFloat64(val);
                }
                "Regex.compile" => {
                    let pattern = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::String(s) => s,
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    match RustRegex::new(&pattern) {
                        Ok(_) => {
                            let mut fields = HashMap::new();
                            fields.insert("pattern".to_string(), Value::String(pattern));
                            let regex_val = Value::Struct {
                                name: "Regex".to_string(),
                                fields,
                            };
                            return Value::EnumVariant {
                                enum_name: "Result".to_string(),
                                variant: "Ok".to_string(),
                                data: EnumData::Tuple(vec![regex_val]),
                            };
                        }
                        Err(e) => {
                            let mut fields = HashMap::new();
                            fields.insert("message".to_string(), Value::String(e.to_string()));
                            let err_val = Value::Struct {
                                name: "RegexError".to_string(),
                                fields,
                            };
                            return Value::EnumVariant {
                                enum_name: "Result".to_string(),
                                variant: "Err".to_string(),
                                data: EnumData::Tuple(vec![err_val]),
                            };
                        }
                    }
                }
                "Stats.sum" | "Stats.prod" | "Stats.mean" | "Stats.variance" | "Stats.stddev"
                | "Stats.median" | "Stats.min" | "Stats.max" => {
                    let xs: Vec<f64> = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::Array(vs) => vs
                                .into_iter()
                                .map(|v| match v {
                                    Value::Float(f) => f,
                                    Value::Int(i) => i as f64,
                                    _ => 0.0,
                                })
                                .collect(),
                            _ => vec![],
                        }
                    } else {
                        vec![]
                    };
                    return eval_stats_fn(&path_str, &xs, span);
                }
                "Base64.encode" | "Base64.encode_url_safe" | "Hex.encode" | "Hex.encode_upper" => {
                    let bytes: Vec<u8> = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::Array(vs) => vs
                                .into_iter()
                                .map(|v| match v {
                                    Value::Int(i) => i as u8,
                                    _ => 0,
                                })
                                .collect(),
                            _ => Vec::new(),
                        }
                    } else {
                        Vec::new()
                    };
                    let s = match path_str.as_str() {
                        "Base64.encode" => base64_encode(&bytes, false),
                        "Base64.encode_url_safe" => base64_encode(&bytes, true),
                        "Hex.encode" => hex_encode(&bytes, false),
                        "Hex.encode_upper" => hex_encode(&bytes, true),
                        _ => unreachable!(),
                    };
                    return Value::String(s);
                }
                "Base64.decode" | "Hex.decode" | "Url.encode" | "Url.decode" => {
                    let s = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::String(s) => s,
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    return match path_str.as_str() {
                        "Base64.decode" => match base64_decode(&s) {
                            Ok(b) => decode_ok_bytes(b),
                            Err(m) => decode_err(m),
                        },
                        "Hex.decode" => match hex_decode(&s) {
                            Ok(b) => decode_ok_bytes(b),
                            Err(m) => decode_err(m),
                        },
                        "Url.encode" => Value::String(url_encode(&s)),
                        "Url.decode" => match url_decode(&s) {
                            Ok(out) => decode_ok_string(out),
                            Err(m) => decode_err(m),
                        },
                        _ => unreachable!(),
                    };
                }
                _ => {
                    // Check for Ordering::Variant pattern
                    if segments.len() == 2 && segments[0] == "Ordering" {
                        return Value::EnumVariant {
                            enum_name: "Ordering".to_string(),
                            variant: segments[1].clone(),
                            data: EnumData::Unit,
                        };
                    }
                    // Numeric primitive From conversion: `T.from(x)` for
                    // integer/float widening. Interpreter stores all ints as
                    // i64 and floats as f64, so widening is the identity.
                    // F32/F64 wrappers are handled by their dedicated cases above.
                    if segments.len() == 2 && segments[1] == "from" {
                        let target = segments[0].as_str();
                        if matches!(
                            target,
                            "i8" | "i16"
                                | "i32"
                                | "i64"
                                | "u8"
                                | "u16"
                                | "u32"
                                | "u64"
                                | "usize"
                                | "f32"
                                | "f64"
                        ) {
                            if let Some(arg) = args.first() {
                                return self.eval_expr_inner(&arg.value);
                            }
                        }
                    }
                    // Lowered operator dispatch: `<Primitive>.<op>(args)`
                    // synthesized by `lowering.rs`. Routes back into the
                    // interpreter's intrinsic ops by reconstructing the
                    // BinOp/UnaryOp and reusing eval_binary/eval_unary.
                    if segments.len() == 2 {
                        let target = segments[0].as_str();
                        let method = segments[1].as_str();
                        let is_primitive = matches!(
                            target,
                            "i8" | "i16"
                                | "i32"
                                | "i64"
                                | "u8"
                                | "u16"
                                | "u32"
                                | "u64"
                                | "usize"
                                | "f32"
                                | "f64"
                                | "bool"
                                | "char"
                                | "String"
                        );
                        if is_primitive {
                            if let Some(result) = self.dispatch_lowered_op(method, args, span) {
                                return result;
                            }
                        }
                    }
                }
            }
        }

        // Built-in functions
        if let ExprKind::Identifier(name) = &callee.kind {
            match name.as_str() {
                "todo" | "unreachable" => {
                    return self.eval_builtin_diverge(name, args, span);
                }
                "Some" => {
                    let val = if let Some(a) = args.first() {
                        self.eval_expr_inner(&a.value)
                    } else {
                        Value::Unit
                    };
                    return Value::EnumVariant {
                        enum_name: "Option".to_string(),
                        variant: "Some".to_string(),
                        data: EnumData::Tuple(vec![val]),
                    };
                }
                "Ok" => {
                    let val = if let Some(a) = args.first() {
                        self.eval_expr_inner(&a.value)
                    } else {
                        Value::Unit
                    };
                    return Value::EnumVariant {
                        enum_name: "Result".to_string(),
                        variant: "Ok".to_string(),
                        data: EnumData::Tuple(vec![val]),
                    };
                }
                "Err" => {
                    let val = if let Some(a) = args.first() {
                        self.eval_expr_inner(&a.value)
                    } else {
                        Value::Unit
                    };
                    return Value::EnumVariant {
                        enum_name: "Result".to_string(),
                        variant: "Err".to_string(),
                        data: EnumData::Tuple(vec![val]),
                    };
                }
                "print" | "println" => {
                    return self.eval_builtin_print(name, args);
                }
                "dbg" => {
                    return self.eval_builtin_dbg(args, span);
                }
                "assert" => {
                    return self.eval_builtin_assert(args, span);
                }
                "assert_eq" => {
                    return self.eval_builtin_assert_eq(args, span);
                }
                "assert_ne" => {
                    return self.eval_builtin_assert_ne(args, span);
                }
                _ => {}
            }
        }

        // Evaluate arguments
        let arg_vals: Vec<Value> = args
            .iter()
            .map(|a| self.eval_expr_inner(&a.value))
            .collect();

        // Check for enum variant constructor before evaluating callee
        if let ExprKind::Identifier(name) = &callee.kind {
            if self.env.get(name).is_none() {
                if let Some(enum_name) = self.find_enum_for_variant(name) {
                    return Value::EnumVariant {
                        enum_name,
                        variant: name.clone(),
                        data: EnumData::Tuple(arg_vals),
                    };
                }
            }
        }

        // Evaluate callee
        let callee_val = self.eval_expr_inner(callee);

        match callee_val {
            Value::Function {
                param_patterns,
                param_defaults,
                body,
                closure_env,
                ..
            } => {
                self.env.push_scope();
                let pushed_subs = self.push_type_subs_for_call(span);
                if let Some(ref captured) = closure_env {
                    for (k, v) in captured {
                        self.env.define(k.clone(), v.clone());
                    }
                }
                for (i, pat) in param_patterns.iter().enumerate() {
                    let val = if let Some(v) = arg_vals.get(i) {
                        v.clone()
                    } else if let Some(Some(default_expr)) = param_defaults.get(i) {
                        self.eval_expr_inner(default_expr)
                    } else {
                        continue;
                    };
                    self.bind_pattern(pat, val);
                }
                let result = self.eval_block_inner(&body);

                // CICO write-back: for each `mut`-marked call arg whose
                // value is a simple identifier, copy the callee's final
                // binding for the corresponding param back to the caller's
                // variable before the scope is popped.
                let mut writebacks: Vec<(String, Value)> = Vec::new();
                for (i, arg) in args.iter().enumerate() {
                    if !arg.mut_marker {
                        continue;
                    }
                    let caller_var = match &arg.value.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => continue,
                    };
                    if let Some(pat) = param_patterns.get(i) {
                        if let crate::ast::PatternKind::Binding(param_name) = &pat.kind {
                            if let Some(val) = self.env.get(param_name) {
                                writebacks.push((caller_var, val));
                            }
                        }
                    }
                }

                self.env.pop_scope();
                if pushed_subs {
                    self.type_subs_stack.pop();
                }

                for (caller_var, val) in writebacks {
                    self.env.set(&caller_var, val);
                }

                match result {
                    Ok(v) => v,
                    Err(ControlFlow::Return(v)) => v,
                    Err(cf) => self.set_cf(cf),
                }
            }
            _ => {
                // Try enum variant constructor by name
                let variant_name = match &callee.kind {
                    ExprKind::Identifier(n) => n.clone(),
                    ExprKind::Path(segs) => segs.last().cloned().unwrap_or_default(),
                    _ => String::new(),
                };
                if let Some(enum_name) = self.find_enum_for_variant(&variant_name) {
                    return Value::EnumVariant {
                        enum_name,
                        variant: variant_name,
                        data: EnumData::Tuple(arg_vals),
                    };
                }
                unreachable!(
                    "call target is not callable at {}:{}; should be caught by typechecker",
                    span.line, span.column
                )
            }
        }
    }

    /// Recognize the `with_provider[R](provider, closure)` call shape. Returns
    /// the resource name, the provider argument, and the closure argument if
    /// the callee is `Index(Ident("with_provider") | Path(["with_provider"]), R)`
    /// where `R` is a bare identifier or a single-segment path, and `args` has
    /// exactly two entries with no label. Anything else returns `None` so the
    /// normal call dispatch runs.
    fn match_with_provider<'e>(
        callee: &'e Expr,
        args: &'e [CallArg],
    ) -> Option<(String, &'e Expr, &'e Expr)> {
        let ExprKind::Index { object, index } = &callee.kind else {
            return None;
        };
        let is_with_provider = match &object.kind {
            ExprKind::Identifier(n) => n == "with_provider",
            ExprKind::Path(segs) => segs.as_slice() == ["with_provider"],
            _ => false,
        };
        if !is_with_provider {
            return None;
        }
        let resource = match &index.kind {
            ExprKind::Identifier(n) => n.clone(),
            ExprKind::Path(segs) => segs.last().cloned()?,
            _ => return None,
        };
        if args.len() != 2 {
            return None;
        }
        Some((resource, &args[0].value, &args[1].value))
    }

    /// Execute `with_provider[R](provider, closure)`. Evaluates `provider`,
    /// pushes a frame binding `R` to the (`Arc`-wrapped) provider value,
    /// evaluates `closure` (must produce a callable `Value::Function`), invokes
    /// it with no arguments, then pops the frame on any exit path — including
    /// panics, `?` propagation, `ExitUnwind`, and runtime errors — so a test
    /// that fails mid-closure can't leak a provider binding into the next
    /// test. The returned value is whatever the closure produced.
    fn eval_with_provider(
        &mut self,
        resource: &str,
        provider_expr: &Expr,
        closure_expr: &Expr,
        span: &Span,
    ) -> Value {
        let provider = self.eval_expr_inner(provider_expr);
        if self.check_cf() {
            return Value::Unit;
        }

        self.push_provider_frame();
        self.bind_provider(resource.to_string(), provider);

        let closure = self.eval_expr_inner(closure_expr);
        if self.check_cf() {
            self.pop_provider_frame();
            return Value::Unit;
        }

        let result = self.invoke_zero_arg_closure(closure, span);
        self.pop_provider_frame();
        result
    }

    /// Execute a `providers { R => e, ... } in { body }` block.
    /// Evaluate-all-then-scope per design.md: every provider expression runs
    /// *before* any frame is pushed, so a failure in a later expression leaves
    /// no scopes to unwind. One frame is pushed per binding, matching the
    /// nested `with_provider` desugaring so future escape-check machinery can
    /// attribute captures to specific resources. Frames are popped on every
    /// exit path (normal return, `?`, panic, `ExitUnwind`, runtime error) so
    /// bindings cannot leak past the block.
    fn eval_providers_block(&mut self, bindings: &[ProviderBinding], body: &Block) -> Value {
        // Phase 1: evaluate all provider expressions. Stop on the first cf.
        let mut values: Vec<(String, Value)> = Vec::with_capacity(bindings.len());
        for b in bindings {
            let v = self.eval_expr_inner(&b.value);
            if self.check_cf() {
                return Value::Unit;
            }
            values.push((b.resource.clone(), v));
        }

        // Phase 2: push one frame per binding (outer-to-inner source order)
        // and bind each provider.
        let frames_pushed = values.len();
        for (resource, provider) in values {
            self.push_provider_frame();
            self.bind_provider(resource, provider);
        }

        // Phase 3: evaluate the body; value is the block's value.
        let result = match self.eval_block_inner(body) {
            Ok(v) => v,
            Err(ControlFlow::Return(v)) => v,
            Err(cf) => self.set_cf(cf),
        };

        // Phase 4: pop every frame we pushed — even on an error/unwind path.
        for _ in 0..frames_pushed {
            self.pop_provider_frame();
        }
        result
    }

    /// Invoke a `Value::Function` closure taking no arguments. Used by
    /// `with_provider` to run the body closure; factored out so future
    /// fixtures (`providers { }`, multi-attribute test wrapping) can reuse the
    /// invocation path without duplicating frame-management boilerplate.
    fn invoke_zero_arg_closure(&mut self, callee_val: Value, span: &Span) -> Value {
        match callee_val {
            Value::Function {
                body, closure_env, ..
            } => {
                self.env.push_scope();
                if let Some(ref captured) = closure_env {
                    for (k, v) in captured {
                        self.env.define(k.clone(), v.clone());
                    }
                }
                let result = self.eval_block_inner(&body);
                self.env.pop_scope();
                match result {
                    Ok(v) => v,
                    Err(ControlFlow::Return(v)) => v,
                    Err(cf) => self.set_cf(cf),
                }
            }
            _ => unreachable!(
                "with_provider closure at {}:{} is not a Value::Function; \
                 should be caught by typechecker",
                span.line, span.column
            ),
        }
    }

    /// Pull the next element from a `Value::Iterator`, applying its lazy
    /// adaptor chain (`Map` / `Filter` / future). Returns `None` when
    /// exhausted; callers are responsible for any state-write-back of
    /// the modified iterator value to their bindings.
    ///
    /// `Filter` may reject items, so the body loops until either an item
    /// passes every step or the source runs out. The adaptor closures
    /// can mutate captured outer bindings (via `mut ref` capture); the
    /// iterator's own state (items / cursor / steps) is parameter data,
    /// not on `self`, so the borrow checker tolerates the nested call.
    fn iterator_step(&mut self, iter: &mut Value) -> Option<Value> {
        // Snapshot the step chain once so the per-element loop doesn't
        // hold a borrow on `*iter` across `invoke_function_value` calls.
        let steps = match iter {
            Value::Iterator { steps, .. } => steps.clone(),
            _ => return None,
        };
        loop {
            let raw_item = {
                let Value::Iterator { items, cursor, .. } = iter else {
                    return None;
                };
                if *cursor >= items.len() {
                    return None;
                }
                let it = items[*cursor].clone();
                *cursor += 1;
                it
            };
            let mut item = raw_item;
            let mut keep = true;
            for step in &steps {
                match step {
                    IteratorStep::Map(f) => {
                        item = self.invoke_function_value(f.clone(), vec![item]);
                    }
                    IteratorStep::Filter(pred) => {
                        let result = self.invoke_function_value(pred.clone(), vec![item.clone()]);
                        if !matches!(result, Value::Bool(true)) {
                            keep = false;
                            break;
                        }
                    }
                }
            }
            if keep {
                return Some(item);
            }
        }
    }

    /// Invoke a `Value::Function` (closure or named function) with
    /// pre-evaluated argument values. Used by iterator adaptors that
    /// receive a closure as an already-evaluated value rather than via the
    /// AST path `eval_call` takes (no CICO write-back, no default-value
    /// evaluation, no type-substitution stack — the closure is fully
    /// monomorphic by the time it reaches an adaptor step).
    fn invoke_function_value(&mut self, callee: Value, arg_vals: Vec<Value>) -> Value {
        let Value::Function {
            param_patterns,
            body,
            closure_env,
            ..
        } = callee
        else {
            return Value::Unit;
        };
        self.env.push_scope();
        if let Some(captured) = closure_env {
            for (k, v) in captured {
                self.env.define(k, v);
            }
        }
        for (i, pat) in param_patterns.iter().enumerate() {
            if let Some(v) = arg_vals.get(i) {
                self.bind_pattern(pat, v.clone());
            }
        }
        let result = self.eval_block_inner(&body);
        self.env.pop_scope();
        match result {
            Ok(v) => v,
            Err(ControlFlow::Return(v)) => v,
            Err(cf) => self.set_cf(cf),
        }
    }

    fn eval_method_call(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Value {
        // Type-receiver associated calls: `T.method(...)` where `T` is a
        // primitive type name. The receiver is an identifier naming a type
        // — not a value — so eval_expr_inner would panic. Handle two shapes:
        //   (a) `.from(x)` — numeric widening (identity at interpreter layer)
        //   (b) operator methods (add/sub/lt/eq/bitand/not/…) — delegate to
        //       the same dispatch used for the lowered `Call(Path)` form.
        if let ExprKind::Identifier(type_name) = &object.kind {
            let target = type_name.as_str();
            let is_primitive = matches!(
                target,
                "i8" | "i16"
                    | "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "usize"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "char"
                    | "String"
            );
            if is_primitive {
                if method == "from" {
                    if let Some(arg) = args.first() {
                        return self.eval_expr_inner(&arg.value);
                    }
                }
                if let Some(result) = self.dispatch_lowered_op(method, args, span) {
                    return result;
                }
            }

            // Lowercase stdlib module aliases: `env.args()`, `env.var(name)`.
            // Map to the capitalized effect resource name so the provider
            // stack lookup in `eval_resource_method` finds the right binding.
            let resource_alias = match type_name.as_str() {
                "env" => Some("Env"),
                _ => None,
            };
            if let Some(resource) = resource_alias {
                return self.eval_resource_method(resource, method, args, span);
            }

            // Effect-resource receiver: `UserDB.query(...)` resolves through
            // the top-of-stack provider binding for `UserDB` (design.md §
            // Provider-Rooted Resources > Runtime mechanics). `UserDB` is
            // not a value — it's a tracked identity — so we skip
            // `eval_expr_inner(object)` on this path and dispatch directly
            // on the provider instance stored in `provider_stack`.
            if self.effect_resources.contains(type_name) {
                return self.eval_resource_method(type_name, method, args, span);
            }
        }

        let obj = self.eval_expr_inner(object);

        // `#[derive(Display)]` — `to_string()` on a unit enum variant.
        if method == "to_string" {
            if let Value::EnumVariant {
                enum_name,
                variant,
                data: EnumData::Unit,
            } = &obj
            {
                let has_display = self
                    .typecheck_result
                    .enum_info
                    .get(enum_name.as_str())
                    .map(|info| info.derived_traits.contains("Display"))
                    .unwrap_or(false);
                if has_display {
                    let s = if self
                        .typecheck_result
                        .display_snake_case_enums
                        .contains(enum_name.as_str())
                    {
                        pascal_to_snake(variant)
                    } else {
                        variant.clone()
                    };
                    return Value::String(s);
                }
            }
            // All other Display-able values: delegate to Value::fmt
            return Value::String(format!("{}", obj));
        }

        // Built-in methods
        match method {
            "unwrap" => {
                return match &obj {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } if variant == "Ok" || variant == "Some" => {
                        vals.first().cloned().unwrap_or(Value::Unit)
                    }
                    Value::EnumVariant { variant, .. } if variant == "Err" || variant == "None" => {
                        return self
                            .record_runtime_error(format!("called unwrap() on {}", variant), span);
                    }
                    other => other.clone(),
                };
            }
            "expect" => {
                let msg = if let Some(arg) = args.first() {
                    match self.eval_expr_inner(&arg.value) {
                        Value::String(s) => s,
                        v => format!("{}", v),
                    }
                } else {
                    String::new()
                };
                return match &obj {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } if variant == "Ok" || variant == "Some" => {
                        vals.first().cloned().unwrap_or(Value::Unit)
                    }
                    Value::EnumVariant { variant, .. } if variant == "Err" || variant == "None" => {
                        return self.record_runtime_error(
                            if msg.is_empty() {
                                format!("expect() called on {}", variant)
                            } else {
                                format!("{}: {}", msg, variant)
                            },
                            span,
                        );
                    }
                    other => other.clone(),
                };
            }
            "len" => {
                return match &obj {
                    Value::Array(v) => Value::Int(v.len() as i64),
                    Value::String(s) => Value::Int(s.len() as i64),
                    Value::Map(m) => Value::Int(m.len() as i64),
                    Value::SortedSet(s) => Value::Int(s.len() as i64),
                    Value::Set(s) => Value::Int(s.len() as i64),
                    // Note: Map also handled via Map.len() match above
                    _ => unreachable!(
                        "len() on unsupported type at {}:{}; should be caught by typechecker",
                        span.line, span.column
                    ),
                };
            }
            "iter" | "into_iter" => {
                // Snapshot the source elements eagerly into a Value::Iterator.
                // Map yields (k, v) tuples; SortedSet flattens to ascending
                // order; Set/Array yield elements in storage order. The
                // tree-walk interpreter is type-erased so iter() and
                // into_iter() are identical at this layer — the design.md
                // borrow-vs-consume distinction is a typechecker concern.
                let items = match &obj {
                    Value::Array(v) => v.clone(),
                    Value::Set(s) => s.clone(),
                    Value::SortedSet(s) => s.keys().map(|k| k.0.clone()).collect(),
                    Value::Map(m) => m
                        .iter()
                        .map(|(k, v)| Value::Tuple(vec![k.clone(), v.clone()]))
                        .collect(),
                    _ => unreachable!(
                        "{}() on unsupported type at {}:{}; should be caught by typechecker",
                        method, span.line, span.column,
                    ),
                };
                return Value::Iterator {
                    items,
                    cursor: 0,
                    steps: Vec::new(),
                };
            }
            "next" => {
                // `Iterator.next()` — pull the next item via `iterator_step`,
                // applying any adaptor closures registered in `steps`. When
                // the receiver is a binding, write the advanced state back
                // so subsequent calls see it. The `matches!` guard borrows
                // `obj` so the fall-through path (defensive — typechecker
                // should reject non-Iterator receivers) can keep using it.
                if matches!(obj, Value::Iterator { .. }) {
                    let mut iter_val = obj;
                    let yielded = self.iterator_step(&mut iter_val);
                    let result = match yielded {
                        Some(val) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![val]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, iter_val);
                    }
                    return result;
                }
            }
            "map" | "filter" => {
                // Lazy adaptors — append a `MapStep(closure)` /
                // `FilterStep(closure)` to the iterator's adaptor chain.
                // The closure is evaluated to a Value::Function once at
                // construction; per-element invocation happens at next()
                // time via `iterator_step`. Per design.md § Iterator
                // Adaptors, transformations are lazy — only terminal ops
                // drive iteration.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            format!("Iterator.{}() requires a closure argument", method),
                            span,
                        );
                    };
                    let closure = self.eval_expr_inner(&arg.value);
                    if !matches!(closure, Value::Function { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.{}() expects a closure; got {}", method, closure),
                            span,
                        );
                    }
                    let Value::Iterator {
                        items,
                        cursor,
                        mut steps,
                    } = obj
                    else {
                        unreachable!()
                    };
                    steps.push(match method {
                        "map" => IteratorStep::Map(closure),
                        "filter" => IteratorStep::Filter(closure),
                        _ => unreachable!(),
                    });
                    return Value::Iterator {
                        items,
                        cursor,
                        steps,
                    };
                }
            }
            "count" => {
                // Terminal — drain the iterator (firing all adaptor
                // closures) and count yielded elements.
                if matches!(obj, Value::Iterator { .. }) {
                    let mut iter_val = obj;
                    let mut n: i64 = 0;
                    while self.iterator_step(&mut iter_val).is_some() {
                        n += 1;
                    }
                    return Value::Int(n);
                }
            }
            "collect" => {
                // Terminal v1 — drain the iterator into a Vec[T]
                // (Value::Array). FromIterator-driven dispatch into other
                // collections is a follow-up CR.
                if matches!(obj, Value::Iterator { .. }) {
                    let mut iter_val = obj;
                    let mut out = Vec::new();
                    while let Some(v) = self.iterator_step(&mut iter_val) {
                        out.push(v);
                    }
                    return Value::Array(out);
                }
            }
            "fold" => {
                // Terminal — `fold(init, f)`. Walk via repeated
                // iterator_step pulls, threading the accumulator through
                // the closure on each step.
                if matches!(obj, Value::Iterator { .. }) {
                    if args.len() != 2 {
                        return self.record_runtime_error(
                            format!("Iterator.fold() expects 2 arguments, got {}", args.len()),
                            span,
                        );
                    }
                    let mut acc = self.eval_expr_inner(&args[0].value);
                    let f = self.eval_expr_inner(&args[1].value);
                    if !matches!(f, Value::Function { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.fold() expects a closure; got {}", f),
                            span,
                        );
                    }
                    let mut iter_val = obj;
                    while let Some(item) = self.iterator_step(&mut iter_val) {
                        acc = self.invoke_function_value(f.clone(), vec![acc, item]);
                    }
                    return acc;
                }
            }
            "any" | "all" => {
                // Short-circuit terminals. `any(pred)` returns true the
                // first time `pred` returns true; `all(pred)` returns
                // false the first time `pred` returns false. Both walk
                // the iterator via iterator_step — the loop bails the
                // moment the answer is decided, so upstream adaptor
                // closures only fire for as many elements as it takes.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            format!("Iterator.{}() requires a closure argument", method),
                            span,
                        );
                    };
                    let pred = self.eval_expr_inner(&arg.value);
                    if !matches!(pred, Value::Function { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.{}() expects a closure; got {}", method, pred),
                            span,
                        );
                    }
                    let want_any = method == "any";
                    let mut iter_val = obj;
                    while let Some(item) = self.iterator_step(&mut iter_val) {
                        let result = self.invoke_function_value(pred.clone(), vec![item]);
                        let truthy = matches!(result, Value::Bool(true));
                        if want_any && truthy {
                            return Value::Bool(true);
                        }
                        if !want_any && !truthy {
                            return Value::Bool(false);
                        }
                    }
                    // Source exhausted with no decisive answer — any
                    // returns false (no element matched), all returns
                    // true (every element matched / source was empty).
                    return Value::Bool(!want_any);
                }
            }
            "as_slice" | "as_slice_mut" => {
                // The tree-walk interpreter is type-erased; Slice[T] uses
                // the same Value::Array representation as Vec/Array. Clone
                // the backing data so the slice binding doesn't share
                // storage with the source (matches the interpreter's
                // broader value-semantics stance). Mutation through a
                // mutable slice consequently does not propagate back —
                // the compiled codegen has full aliasing semantics.
                return match &obj {
                    Value::Array(v) => Value::Array(v.clone()),
                    _ => unreachable!(
                        "{}() on unsupported type at {}:{}; should be caught by typechecker",
                        method, span.line, span.column
                    ),
                };
            }
            "push" => {
                if let Value::Array(mut arr) = obj {
                    let val = if let Some(arg) = args.first() {
                        self.eval_expr_inner(&arg.value)
                    } else {
                        Value::Unit
                    };
                    arr.push(val);
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Array(arr));
                    }
                    return Value::Unit;
                }
            }
            "is_some" => {
                return match &obj {
                    Value::EnumVariant { variant, .. } if variant == "Some" => Value::Bool(true),
                    Value::EnumVariant { variant, .. } if variant == "None" => Value::Bool(false),
                    _ => Value::Bool(true),
                };
            }
            "is_none" => {
                return match &obj {
                    Value::EnumVariant { variant, .. } if variant == "None" => Value::Bool(true),
                    _ => Value::Bool(false),
                };
            }
            "is_ok" => {
                return match &obj {
                    Value::EnumVariant { variant, .. } if variant == "Ok" => Value::Bool(true),
                    _ => Value::Bool(false),
                };
            }
            "is_err" => {
                return match &obj {
                    Value::EnumVariant { variant, .. } if variant == "Err" => Value::Bool(true),
                    _ => Value::Bool(false),
                };
            }
            // Atomic[T] methods
            "load" => {
                if let Value::Atomic(inner) = &obj {
                    // Ordering argument accepted but ignored (no concurrency in tree-walk interpreter)
                    return *inner.clone();
                }
            }
            "store" => {
                if let Value::Atomic(_) = &obj {
                    let val = if let Some(arg) = args.first() {
                        self.eval_expr_inner(&arg.value)
                    } else {
                        Value::Unit
                    };
                    // Update the atomic in the environment
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Atomic(Box::new(val)));
                    }
                    return Value::Unit;
                }
            }
            // ── Slice[T] / Vec[T] / Array[T,N] shared read-only methods ──────────
            // The interpreter uses Value::Array for all sequence types (Vec,
            // Array, Slice). Each arm only returns when `obj` IS a
            // Value::Array; otherwise it falls through to the impl-block
            // lookup so user-defined structs with the same method name
            // (`struct Counter { fn get(self) ... }`) still resolve correctly.
            "is_empty" => {
                if let Value::Array(ref v) = obj {
                    return Value::Bool(v.is_empty());
                }
                if let Value::String(ref s) = obj {
                    return Value::Bool(s.is_empty());
                }
                if let Value::SortedSet(ref s) = obj {
                    return Value::Bool(s.is_empty());
                }
                if let Value::Set(ref s) = obj {
                    return Value::Bool(s.is_empty());
                }
                if let Value::Map(ref m) = obj {
                    return Value::Bool(m.is_empty());
                }
            }
            "first" => {
                if let Value::Array(ref v) = obj {
                    return if let Some(first) = v.first() {
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![first.clone()]),
                        }
                    } else {
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        }
                    };
                }
            }
            "last" => {
                if let Value::Array(ref v) = obj {
                    return if let Some(last) = v.last() {
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![last.clone()]),
                        }
                    } else {
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        }
                    };
                }
            }
            "get" => {
                if let Value::Array(ref v) = obj {
                    let idx = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(0));
                    return if let Value::Int(i) = idx {
                        let i = i as usize;
                        if i < v.len() {
                            Value::EnumVariant {
                                enum_name: "Option".to_string(),
                                variant: "Some".to_string(),
                                data: EnumData::Tuple(vec![v[i].clone()]),
                            }
                        } else {
                            Value::EnumVariant {
                                enum_name: "Option".to_string(),
                                variant: "None".to_string(),
                                data: EnumData::Unit,
                            }
                        }
                    } else {
                        Value::Unit
                    };
                }
                if let Value::Map(ref m) = obj {
                    let key = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return match m.iter().find(|(k, _)| *k == key) {
                        Some((_, v)) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![v.clone()]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
                if let Value::Struct { ref name, .. } = obj {
                    if name == "Client" {
                        let url = args
                            .first()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        return eval_http_get(&url);
                    }
                }
            }
            "contains" => {
                if let Value::Array(ref v) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(v.contains(&needle));
                }
                if let Value::String(ref s) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    if let Value::String(sub) = needle {
                        return Value::Bool(s.contains(sub.as_str()));
                    }
                    return Value::Bool(false);
                }
                if let Value::SortedSet(ref s) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(s.contains_key(&OrdValue(needle)));
                }
                if let Value::Set(ref s) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(s.contains(&needle));
                }
            }
            "contains_key" => {
                if let Value::Map(ref m) = obj {
                    let key = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(m.iter().any(|(k, _)| *k == key));
                }
            }
            "binary_search" => {
                if let Value::Array(ref v) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return match v.binary_search_by(|probe| value_compare(probe, &needle)) {
                        Ok(i) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![Value::Int(i as i64)]),
                        },
                        Err(_) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
            }
            "split_at" => {
                if let Value::Array(ref v) = obj {
                    let idx = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(0));
                    return if let Value::Int(i) = idx {
                        let i = (i as usize).min(v.len());
                        let left = Value::Array(v[..i].to_vec());
                        let right = Value::Array(v[i..].to_vec());
                        Value::Tuple(vec![left, right])
                    } else {
                        Value::Unit
                    };
                }
            }
            "chunks" => {
                if let Value::Array(ref v) = obj {
                    let n = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(1));
                    if let Value::Int(n) = n {
                        let n = if n > 0 { n as usize } else { 1 };
                        let chunks: Vec<Value> =
                            v.chunks(n).map(|c| Value::Array(c.to_vec())).collect();
                        return Value::Array(chunks);
                    }
                }
            }
            "windows" => {
                if let Value::Array(ref v) = obj {
                    let n = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(1));
                    if let Value::Int(n) = n {
                        let n = if n > 0 && (n as usize) <= v.len() {
                            n as usize
                        } else {
                            return Value::Array(vec![]);
                        };
                        let wins: Vec<Value> =
                            v.windows(n).map(|w| Value::Array(w.to_vec())).collect();
                        return Value::Array(wins);
                    }
                }
            }
            "sort" => {
                if let Value::Array(mut arr) = obj {
                    arr.sort_by(value_compare);
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Array(arr));
                    }
                    return Value::Unit;
                }
            }
            "sort_by" => {
                // sort_by(|a, b| ...) — interpreter uses natural value ordering
                // as a fallback since closure invocation inside a comparator
                // requires re-entrancy unsupported at this call site.
                if let Value::Array(mut arr) = obj {
                    arr.sort_by(value_compare);
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Array(arr));
                    }
                    return Value::Unit;
                }
            }
            "sorted" => {
                if let Value::String(ref s) = obj {
                    let mut chars: Vec<char> = s.chars().collect();
                    chars.sort_unstable();
                    return Value::String(chars.into_iter().collect());
                }
                if let Value::Array(mut arr) = obj {
                    arr.sort_by(value_compare);
                    return Value::Array(arr);
                }
            }
            "sorted_by" => {
                // Closure comparators require re-entrancy not yet supported;
                // fall back to natural ordering for both strings and arrays.
                if let Value::String(ref s) = obj {
                    let mut chars: Vec<char> = s.chars().collect();
                    chars.sort_unstable();
                    return Value::String(chars.into_iter().collect());
                }
                if let Value::Array(mut arr) = obj {
                    arr.sort_by(value_compare);
                    return Value::Array(arr);
                }
            }
            "reverse" => {
                if let Value::Array(mut arr) = obj {
                    arr.reverse();
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Array(arr));
                    }
                    return Value::Unit;
                }
            }
            "fill" => {
                let fill_val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Array(mut arr) = obj {
                    for elem in arr.iter_mut() {
                        *elem = fill_val.clone();
                    }
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Array(arr));
                    }
                    return Value::Unit;
                }
            }
            "swap" => {
                let i = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                let j = args
                    .get(1)
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                if let (Value::Int(i_val), Value::Int(j_val)) = (i, j) {
                    if let Value::Array(mut arr) = obj {
                        let i = i_val as usize;
                        let j = j_val as usize;
                        if i < arr.len() && j < arr.len() {
                            arr.swap(i, j);
                        }
                        if let ExprKind::Identifier(name) = &object.kind {
                            self.env.set(name, Value::Array(arr));
                        }
                        return Value::Unit;
                    }
                } else {
                    // consume obj to avoid borrow-after-move
                    let _ = obj;
                }
            }
            // ── Channel[T] / Sender[T] / Receiver[T] methods ──────────────
            "send" => {
                let val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Sender(ref queue) = obj {
                    queue.lock().unwrap().push_back(val);
                    return Value::Unit;
                }
            }
            "recv" => {
                if let Value::Receiver(ref queue) = obj {
                    // In the tree-walk interpreter tests the sender always
                    // fires before recv, so the queue has an item. If empty
                    // (would deadlock in a real runtime) return Unit rather
                    // than blocking the interpreter thread forever.
                    let val = queue.lock().unwrap().pop_front().unwrap_or(Value::Unit);
                    return val;
                }
            }
            "try_recv" => {
                if let Value::Receiver(ref queue) = obj {
                    let opt = queue.lock().unwrap().pop_front();
                    return match opt {
                        Some(v) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![v]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
            }
            // clone() on Sender creates an additional producer sharing the
            // same queue Arc. Conflicts with Array.clone / general clone —
            // gate on Value::Sender explicitly so other types fall through.
            "clone" => {
                if let Value::Sender(ref queue) = obj {
                    return Value::Sender(Arc::clone(queue));
                }
            }

            // ── Map[K, V] methods ─────────────────────────────────────────
            "get_or" => {
                if let Value::Map(ref m) = obj {
                    let key = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    let default = args
                        .get(1)
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return match m.iter().find(|(k, _)| *k == key) {
                        Some((_, v)) => v.clone(),
                        None => default,
                    };
                }
            }
            "keys" => {
                if let Value::Map(ref m) = obj {
                    return Value::Array(m.iter().map(|(k, _)| k.clone()).collect());
                }
            }
            "values" => {
                if let Value::Map(ref m) = obj {
                    return Value::Array(m.iter().map(|(_, v)| v.clone()).collect());
                }
            }
            "entries" => {
                if let Value::Map(ref m) = obj {
                    return Value::Array(
                        m.iter()
                            .map(|(k, v)| Value::Tuple(vec![k.clone(), v.clone()]))
                            .collect(),
                    );
                }
            }
            "merge" => {
                if let Value::Map(ref base) = obj {
                    let other = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Map(Vec::new()));
                    if let Value::Map(other_entries) = other {
                        let mut result = base.clone();
                        for (k, v) in other_entries {
                            if let Some(entry) = result.iter_mut().find(|(ek, _)| *ek == k) {
                                entry.1 = v;
                            } else {
                                result.push((k, v));
                            }
                        }
                        return Value::Map(result);
                    }
                }
            }

            // ── SortedSet[T: Ord] methods ──────────────────────────────────
            "insert" => {
                let val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Map(mut m) = obj {
                    // Map.insert(key, value) -> Option[V] (old value)
                    let value = args
                        .get(1)
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    let old = if let Some(entry) = m.iter_mut().find(|(k, _)| *k == val) {
                        let prev = entry.1.clone();
                        entry.1 = value;
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![prev]),
                        }
                    } else {
                        m.push((val, value));
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        }
                    };
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Map(m));
                    }
                    return old;
                }
                if let Value::SortedSet(mut set) = obj {
                    let was_absent = set.insert(OrdValue(val), ()).is_none();
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::SortedSet(set));
                    }
                    return Value::Bool(was_absent);
                }
                if let Value::Set(mut set) = obj {
                    let was_absent = !set.contains(&val);
                    if was_absent {
                        set.push(val);
                    }
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Set(set));
                    }
                    return Value::Bool(was_absent);
                }
            }
            "remove" => {
                let val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Map(mut m) = obj {
                    let old = if let Some(pos) = m.iter().position(|(k, _)| *k == val) {
                        let (_, v) = m.remove(pos);
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![v]),
                        }
                    } else {
                        Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        }
                    };
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Map(m));
                    }
                    return old;
                }
                if let Value::SortedSet(mut set) = obj {
                    let was_present = set.remove(&OrdValue(val)).is_some();
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::SortedSet(set));
                    }
                    return Value::Bool(was_present);
                }
                if let Value::Set(mut set) = obj {
                    let was_present = if let Some(pos) = set.iter().position(|x| *x == val) {
                        set.swap_remove(pos);
                        true
                    } else {
                        false
                    };
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Set(set));
                    }
                    return Value::Bool(was_present);
                }
            }
            "clear" => {
                if let Value::Map(_) = obj {
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Map(Vec::new()));
                    }
                    return Value::Unit;
                }
            }
            "min" => {
                if let Value::SortedSet(ref set) = obj {
                    return match set.keys().next() {
                        Some(k) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![k.0.clone()]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
            }
            "max" => {
                if let Value::SortedSet(ref set) = obj {
                    return match set.keys().next_back() {
                        Some(k) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![k.0.clone()]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
            }
            "union" => {
                let other = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let (Value::SortedSet(ref a_set), Value::SortedSet(ref b_set)) = (&obj, &other) {
                    #[allow(clippy::mutable_key_type)]
                    let mut result = a_set.clone();
                    for (k, _v) in b_set.iter() {
                        result.insert(k.clone(), ());
                    }
                    return Value::SortedSet(result);
                }
                if let (Value::Set(ref a_set), Value::Set(ref b_set)) = (&obj, &other) {
                    let mut result = a_set.clone();
                    for v in b_set {
                        if !result.contains(v) {
                            result.push(v.clone());
                        }
                    }
                    return Value::Set(result);
                }
            }
            "intersection" => {
                let other = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let (Value::SortedSet(ref a_set), Value::SortedSet(ref b_set)) = (&obj, &other) {
                    #[allow(clippy::mutable_key_type)]
                    let result: BTreeMap<OrdValue, ()> = a_set
                        .iter()
                        .filter(|(k, _)| b_set.contains_key(*k))
                        .map(|(k, v)| (k.clone(), *v))
                        .collect();
                    return Value::SortedSet(result);
                }
                if let (Value::Set(ref a_set), Value::Set(ref b_set)) = (&obj, &other) {
                    let result: Vec<Value> = a_set
                        .iter()
                        .filter(|v| b_set.contains(v))
                        .cloned()
                        .collect();
                    return Value::Set(result);
                }
            }
            "difference" => {
                let other = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let (Value::SortedSet(ref a_set), Value::SortedSet(ref b_set)) = (&obj, &other) {
                    #[allow(clippy::mutable_key_type)]
                    let result: BTreeMap<OrdValue, ()> = a_set
                        .iter()
                        .filter(|(k, _)| !b_set.contains_key(*k))
                        .map(|(k, v)| (k.clone(), *v))
                        .collect();
                    return Value::SortedSet(result);
                }
                if let (Value::Set(ref a_set), Value::Set(ref b_set)) = (&obj, &other) {
                    let result: Vec<Value> = a_set
                        .iter()
                        .filter(|v| !b_set.contains(v))
                        .cloned()
                        .collect();
                    return Value::Set(result);
                }
            }
            "is_match" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Regex" {
                        if let Some(Value::String(ref pattern)) = fields.get("pattern") {
                            if let Ok(rx) = RustRegex::new(pattern) {
                                let haystack = args
                                    .first()
                                    .map(|a| match self.eval_expr_inner(&a.value) {
                                        Value::String(s) => s,
                                        _ => String::new(),
                                    })
                                    .unwrap_or_default();
                                return Value::Bool(rx.is_match(&haystack));
                            }
                        }
                    }
                }
            }
            "find" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Regex" {
                        if let Some(Value::String(ref pattern)) = fields.get("pattern") {
                            if let Ok(rx) = RustRegex::new(pattern) {
                                let haystack = args
                                    .first()
                                    .map(|a| match self.eval_expr_inner(&a.value) {
                                        Value::String(s) => s,
                                        _ => String::new(),
                                    })
                                    .unwrap_or_default();
                                return match rx.find(&haystack) {
                                    Some(m) => {
                                        let mut mf = HashMap::new();
                                        mf.insert(
                                            "text".to_string(),
                                            Value::String(m.as_str().to_string()),
                                        );
                                        mf.insert(
                                            "start".to_string(),
                                            Value::Int(m.start() as i64),
                                        );
                                        mf.insert("end".to_string(), Value::Int(m.end() as i64));
                                        Value::EnumVariant {
                                            enum_name: "Option".to_string(),
                                            variant: "Some".to_string(),
                                            data: EnumData::Tuple(vec![Value::Struct {
                                                name: "Match".to_string(),
                                                fields: mf,
                                            }]),
                                        }
                                    }
                                    None => Value::EnumVariant {
                                        enum_name: "Option".to_string(),
                                        variant: "None".to_string(),
                                        data: EnumData::Unit,
                                    },
                                };
                            }
                        }
                    }
                }
            }
            "find_all" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Regex" {
                        if let Some(Value::String(ref pattern)) = fields.get("pattern") {
                            if let Ok(rx) = RustRegex::new(pattern) {
                                let haystack = args
                                    .first()
                                    .map(|a| match self.eval_expr_inner(&a.value) {
                                        Value::String(s) => s,
                                        _ => String::new(),
                                    })
                                    .unwrap_or_default();
                                let matches: Vec<Value> = rx
                                    .find_iter(&haystack)
                                    .map(|m| {
                                        let mut mf = HashMap::new();
                                        mf.insert(
                                            "text".to_string(),
                                            Value::String(m.as_str().to_string()),
                                        );
                                        mf.insert(
                                            "start".to_string(),
                                            Value::Int(m.start() as i64),
                                        );
                                        mf.insert("end".to_string(), Value::Int(m.end() as i64));
                                        Value::Struct {
                                            name: "Match".to_string(),
                                            fields: mf,
                                        }
                                    })
                                    .collect();
                                return Value::Array(matches);
                            }
                        }
                    }
                }
            }
            "replace_all" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Regex" {
                        if let Some(Value::String(ref pattern)) = fields.get("pattern") {
                            if let Ok(rx) = RustRegex::new(pattern) {
                                let mut arg_iter = args.iter();
                                let haystack = arg_iter
                                    .next()
                                    .map(|a| match self.eval_expr_inner(&a.value) {
                                        Value::String(s) => s,
                                        _ => String::new(),
                                    })
                                    .unwrap_or_default();
                                let replacement = arg_iter
                                    .next()
                                    .map(|a| match self.eval_expr_inner(&a.value) {
                                        Value::String(s) => s,
                                        _ => String::new(),
                                    })
                                    .unwrap_or_default();
                                let result = rx.replace_all(&haystack, replacement.as_str());
                                return Value::String(result.into_owned());
                            }
                        }
                    }
                }
            }
            // ── Client method dispatch ────────────────────────────────────────
            "post" => {
                if let Value::Struct { ref name, .. } = obj {
                    if name == "Client" {
                        let mut arg_iter = args.iter();
                        let url = arg_iter
                            .next()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        let body = arg_iter
                            .next()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        return eval_http_post(&url, &body);
                    }
                }
            }
            // ── Response / HttpError method dispatch ──────────────────────────
            "status" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Response" {
                        if let Some(v) = fields.get("status") {
                            return v.clone();
                        }
                        return Value::Int(0);
                    }
                }
            }
            "body" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Response" {
                        if let Some(v) = fields.get("body") {
                            return v.clone();
                        }
                        return Value::String(String::new());
                    }
                }
            }
            "header" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Response" {
                        let header_name = args
                            .first()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        // Headers are stored as a Map field (key → value strings).
                        if let Some(Value::Map(ref pairs)) = fields.get("headers") {
                            for (k, v) in pairs {
                                if let (Value::String(k_str), Value::String(v_str)) = (k, v) {
                                    if k_str.eq_ignore_ascii_case(&header_name) {
                                        return Value::EnumVariant {
                                            enum_name: "Option".to_string(),
                                            variant: "Some".to_string(),
                                            data: EnumData::Tuple(vec![Value::String(
                                                v_str.clone(),
                                            )]),
                                        };
                                    }
                                }
                            }
                        }
                        return Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        };
                    }
                }
            }
            "message" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "HttpError" {
                        if let Some(v) = fields.get("message") {
                            return v.clone();
                        }
                        return Value::String(String::new());
                    }
                }
            }
            _ => {}
        }

        // Try to find method via impl block
        let type_name = self.value_type_name(&obj);
        let method_key = format!("{}.{}", type_name, method);

        if let Some(func) = self.env.get(&method_key) {
            let mut arg_vals: Vec<Value> = vec![obj];
            arg_vals.extend(args.iter().map(|a| self.eval_expr_inner(&a.value)));

            if let Value::Function {
                param_patterns,
                param_defaults,
                body,
                closure_env,
                ..
            } = func
            {
                self.env.push_scope();
                if let Some(ref captured) = closure_env {
                    for (k, v) in captured {
                        self.env.define(k.clone(), v.clone());
                    }
                }
                // `param_patterns` already includes the `self` binding for
                // self-taking methods (prepended at impl-registration time),
                // so a straight in-order bind handles both receiver and args.
                for (i, pat) in param_patterns.iter().enumerate() {
                    let val = if let Some(v) = arg_vals.get(i) {
                        v.clone()
                    } else if let Some(Some(default_expr)) = param_defaults.get(i) {
                        self.eval_expr_inner(default_expr)
                    } else {
                        continue;
                    };
                    self.bind_pattern(pat, val);
                }
                let result = self.eval_block_inner(&body);
                self.env.pop_scope();
                return match result {
                    Ok(v) => v,
                    Err(ControlFlow::Return(v)) => v,
                    Err(cf) => self.set_cf(cf),
                };
            }
        }

        unreachable!(
            "method '{}' not found on type '{}' at {}:{}; should be caught by typechecker",
            method, type_name, span.line, span.column
        )
    }

    /// Dispatch `Resource.method(...)` by looking up the active provider for
    /// `Resource` on the provider stack and invoking `method` on the stored
    /// provider value. The value's concrete type (e.g. `InMemoryUserDB`) feeds
    /// the standard impl-block method table — so any `impl Trait for P` whose
    /// bounds satisfy the resource's provider-trait contract resolves
    /// correctly without a vtable. Missing provider bindings produce a
    /// runtime error: the typechecker accepts the call because the effect
    /// declares the resource, but at runtime no `with_provider` scope or
    /// ambient default installed the binding.
    fn eval_resource_method(
        &mut self,
        resource: &str,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Value {
        let Some(provider_arc) = self.lookup_provider(resource) else {
            return self.record_runtime_error(
                format!(
                    "no provider bound for resource '{}'; \
                     call `with_provider[{}](..., || {{ ... }})` to scope one",
                    resource, resource
                ),
                span,
            );
        };

        let provider = (*provider_arc).clone();
        let type_name = self.value_type_name(&provider);

        // Ambient program-rooted resources: the default provider is a
        // zero-field `BuiltinDefault<R>` struct (see `register_items`).
        // Dispatch its methods in Rust — `Clock.now()` returns the current
        // Unix timestamp in seconds, etc. User-declared resources never
        // start with the `BuiltinDefault` prefix, so the check is safe.
        if let Some(resource_name) = type_name.strip_prefix("BuiltinDefault") {
            return self.eval_builtin_resource_method(resource_name, method, args, span);
        }

        let method_key = format!("{}.{}", type_name, method);

        let Some(func) = self.env.get(&method_key) else {
            return self.record_runtime_error(
                format!(
                    "provider type '{}' bound to resource '{}' has no method '{}'",
                    type_name, resource, method
                ),
                span,
            );
        };

        let Value::Function {
            param_patterns,
            param_defaults,
            body,
            closure_env,
            ..
        } = func
        else {
            return self.record_runtime_error(
                format!("method '{}.{}' is not callable", type_name, method),
                span,
            );
        };

        let mut arg_vals: Vec<Value> = vec![provider];
        arg_vals.extend(args.iter().map(|a| self.eval_expr_inner(&a.value)));
        if self.check_cf() {
            return Value::Unit;
        }

        self.env.push_scope();
        if let Some(ref captured) = closure_env {
            for (k, v) in captured {
                self.env.define(k.clone(), v.clone());
            }
        }
        for (i, pat) in param_patterns.iter().enumerate() {
            let val = if let Some(v) = arg_vals.get(i) {
                v.clone()
            } else if let Some(Some(default_expr)) = param_defaults.get(i) {
                self.eval_expr_inner(default_expr)
            } else {
                continue;
            };
            self.bind_pattern(pat, val);
        }
        let result = self.eval_block_inner(&body);
        self.env.pop_scope();
        match result {
            Ok(v) => v,
            Err(ControlFlow::Return(v)) => v,
            Err(cf) => self.set_cf(cf),
        }
    }

    /// Dispatch a method call against the default provider for an ambient
    /// program-rooted resource. Called from [`eval_resource_method`] when
    /// the provider's type name has the `BuiltinDefault` prefix — i.e., no
    /// user `with_provider` has shadowed it yet. Each primitive's method
    /// surface is hand-coded here; the set grows as additional primitives
    /// land under `PRELUDE_EFFECT_RESOURCES`.
    fn eval_builtin_resource_method(
        &mut self,
        resource: &str,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Value {
        // Evaluate args for side-effect parity with the user-provider path.
        let arg_vals: Vec<Value> = args
            .iter()
            .map(|a| self.eval_expr_inner(&a.value))
            .collect();
        if self.check_cf() {
            return Value::Unit;
        }
        match (resource, method) {
            ("Clock", "now") => {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Value::Int(secs)
            }
            ("RandomSource", "next_u64") => {
                // Xorshift64 — adequate for the interpreter's non-cryptographic
                // use; real entropy comes through LLVM codegen later. The
                // `u64 as i64` cast is lossless bit-for-bit and matches the
                // Clock arm's convention for fitting wider values into
                // `Value::Int`.
                let mut x = self.rand_state;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.rand_state = x;
                Value::Int(x as i64)
            }
            ("Env", "args") => {
                // Process argv as `Vec[String]`. `std::env::args()` is
                // platform-safe and includes the binary path as element 0,
                // matching the Kāra spec's `env.args()` surface (design.md
                // § Built-in Resources — Nondeterminism, line 2799). Lossy
                // conversion for non-UTF-8 argv: `std::env::args` itself
                // panics in that case, same as Rust's convention.
                let vals: Vec<Value> = std::env::args().map(Value::String).collect();
                Value::Array(vals)
            }
            ("Env", "var") => {
                // `env.var(name) -> Result[String, VarError]` per design.md
                // § Built-in Resources line 2799. `VarError` shape settled
                // in brainstorming v49: single `NotPresent` variant, no
                // payload. `std::env::var` returns `Err(NotPresent)` for
                // missing vars and `Err(NotUnicode)` for non-UTF-8 values
                // — we collapse both to `VarError.NotPresent` since Kāra's
                // strict-UTF-8 `String` cannot carry the offending bytes.
                let name = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Env.var expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                match std::env::var(&name) {
                    Ok(v) => Value::EnumVariant {
                        enum_name: "Result".to_string(),
                        variant: "Ok".to_string(),
                        data: EnumData::Tuple(vec![Value::String(v)]),
                    },
                    Err(_) => Value::EnumVariant {
                        enum_name: "Result".to_string(),
                        variant: "Err".to_string(),
                        data: EnumData::Tuple(vec![Value::EnumVariant {
                            enum_name: "VarError".to_string(),
                            variant: "NotPresent".to_string(),
                            data: EnumData::Unit,
                        }]),
                    },
                }
            }
            // ── Stdin ──────────────────────────────────────────────
            ("Stdin", "read_line") => {
                self.track_effect("reads(Stdin)");
                let mut buf = String::new();
                match std::io::stdin().read_line(&mut buf) {
                    Ok(_) => io_ok(Value::String(buf)),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }
            ("Stdin", "read_to_string") => {
                self.track_effect("reads(Stdin)");
                let mut buf = String::new();
                use std::io::Read;
                match std::io::stdin().read_to_string(&mut buf) {
                    Ok(_) => io_ok(Value::String(buf)),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }

            // ── Stdout / Stderr ────────────────────────────────────
            ("Stdout", "flush") => {
                self.track_effect("writes(Stdout)");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Value::Unit
            }
            ("Stderr", "flush") => {
                self.track_effect("writes(Stderr)");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                Value::Unit
            }

            // ── FileSystem ─────────────────────────────────────────
            ("FileSystem", "read_to_string") => {
                self.track_effect("reads(FileSystem)");
                let path = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "FileSystem.read_to_string expects a String path".to_string(),
                            span,
                        );
                    }
                };
                match std::fs::read_to_string(&path) {
                    Ok(contents) => io_ok(Value::String(contents)),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }
            ("FileSystem", "write") => {
                self.track_effect("writes(FileSystem)");
                let (path, contents) = match (arg_vals.first(), arg_vals.get(1)) {
                    (Some(Value::String(p)), Some(Value::String(c))) => (p.clone(), c.clone()),
                    _ => {
                        return self.record_runtime_error(
                            "FileSystem.write expects (String path, String contents)".to_string(),
                            span,
                        );
                    }
                };
                match std::fs::write(&path, contents.as_bytes()) {
                    Ok(()) => io_ok(Value::Unit),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }

            _ => self.record_runtime_error(
                format!(
                    "ambient resource '{}' has no default method '{}' yet",
                    resource, method
                ),
                span,
            ),
        }
    }

    // ── Match evaluation ────────────────────────────────────────

    fn eval_match(&mut self, scrutinee: &Value, arms: &[MatchArm], span: &Span) -> Value {
        for arm in arms {
            if self.try_match_pattern(&arm.pattern, scrutinee) {
                // Check guard if present
                if let Some(ref guard) = arm.guard {
                    self.env.push_scope();
                    self.bind_pattern(&arm.pattern, scrutinee.clone());
                    let guard_val = self.eval_expr_inner(guard);
                    self.env.pop_scope();
                    if !self.is_truthy(&guard_val) {
                        continue;
                    }
                }
                self.env.push_scope();
                self.bind_pattern(&arm.pattern, scrutinee.clone());
                let result = self.eval_expr_inner(&arm.body);
                self.env.pop_scope();
                return result;
            }
        }
        unreachable!(
            "non-exhaustive match at {}:{}; should be caught by exhaustiveness checker",
            span.line, span.column
        )
    }

    // ── Pattern matching ────────────────────────────────────────

    fn try_match_pattern(&self, pattern: &Pattern, value: &Value) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard => true,
            PatternKind::Binding(name) => {
                // Check if this is actually an enum variant name (unit variant)
                if let Some(Value::EnumVariant {
                    variant,
                    data: EnumData::Unit,
                    ..
                }) = self.env.get(name)
                {
                    if let Value::EnumVariant { variant: v2, .. } = value {
                        return variant == *v2;
                    }
                    return false;
                }
                true // actual binding — matches anything
            }
            PatternKind::Literal(lit) => {
                let lit_val = self.literal_to_value(lit);
                lit_val == *value
            }
            PatternKind::TupleVariant { path, patterns } => {
                let variant_name = path.last().cloned().unwrap_or_default();
                match value {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } => {
                        variant == &variant_name
                            && patterns.len() == vals.len()
                            && patterns
                                .iter()
                                .zip(vals)
                                .all(|(p, v)| self.try_match_pattern(p, v))
                    }
                    _ => false,
                }
            }
            PatternKind::Struct { path, fields } => {
                let name = path.last().cloned().unwrap_or_default();
                match value {
                    Value::Struct {
                        name: sn,
                        fields: sfields,
                    } if *sn == name => fields.iter().all(|fp| {
                        if let Some(val) = sfields.get(&fp.name) {
                            if let Some(ref sub) = fp.pattern {
                                self.try_match_pattern(sub, val)
                            } else {
                                true
                            }
                        } else {
                            false
                        }
                    }),
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Struct(sfields),
                        ..
                    } if *variant == name => fields.iter().all(|fp| {
                        if let Some(val) = sfields.get(&fp.name) {
                            if let Some(ref sub) = fp.pattern {
                                self.try_match_pattern(sub, val)
                            } else {
                                true
                            }
                        } else {
                            false
                        }
                    }),
                    _ => false,
                }
            }
            PatternKind::Tuple(patterns) => match value {
                Value::Tuple(vals) => {
                    patterns.len() == vals.len()
                        && patterns
                            .iter()
                            .zip(vals)
                            .all(|(p, v)| self.try_match_pattern(p, v))
                }
                _ => false,
            },
            PatternKind::Or(alternatives) => alternatives
                .iter()
                .any(|p| self.try_match_pattern(p, value)),
            PatternKind::RangePattern { .. } => true, // simplified
            PatternKind::AtBinding { pattern, .. } => self.try_match_pattern(pattern, value),
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, value: Value) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => {
                // Don't rebind if this is a unit variant name being used as a pattern
                if let Some(existing) = self.env.get(name) {
                    if matches!(
                        existing,
                        Value::EnumVariant {
                            data: EnumData::Unit,
                            ..
                        }
                    ) {
                        return;
                    }
                }
                self.env.define(name.clone(), value);
            }
            PatternKind::Literal(_) => {}
            PatternKind::TupleVariant { patterns, .. } => {
                if let Value::EnumVariant {
                    data: EnumData::Tuple(vals),
                    ..
                } = value
                {
                    for (p, v) in patterns.iter().zip(vals) {
                        self.bind_pattern(p, v);
                    }
                }
            }
            PatternKind::Struct { fields, .. } => {
                let field_vals = match value {
                    Value::Struct { fields: f, .. } => f,
                    Value::EnumVariant {
                        data: EnumData::Struct(f),
                        ..
                    } => f,
                    _ => return,
                };
                for fp in fields {
                    if let Some(val) = field_vals.get(&fp.name) {
                        if let Some(ref sub) = fp.pattern {
                            self.bind_pattern(sub, val.clone());
                        } else {
                            self.env.define(fp.name.clone(), val.clone());
                        }
                    }
                }
            }
            PatternKind::Tuple(patterns) => {
                if let Value::Tuple(vals) = value {
                    for (p, v) in patterns.iter().zip(vals) {
                        self.bind_pattern(p, v);
                    }
                }
            }
            PatternKind::Or(alternatives) => {
                // Bind from first matching alternative
                if let Some(first) = alternatives.first() {
                    self.bind_pattern(first, value);
                }
            }
            PatternKind::AtBinding { name, pattern } => {
                self.env.define(name.clone(), value.clone());
                self.bind_pattern(pattern, value);
            }
            PatternKind::RangePattern { .. } => {}
        }
    }

    // ── Built-in functions ───────────────────────────────────────

    fn eval_builtin_diverge(&mut self, name: &str, args: &[CallArg], span: &Span) -> Value {
        self.track_effect("panics");
        let msg = if let Some(arg) = args.first() {
            match self.eval_expr_inner(&arg.value) {
                Value::String(s) => s,
                _ => String::new(),
            }
        } else {
            String::new()
        };
        let default_msg = if name == "todo" {
            "not yet implemented"
        } else {
            "entered unreachable code"
        };
        let full_msg = if msg.is_empty() {
            default_msg.to_string()
        } else {
            format!("{}: {}", default_msg, msg)
        };
        self.record_runtime_error(full_msg, span)
    }

    fn eval_builtin_print(&mut self, name: &str, args: &[CallArg]) -> Value {
        self.track_effect("writes(Stdout)");
        let val = if let Some(arg) = args.first() {
            format!("{}", self.eval_expr_inner(&arg.value))
        } else {
            String::new()
        };
        if let Some(ref mut output) = self.captured_output {
            if name == "println" {
                output.push(format!("{}\n", val));
            } else {
                output.push(val);
            }
        } else if name == "println" {
            println!("{}", val);
        } else {
            print!("{}", val);
        }
        Value::Unit
    }

    fn eval_builtin_dbg(&mut self, args: &[CallArg], span: &Span) -> Value {
        self.track_effect("writes(Stderr)");
        let val = if let Some(arg) = args.first() {
            self.eval_expr_inner(&arg.value)
        } else {
            Value::Unit
        };
        eprintln!("[{}:{}] = {}", span.line, span.column, val.debug_fmt());
        val
    }

    fn eval_builtin_assert(&mut self, args: &[CallArg], span: &Span) -> Value {
        self.track_effect("panics");
        let cond = match args.first() {
            Some(a) => self.eval_expr_inner(&a.value),
            None => return self.record_runtime_error("assert called with no arguments", span),
        };
        if matches!(cond, Value::Bool(true)) {
            return Value::Unit;
        }
        self.record_runtime_error("assertion failed", span)
    }

    fn eval_builtin_assert_eq(&mut self, args: &[CallArg], span: &Span) -> Value {
        self.track_effect("panics");
        let left = match args.first() {
            Some(a) => self.eval_expr_inner(&a.value),
            None => return self.record_runtime_error("assert_eq requires two arguments", span),
        };
        let right = match args.get(1) {
            Some(a) => self.eval_expr_inner(&a.value),
            None => return self.record_runtime_error("assert_eq requires two arguments", span),
        };
        if left == right {
            return Value::Unit;
        }
        let lstr = left.debug_fmt();
        let rstr = right.debug_fmt();
        self.record_runtime_assertion("assertion failed: left != right", lstr, rstr, span)
    }

    fn eval_builtin_assert_ne(&mut self, args: &[CallArg], span: &Span) -> Value {
        self.track_effect("panics");
        let left = match args.first() {
            Some(a) => self.eval_expr_inner(&a.value),
            None => return self.record_runtime_error("assert_ne requires two arguments", span),
        };
        let right = match args.get(1) {
            Some(a) => self.eval_expr_inner(&a.value),
            None => return self.record_runtime_error("assert_ne requires two arguments", span),
        };
        if left != right {
            return Value::Unit;
        }
        let lstr = left.debug_fmt();
        let rstr = right.debug_fmt();
        self.record_runtime_assertion("assertion failed: left == right", lstr, rstr, span)
    }

    // ── Helpers ─────────────────────────────────────────────────

    fn is_truthy(&self, val: &Value) -> bool {
        match val {
            Value::Bool(b) => *b,
            _ => unreachable!(
                "non-bool condition at runtime ({:?}); should be caught by typechecker",
                val
            ),
        }
    }

    fn set_cf(&mut self, cf: ControlFlow) -> Value {
        self.pending_cf = Some(cf);
        Value::Unit
    }

    fn literal_to_value(&self, lit: &LiteralPattern) -> Value {
        match lit {
            LiteralPattern::Integer(i, _) => Value::Int(*i),
            LiteralPattern::Float(f, _) => Value::Float(*f),
            LiteralPattern::String(s) => Value::String(s.clone()),
            LiteralPattern::Char(c) => Value::Char(*c),
            LiteralPattern::Bool(b) => Value::Bool(*b),
        }
    }

    fn value_type_name(&self, val: &Value) -> String {
        match val {
            Value::Struct { name, .. } => name.clone(),
            Value::EnumVariant { enum_name, .. } => enum_name.clone(),
            Value::Int(_) => "i64".to_string(),
            Value::Float(_) => "f64".to_string(),
            Value::Bool(_) => "bool".to_string(),
            Value::String(_) => "String".to_string(),
            Value::Char(_) => "char".to_string(),
            Value::TotalFloat32(_) => "F32".to_string(),
            Value::TotalFloat64(_) => "F64".to_string(),
            Value::Atomic(_) => "Atomic".to_string(),
            Value::Set(_) => "Set".to_string(),
            _ => "unknown".to_string(),
        }
    }

    fn find_enum_for_variant(&self, variant_name: &str) -> Option<String> {
        for item in &self.program.items {
            if let Item::EnumDef(e) = item {
                for v in &e.variants {
                    if v.name == variant_name {
                        return Some(e.name.clone());
                    }
                }
            }
        }
        None
    }

    fn set_field(&mut self, object: &Expr, field: &str, val: Value) {
        if let ExprKind::Identifier(name) = &object.kind {
            if let Some(Value::Struct { name: sn, fields }) = self.env.get(name) {
                let mut fields = fields;
                fields.insert(field.to_string(), val);
                self.env.set(name, Value::Struct { name: sn, fields });
            }
        }
    }

    fn set_index(&mut self, object: &Expr, index: &Expr, val: Value) {
        if let ExprKind::Identifier(name) = &object.kind {
            let idx = self.eval_expr_inner(index);
            if let (Some(Value::Array(arr)), Value::Int(i)) = (self.env.get(name), idx) {
                let mut arr = arr;
                arr[i as usize] = val;
                self.env.set(name, Value::Array(arr));
            }
        }
    }

    // ── Operators ───────────────────────────────────────────────

    fn eval_unary(&mut self, op: &UnaryOp, operand: Value, span: &Span) -> Value {
        match (op, operand) {
            (UnaryOp::Neg, Value::Int(i)) => Value::Int(match i.checked_neg() {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (UnaryOp::Neg, Value::Float(f)) => Value::Float(-f),
            (UnaryOp::Not, Value::Bool(b)) => Value::Bool(!b),
            (UnaryOp::BitNot, Value::Int(i)) => Value::Int(!i),
            // In the tree-walk interpreter references are passed by value; `*r` is
            // a semantic no-op that returns the underlying value unchanged.
            (UnaryOp::Deref, v) => v,
            _ => unreachable!(
                "type mismatch in unary operation at {}:{}; should be caught by typechecker",
                span.line, span.column
            ),
        }
    }

    fn eval_binary(&mut self, op: &BinOp, left: Value, right: Value, span: &Span) -> Value {
        match (op, left, right) {
            // Arithmetic (Int)
            (BinOp::Add, Value::Int(a), Value::Int(b)) => Value::Int(match a.checked_add(b) {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (BinOp::Sub, Value::Int(a), Value::Int(b)) => Value::Int(match a.checked_sub(b) {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (BinOp::Mul, Value::Int(a), Value::Int(b)) => Value::Int(match a.checked_mul(b) {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (BinOp::Div, Value::Int(a), Value::Int(b)) => {
                if b == 0 {
                    return self.record_runtime_error("division by zero", span);
                }
                Value::Int(match a.checked_div(b) {
                    Some(v) => v,
                    None => return self.record_integer_overflow(span),
                })
            }
            (BinOp::Mod, Value::Int(a), Value::Int(b)) => {
                if b == 0 {
                    return self.record_runtime_error("division by zero", span);
                }
                Value::Int(match a.checked_rem(b) {
                    Some(v) => v,
                    None => return self.record_integer_overflow(span),
                })
            }

            // Arithmetic (Float)
            (BinOp::Add, Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (BinOp::Sub, Value::Float(a), Value::Float(b)) => Value::Float(a - b),
            (BinOp::Mul, Value::Float(a), Value::Float(b)) => Value::Float(a * b),
            (BinOp::Div, Value::Float(a), Value::Float(b)) => Value::Float(a / b),
            (BinOp::Mod, Value::Float(a), Value::Float(b)) => Value::Float(a % b),

            // String Concatenation
            (BinOp::Add, Value::String(a), Value::String(b)) => Value::String(a + &b),

            // Comparison (Int)
            (BinOp::Eq, Value::Int(a), Value::Int(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Int(a), Value::Int(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Int(a), Value::Int(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::Int(a), Value::Int(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Int(a), Value::Int(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::Int(a), Value::Int(b)) => Value::Bool(a >= b),

            // Comparison (Float) - IEEE 754: NaN != NaN
            (BinOp::Eq, Value::Float(a), Value::Float(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Float(a), Value::Float(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Float(a), Value::Float(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::Float(a), Value::Float(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Float(a), Value::Float(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::Float(a), Value::Float(b)) => Value::Bool(a >= b),

            // Comparison (TotalFloat) - total order: NaN == NaN, NaN sorts last
            (BinOp::Eq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(a.total_cmp(&b).is_eq())
            }
            (BinOp::NotEq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(!a.total_cmp(&b).is_eq())
            }
            (BinOp::Lt, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(a.total_cmp(&b).is_lt())
            }
            (BinOp::LtEq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(!a.total_cmp(&b).is_gt())
            }
            (BinOp::Gt, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(a.total_cmp(&b).is_gt())
            }
            (BinOp::GtEq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(!a.total_cmp(&b).is_lt())
            }
            (BinOp::Eq, Value::TotalFloat32(a), Value::TotalFloat32(b)) => {
                Value::Bool(a.total_cmp(&b).is_eq())
            }
            (BinOp::NotEq, Value::TotalFloat32(a), Value::TotalFloat32(b)) => {
                Value::Bool(!a.total_cmp(&b).is_eq())
            }

            // Comparison (String)
            (BinOp::Eq, Value::String(a), Value::String(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::String(a), Value::String(b)) => Value::Bool(a != b),

            // Logical (Bool)
            (BinOp::And, Value::Bool(a), Value::Bool(b)) => Value::Bool(a && b),
            (BinOp::Or, Value::Bool(a), Value::Bool(b)) => Value::Bool(a || b),
            (BinOp::Eq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a != b),

            // Bitwise (Int)
            (BinOp::BitAnd, Value::Int(a), Value::Int(b)) => Value::Int(a & b),
            (BinOp::BitOr, Value::Int(a), Value::Int(b)) => Value::Int(a | b),
            (BinOp::BitXor, Value::Int(a), Value::Int(b)) => Value::Int(a ^ b),
            (BinOp::Shl, Value::Int(a), Value::Int(b)) => Value::Int(a << b),
            (BinOp::Shr, Value::Int(a), Value::Int(b)) => Value::Int(a >> b),

            _ => unreachable!(
                "type mismatch in binary operation {:?} at {}:{}; should be caught by typechecker",
                op, span.line, span.column
            ),
        }
    }

    fn eval_pipe(&mut self, left: &Expr, right: &Expr) -> Value {
        match &right.kind {
            // a |> f => f(a)
            ExprKind::Identifier(_) | ExprKind::Path(_) => {
                let desugared = Expr {
                    span: right.span.clone(),
                    kind: ExprKind::Call {
                        callee: Box::new(right.clone()),
                        args: vec![CallArg {
                            label: None,
                            mut_marker: false,
                            value: left.clone(),
                            span: left.span.clone(),
                        }],
                    },
                };
                self.eval_expr_inner(&desugared)
            }

            // a |> f(args...) => f(a, args...) or f(args with _ replaced)
            ExprKind::Call { callee, args } => {
                let has_placeholder = args
                    .iter()
                    .any(|arg| matches!(arg.value.kind, ExprKind::PipePlaceholder));

                let desugared_args: Vec<CallArg> = if has_placeholder {
                    args.iter()
                        .map(|arg| {
                            if matches!(arg.value.kind, ExprKind::PipePlaceholder) {
                                CallArg {
                                    label: arg.label.clone(),
                                    mut_marker: false,
                                    value: left.clone(),
                                    span: left.span.clone(),
                                }
                            } else {
                                arg.clone()
                            }
                        })
                        .collect()
                } else {
                    let mut new_args = vec![CallArg {
                        label: None,
                        mut_marker: false,
                        value: left.clone(),
                        span: left.span.clone(),
                    }];
                    new_args.extend(args.iter().cloned());
                    new_args
                };

                let desugared = Expr {
                    span: right.span.clone(),
                    kind: ExprKind::Call {
                        callee: callee.clone(),
                        args: desugared_args,
                    },
                };
                self.eval_expr_inner(&desugared)
            }

            _ => unreachable!(
                "invalid pipe right-hand side at {}:{}; should be caught by parser/typechecker",
                right.span.line, right.span.column
            ),
        }
    }

    fn record_integer_overflow(&mut self, span: &Span) -> Value {
        self.record_runtime_error("integer overflow", span)
    }
}

// ── Value ordering helper ────────────────────────────────────────────────────

/// Total ordering for interpreter `Value` used by `sort` / `binary_search`.
/// Defines a canonical order: Int < Float < Bool < Char < String < other.
/// Within each variant, values are ordered by their natural Rust ordering.
fn value_compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Char(x), Value::Char(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Tuple(xs), Value::Tuple(ys)) => xs
            .iter()
            .zip(ys.iter())
            .find_map(|(a, b)| {
                let ord = value_compare(a, b);
                if ord != Ordering::Equal {
                    Some(ord)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| xs.len().cmp(&ys.len())),
        // Two Maps: lexicographic over (key, value) pairs in insertion order
        (Value::Map(a), Value::Map(b)) => a
            .iter()
            .zip(b.iter())
            .find_map(|((ak, av), (bk, bv))| {
                let k_ord = value_compare(ak, bk);
                if k_ord != Ordering::Equal {
                    Some(k_ord)
                } else {
                    let v_ord = value_compare(av, bv);
                    if v_ord != Ordering::Equal {
                        Some(v_ord)
                    } else {
                        None
                    }
                }
            })
            .unwrap_or_else(|| a.len().cmp(&b.len())),
        // Two SortedSets: lexicographic over their ascending key sequences
        (Value::SortedSet(a), Value::SortedSet(b)) => {
            let ak: Vec<_> = a.keys().collect();
            let bk: Vec<_> = b.keys().collect();
            ak.iter()
                .zip(bk.iter())
                .find_map(|(x, y)| {
                    let ord = value_compare(&x.0, &y.0);
                    if ord != Ordering::Equal {
                        Some(ord)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| ak.len().cmp(&bk.len()))
        }
        // Cross-variant ordering by discriminant index
        _ => value_discriminant(a).cmp(&value_discriminant(b)),
    }
}

fn value_discriminant(v: &Value) -> u8 {
    match v {
        Value::Int(_) => 0,
        Value::Float(_) => 1,
        Value::Bool(_) => 2,
        Value::Char(_) => 3,
        Value::String(_) => 4,
        Value::Tuple(_) => 5,
        Value::Array(_) => 6,
        Value::Unit => 7,
        Value::Map(_) => 12,
        Value::SortedSet(_) => 9,
        Value::Set(_) => 13,
        Value::Sender(_) => 10,
        Value::Receiver(_) => 11,
        _ => 8,
    }
}

// ── Stats stdlib helpers ─────────────────────────────────────────────────────

fn eval_stats_fn(name: &str, xs: &[f64], span: &Span) -> Value {
    match name {
        "Stats.sum" => Value::Float(xs.iter().sum()),
        "Stats.prod" => Value::Float(xs.iter().product()),
        "Stats.mean" => {
            if xs.is_empty() {
                panic!(
                    "Stats.mean() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            Value::Float(xs.iter().sum::<f64>() / xs.len() as f64)
        }
        "Stats.variance" => {
            if xs.is_empty() {
                panic!(
                    "Stats.variance() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            let mean = xs.iter().sum::<f64>() / xs.len() as f64;
            let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64;
            Value::Float(var)
        }
        "Stats.stddev" => {
            if xs.is_empty() {
                panic!(
                    "Stats.stddev() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            let mean = xs.iter().sum::<f64>() / xs.len() as f64;
            let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64;
            Value::Float(var.sqrt())
        }
        "Stats.median" => {
            if xs.is_empty() {
                panic!(
                    "Stats.median() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            let mut sorted = xs.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = sorted.len() / 2;
            let median = if sorted.len().is_multiple_of(2) {
                (sorted[mid - 1] + sorted[mid]) / 2.0
            } else {
                sorted[mid]
            };
            Value::Float(median)
        }
        "Stats.min" => {
            let result = xs.iter().copied().reduce(f64::min);
            match result {
                Some(v) => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "Some".to_string(),
                    data: EnumData::Tuple(vec![Value::Float(v)]),
                },
                None => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "None".to_string(),
                    data: EnumData::Unit,
                },
            }
        }
        "Stats.max" => {
            let result = xs.iter().copied().reduce(f64::max);
            match result {
                Some(v) => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "Some".to_string(),
                    data: EnumData::Tuple(vec![Value::Float(v)]),
                },
                None => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "None".to_string(),
                    data: EnumData::Unit,
                },
            }
        }
        _ => Value::Unit,
    }
}

// ── Encoding stdlib helpers (Base64 / Hex / Url) ────────────────────────────

const BASE64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64_encode(bytes: &[u8], url_safe: bool) -> String {
    let alphabet = if url_safe { BASE64_URL } else { BASE64_STD };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((n >> 18) & 0x3f) as usize] as char);
        out.push(alphabet[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() >= 2 {
            out.push(alphabet[((n >> 6) & 0x3f) as usize] as char);
        } else if !url_safe {
            out.push('=');
        }
        if chunk.len() == 3 {
            out.push(alphabet[(n & 0x3f) as usize] as char);
        } else if !url_safe {
            out.push('=');
        }
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn decode_char(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let trimmed = s.trim_end_matches('=');
    let mut bytes = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut buf = [0u8; 4];
    let mut n = 0;
    for c in trimmed.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let v =
            decode_char(c).ok_or_else(|| format!("invalid base64 character: {:?}", c as char))?;
        buf[n] = v;
        n += 1;
        if n == 4 {
            bytes.push((buf[0] << 2) | (buf[1] >> 4));
            bytes.push((buf[1] << 4) | (buf[2] >> 2));
            bytes.push((buf[2] << 6) | buf[3]);
            n = 0;
        }
    }
    match n {
        0 => {}
        1 => return Err("invalid base64 length: trailing single character".to_string()),
        2 => bytes.push((buf[0] << 2) | (buf[1] >> 4)),
        3 => {
            bytes.push((buf[0] << 2) | (buf[1] >> 4));
            bytes.push((buf[1] << 4) | (buf[2] >> 2));
        }
        _ => unreachable!(),
    }
    Ok(bytes)
}

fn hex_encode(bytes: &[u8], upper: bool) -> String {
    let lut: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(lut[(b >> 4) as usize] as char);
        out.push(lut[(b & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    fn from_hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bs = s.as_bytes();
    if !bs.len().is_multiple_of(2) {
        return Err(format!("invalid hex length: {} (must be even)", bs.len()));
    }
    let mut out = Vec::with_capacity(bs.len() / 2);
    for chunk in bs.chunks(2) {
        let hi = from_hex(chunk[0])
            .ok_or_else(|| format!("invalid hex character: {:?}", chunk[0] as char))?;
        let lo = from_hex(chunk[1])
            .ok_or_else(|| format!("invalid hex character: {:?}", chunk[1] as char))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

fn url_decode(s: &str) -> Result<String, String> {
    fn from_hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bs = s.as_bytes();
    let mut out = Vec::with_capacity(bs.len());
    let mut i = 0;
    while i < bs.len() {
        if bs[i] == b'%' {
            if i + 2 >= bs.len() {
                return Err("incomplete percent-encoded sequence at end of input".to_string());
            }
            let hi = from_hex(bs[i + 1]).ok_or_else(|| {
                format!(
                    "invalid percent-encoded byte: %{}{}",
                    bs[i + 1] as char,
                    bs[i + 2] as char
                )
            })?;
            let lo = from_hex(bs[i + 2]).ok_or_else(|| {
                format!(
                    "invalid percent-encoded byte: %{}{}",
                    bs[i + 1] as char,
                    bs[i + 2] as char
                )
            })?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bs[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|e| format!("invalid UTF-8 in decoded URL: {e}"))
}

fn decode_ok_bytes(bytes: Vec<u8>) -> Value {
    let arr = bytes.into_iter().map(|b| Value::Int(b as i64)).collect();
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![Value::Array(arr)]),
    }
}

fn decode_ok_string(s: String) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![Value::String(s)]),
    }
}

fn decode_err(message: String) -> Value {
    let mut fields = HashMap::new();
    fields.insert("message".to_string(), Value::String(message));
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Err".to_string(),
        data: EnumData::Tuple(vec![Value::Struct {
            name: "DecodeError".to_string(),
            fields,
        }]),
    }
}

// ── I/O stdlib helpers ──────────────────────────────────────────────────────

fn io_ok(val: Value) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![val]),
    }
}

fn io_err_value(io_error: Value) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Err".to_string(),
        data: EnumData::Tuple(vec![io_error]),
    }
}

fn io_error_from_std(e: &std::io::Error) -> Value {
    let (variant, payload) = match e.kind() {
        std::io::ErrorKind::NotFound => ("NotFound", None),
        std::io::ErrorKind::PermissionDenied => ("PermissionDenied", None),
        std::io::ErrorKind::AlreadyExists => ("AlreadyExists", None),
        std::io::ErrorKind::UnexpectedEof => ("UnexpectedEof", None),
        std::io::ErrorKind::InvalidData => ("InvalidUtf8", None),
        std::io::ErrorKind::Interrupted => ("Interrupted", None),
        _ => ("Other", Some(e.to_string())),
    };
    Value::EnumVariant {
        enum_name: "IoError".to_string(),
        variant: variant.to_string(),
        data: match payload {
            None => EnumData::Unit,
            Some(msg) => EnumData::Tuple(vec![Value::String(msg)]),
        },
    }
}

// ── std.http helpers ──────────────────────────────────────────────────────────

fn make_response(status: u16, body: String, headers: Vec<(String, String)>) -> Value {
    let mut fields = HashMap::new();
    fields.insert("status".to_string(), Value::Int(status as i64));
    fields.insert("body".to_string(), Value::String(body));
    let header_pairs: Vec<Value> = headers
        .into_iter()
        .map(|(k, v)| Value::Tuple(vec![Value::String(k), Value::String(v)]))
        .collect();
    // Store headers as a flat Vec<(k,v)> in a Map value for header() lookup.
    let map_pairs: Vec<(Value, Value)> = header_pairs
        .iter()
        .filter_map(|v| {
            if let Value::Tuple(ref kv) = v {
                if kv.len() == 2 {
                    return Some((kv[0].clone(), kv[1].clone()));
                }
            }
            None
        })
        .collect();
    fields.insert("headers".to_string(), Value::Map(map_pairs));
    Value::Struct {
        name: "Response".to_string(),
        fields,
    }
}

fn make_http_error(message: String) -> Value {
    let mut fields = HashMap::new();
    fields.insert("message".to_string(), Value::String(message));
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Err".to_string(),
        data: EnumData::Tuple(vec![Value::Struct {
            name: "HttpError".to_string(),
            fields,
        }]),
    }
}

fn wrap_ok_response(resp: ureq::Response) -> Value {
    let status = resp.status();
    // Collect headers before consuming the response.
    let content_type = resp.header("content-type").unwrap_or("").to_string();
    let body = resp.into_string().unwrap_or_default();
    let mut headers = Vec::new();
    if !content_type.is_empty() {
        headers.push(("content-type".to_string(), content_type));
    }
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![make_response(status, body, headers)]),
    }
}

fn eval_http_get(url: &str) -> Value {
    match ureq::get(url).call() {
        Ok(resp) => wrap_ok_response(resp),
        Err(e) => make_http_error(e.to_string()),
    }
}

fn eval_http_post(url: &str, body: &str) -> Value {
    match ureq::post(url).send_string(body) {
        Ok(resp) => wrap_ok_response(resp),
        Err(e) => make_http_error(e.to_string()),
    }
}
