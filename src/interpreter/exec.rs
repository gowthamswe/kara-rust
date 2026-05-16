//! Control-flow signals, scope/cleanup helpers, and the Env scope chain.
//!
//! Houses the non-local control-flow enum (`ControlFlow`), the
//! cleanup-action types (`CleanupAction`, `ErrDeferEntry`), the
//! block-exit classifier (`ExitPath`), value-deep-clone
//! (`deep_clone_value`), slice-pattern view (`slice_pattern_view`),
//! `option_value_from` / `cancelled_sentinel`, last-use analysis
//! (`compute_block_last_use`, `push_drops_for_stmt`), the scope-chain
//! `Env` struct with its impl, and the free-identifier scanning
//! helpers (`add_pattern_bindings`, `collect_free_idents_block`,
//! `collect_free_idents_expr`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use crate::ast::*;

use super::value::{EnumData, Value};

// ── Control Flow Signals ────────────────────────────────────────

/// Signals for non-local control flow (return, break, continue, exit).
#[derive(Debug)]
pub(crate) enum ControlFlow {
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
    /// A `par {}` sibling branch observed the shared cancel flag at
    /// a between-statement effect-boundary check. The propagating
    /// branch's `errdefer` phase fires with `e = Cancelled` per
    /// design.md § Drop ordering within a branch. `eval_par_block`
    /// silences this on the result side — the originating branch's
    /// real `Err` is the scope's return value under fail-fast.
    Cancelled,
}

pub(crate) type EvalResult = Result<Value, ControlFlow>;

// ── Unified drop+defer cleanup stack ────────────────────────────

/// One entry in a block's unified drop+defer cleanup stack. Per
/// design.md § Drop ordering within a branch, destructors and
/// `defer` blocks interleave in a single program-order LIFO stack.
pub(crate) enum CleanupAction {
    /// A `defer { ... }` block.
    Defer(Block),
    /// A binding's destructor slot. The action is a no-op today — the
    /// Phase 6 user-`Drop` and Rc/Arc-decrement wiring attaches here
    /// without disturbing program-order LIFO position.
    #[allow(dead_code)]
    Drop { name: String },
}

/// One entry in a block's `errdefer` stack (phase-1 cleanup, error
/// paths only). Kept separate from the unified drop+defer stack
/// because `errdefer` always fires before any destructor or `defer`.
pub(crate) struct ErrDeferEntry {
    pub(crate) binding: Option<String>,
    pub(crate) body: Block,
}

/// Classification of a block's exit path, used to drive `errdefer`
/// behavior. Param-less `errdefer` fires on every error path;
/// `errdefer(e)` only binds when a payload is available.
pub(crate) enum ExitPath {
    Normal,
    Err(Value),
    NoneProp,
    Panic,
    /// `par {}` cancellation — sub-step 4 emits this from cancelled
    /// siblings so `errdefer(e)` binds `e` to `Cancelled`.
    #[allow(dead_code)]
    Cancelled(Value),
}

impl ExitPath {
    pub(crate) fn classify(cf: &ControlFlow) -> ExitPath {
        match cf {
            ControlFlow::Return(Value::EnumVariant { variant, data, .. }) if variant == "Err" => {
                let payload = match data {
                    EnumData::Tuple(vs) => vs.first().cloned().unwrap_or(Value::Unit),
                    _ => Value::Unit,
                };
                ExitPath::Err(payload)
            }
            ControlFlow::Return(Value::EnumVariant { variant, .. }) if variant == "None" => {
                ExitPath::NoneProp
            }
            ControlFlow::Cancelled => ExitPath::Cancelled(cancelled_sentinel()),
            ControlFlow::RuntimeError | ControlFlow::ExitUnwind { .. } => ExitPath::Panic,
            _ => ExitPath::Normal,
        }
    }

    pub(crate) fn is_error(&self) -> bool {
        !matches!(self, ExitPath::Normal)
    }
}

impl std::fmt::Debug for ExitPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitPath::Normal => write!(f, "Normal"),
            ExitPath::Err(_) => write!(f, "Err(_)"),
            ExitPath::NoneProp => write!(f, "NoneProp"),
            ExitPath::Panic => write!(f, "Panic"),
            ExitPath::Cancelled(_) => write!(f, "Cancelled(_)"),
        }
    }
}

/// Deep-clone a `Value`, materializing independent storage for the
/// by-value collection variants (`Array`, `Map`, `Set`, `Tuple`,
/// `Struct`, `EnumVariant`, `Slice`). The derived `Clone` on `Value`
/// shallow-clones `Array` / `SortedSet` etc. (the `Arc<RwLock<...>>`
/// is bumped, sharing storage) — that's the right default for most
/// dispatch paths since slice tracking depends on the shared cell.
/// But operations whose Kāra-spec semantics produce *independent*
/// copies (e.g., `Vec.filled[T: Clone]`) must materialize fresh
/// storage per slot, otherwise nested-collection element types alias
/// across copies.
///
/// Reference-semantics types (`SharedStruct`, `Sender`, `Receiver`,
/// `SharedCell`, `Atomic`) preserve aliasing — those types are
/// shared-by-design per Kāra's `shared struct` and channel rules.
pub(crate) fn deep_clone_value(v: &Value) -> Value {
    match v {
        Value::Array(rc) => {
            let items: Vec<Value> = rc.read().unwrap().iter().map(deep_clone_value).collect();
            Value::array_of(items)
        }
        Value::Slice {
            storage,
            start,
            len,
            ..
        } => {
            // A deep clone of a slice produces an independent owned
            // snapshot — the original window's storage is left alone.
            let snapshot: Vec<Value> = storage.read().unwrap()[*start..*start + *len]
                .iter()
                .map(deep_clone_value)
                .collect();
            Value::array_of(snapshot)
        }
        Value::Set(items) => Value::Set(items.iter().map(deep_clone_value).collect()),
        Value::Map(entries) => Value::Map(
            entries
                .iter()
                .map(|(k, val)| (deep_clone_value(k), deep_clone_value(val)))
                .collect(),
        ),
        Value::Tuple(items) => Value::Tuple(items.iter().map(deep_clone_value).collect()),
        Value::Struct { name, fields } => Value::Struct {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(k, val)| (k.clone(), deep_clone_value(val)))
                .collect(),
        },
        Value::EnumVariant {
            enum_name,
            variant,
            data,
        } => Value::EnumVariant {
            enum_name: enum_name.clone(),
            variant: variant.clone(),
            data: match data {
                EnumData::Unit => EnumData::Unit,
                EnumData::Tuple(vals) => {
                    EnumData::Tuple(vals.iter().map(deep_clone_value).collect())
                }
                EnumData::Struct(fields) => EnumData::Struct(
                    fields
                        .iter()
                        .map(|(k, val)| (k.clone(), deep_clone_value(val)))
                        .collect(),
                ),
            },
        },
        // Primitives, String, SortedSet (primitive-keyed), and the
        // reference-semantics types (SharedStruct, Sender, Receiver,
        // SharedCell, Atomic) all clone correctly under the derive.
        _ => v.clone(),
    }
}

/// Uniform view of a slice-pattern scrutinee — `(storage, offset, len,
/// source_mutable)`. `Value::Array` exposes its entire backing at offset
/// 0 (immutable for the rest-binding mode flag); `Value::Slice`
/// re-exposes its existing window with the inherited mutability flag.
type SlicePatternView = (Arc<RwLock<Vec<Value>>>, usize, usize, bool);

/// View a slice-pattern scrutinee as a `SlicePatternView`. The
/// rest binding's mutability mirrors the source. Returns `None` for
/// any other Value variant (the typechecker rejects non-sequence
/// scrutinees, so this is a defensive never-match fallback if reached).
pub(crate) fn slice_pattern_view(value: &Value) -> Option<SlicePatternView> {
    match value {
        Value::Array(rc) => {
            let len = rc.read().unwrap().len();
            Some((rc.clone(), 0, len, false))
        }
        Value::Slice {
            storage,
            start,
            len,
            mutable,
        } => Some((storage.clone(), *start, *len, *mutable)),
        _ => None,
    }
}

/// Wrap a `Some(Value)` / `None` Rust option in the corresponding
/// Kāra `Option[T]` enum variant. Used by `pop_back` / `pop_front` —
/// any method whose return type is `Option[T]` and whose Rust impl
/// already produces an `Option<Value>`.
pub(crate) fn option_value_from(v: Option<Value>) -> Value {
    match v {
        Some(inner) => Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "Some".to_string(),
            data: EnumData::Tuple(vec![inner]),
        },
        None => Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "None".to_string(),
            data: EnumData::Unit,
        },
    }
}

/// Sentinel value bound to `errdefer(e)` in cancelled `par {}` siblings.
/// Per design.md § Drop ordering within a branch, the real value should
/// come from `E::cancelled()` where `E` is the function's `Err` type and
/// `E: Cancellable`; until that trait + factory wiring lands in the
/// typechecker, a placeholder unit-variant carries the right shape.
pub(crate) fn cancelled_sentinel() -> Value {
    Value::EnumVariant {
        enum_name: "Cancelled".to_string(),
        variant: "Cancelled".to_string(),
        data: EnumData::Unit,
    }
}

/// Per-binding last-use index map used by `eval_block_inner` to
/// fire `Drop` slots at the live-range end (NLL placement) instead
/// of waiting for scope exit. Per design.md § Drop ordering within
/// a branch, NLL drops happen at the binding's last-use program
/// point; this map tells the block evaluator which statement to
/// fire each binding's `Drop` after.
///
/// Sentinel: `stmts.len()` means "scope exit" — the binding is
/// referenced in the block's `final_expr`, in any registered
/// defer/errdefer body, or in any nested-block construct that the
/// shallow walker conservatively treats as opaque. Drops with this
/// sentinel stay in `cleanup` and drain via the unified LIFO at
/// scope exit, preserving defer/drop interleave for that case.
///
/// The walker is intentionally conservative — it only fires NLL
/// drops when it can prove the binding is dead. Cross-block
/// liveness (CFG dataflow) is out of scope for this round.
pub(crate) fn compute_block_last_use(block: &Block) -> HashMap<String, usize> {
    // Collect every binding the block introduces.
    let mut owned: HashSet<String> = HashSet::new();
    for stmt in &block.stmts {
        match &stmt.kind {
            StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => {
                for n in pattern.binding_names() {
                    owned.insert(n);
                }
            }
            StmtKind::LetUninit { name, .. } => {
                owned.insert(name.clone());
            }
            _ => {}
        }
    }
    if owned.is_empty() {
        return HashMap::new();
    }
    let scope_exit = block.stmts.len();
    let mut last_use: HashMap<String, usize> = HashMap::new();

    // Per-statement free-idents walk. We only care which `owned`
    // bindings each statement *references* — outer-block bindings
    // shadowed by inner constructs already get filtered by the
    // walker's `bound` tracking when it descends into nested blocks.
    // We pass a fresh empty `bound` set per stmt so the OUTER `owned`
    // names always show up as free idents.
    let record_use = |name: String,
                      idx: usize,
                      owned: &HashSet<String>,
                      last_use: &mut HashMap<String, usize>,
                      scope_exit: usize| {
        if !owned.contains(&name) {
            return;
        }
        // Pinned-to-scope-exit wins; otherwise advance to the latest idx.
        match last_use.get(&name).copied() {
            Some(prev) if prev == scope_exit => {}
            _ => {
                last_use.insert(name, idx);
            }
        }
    };
    for (idx, stmt) in block.stmts.iter().enumerate() {
        let mut idents: Vec<String> = Vec::new();
        match &stmt.kind {
            // A defer/errdefer body executes at scope exit. Any
            // binding it references must remain live until then —
            // pin those to `scope_exit`.
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                let mut bound: HashSet<String> = HashSet::new();
                collect_free_idents_block(body, &mut bound, &mut idents);
                for name in idents {
                    if owned.contains(&name) {
                        last_use.insert(name, scope_exit);
                    }
                }
                continue;
            }
            // Let RHS uses outer scope; the new pattern binding takes
            // effect for subsequent statements.
            StmtKind::Let { value, .. } | StmtKind::LetElse { value, .. } => {
                let mut bound: HashSet<String> = HashSet::new();
                collect_free_idents_expr(value, &mut bound, &mut idents);
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::Assign { target, value } => {
                let mut bound: HashSet<String> = HashSet::new();
                collect_free_idents_expr(target, &mut bound, &mut idents);
                collect_free_idents_expr(value, &mut bound, &mut idents);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                let mut bound: HashSet<String> = HashSet::new();
                collect_free_idents_expr(target, &mut bound, &mut idents);
                collect_free_idents_expr(value, &mut bound, &mut idents);
            }
            StmtKind::Expr(expr) => {
                let mut bound: HashSet<String> = HashSet::new();
                collect_free_idents_expr(expr, &mut bound, &mut idents);
            }
        }
        for name in idents {
            record_use(name, idx, &owned, &mut last_use, scope_exit);
        }
    }
    // The block's `final_expr` (if any) runs after the last stmt
    // but before scope-exit cleanup drains. A binding referenced
    // there must stay live until scope exit so the unified LIFO
    // drain interleaves it with any Defers correctly.
    if let Some(final_expr) = &block.final_expr {
        let mut idents: Vec<String> = Vec::new();
        let mut bound: HashSet<String> = HashSet::new();
        collect_free_idents_expr(final_expr, &mut bound, &mut idents);
        for name in idents {
            if owned.contains(&name) {
                last_use.insert(name, scope_exit);
            }
        }
    }
    // Bindings introduced but never read: NLL says they die
    // immediately after the let — last_use = the let's own index.
    for stmt_idx in 0..block.stmts.len() {
        let stmt = &block.stmts[stmt_idx];
        match &stmt.kind {
            StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => {
                for n in pattern.binding_names() {
                    last_use.entry(n).or_insert(stmt_idx);
                }
            }
            StmtKind::LetUninit { name, .. } => {
                last_use.entry(name.clone()).or_insert(stmt_idx);
            }
            _ => {}
        }
    }
    last_use
}

/// Push a `Drop` action for each binding the statement introduced.
/// Called after the statement evaluates successfully, so the drop
/// slot lands at the program-order LIFO position the binding
/// claims in the unified stack.
pub(crate) fn push_drops_for_stmt(stmt: &Stmt, cleanup: &mut Vec<CleanupAction>) {
    match &stmt.kind {
        StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => {
            for name in pattern.binding_names() {
                cleanup.push(CleanupAction::Drop { name });
            }
        }
        StmtKind::LetUninit { name, .. } => {
            cleanup.push(CleanupAction::Drop { name: name.clone() });
        }
        _ => {}
    }
}

// ── Scoped Environment ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct Env {
    pub(crate) scopes: Vec<HashMap<String, Value>>,
}

impl Env {
    pub(crate) fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn define(&mut self, name: String, val: Value) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, val);
        }
    }

    pub(crate) fn set(&mut self, name: &str, val: Value) {
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
    pub(crate) fn get(&self, name: &str) -> Option<Value> {
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
    pub(crate) fn snapshot(&self) -> HashMap<String, Value> {
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
    pub(crate) fn wrap_capture(&mut self, name: &str) -> Option<Value> {
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
pub(crate) fn add_pattern_bindings(pat: &Pattern, out: &mut HashSet<String>) {
    for n in pat.binding_names() {
        out.insert(n);
    }
}

pub(crate) fn collect_free_idents_block(
    block: &Block,
    bound: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
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

pub(crate) fn collect_free_idents_expr(
    expr: &Expr,
    bound: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    match &expr.kind {
        ExprKind::Identifier(name) => {
            if !bound.contains(name) {
                out.push(name.clone());
            }
        }
        ExprKind::Path { .. }
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
        | ExprKind::OffsetOf { .. }
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
        ExprKind::LabeledBlock { body, .. } => collect_free_idents_block(body, bound, out),
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
        ExprKind::Par(b) | ExprKind::Seq(b) | ExprKind::Unsafe(b) | ExprKind::Try(b) => {
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
