use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, RwLock};

use crate::ast::*;
use crate::token::Span;
use crate::typechecker::TypeCheckResult;

mod builtin;
mod eval_call;
mod eval_expr;
mod eval_ops;
mod eval_stmt;
mod exec;
mod helpers;
mod iter_eval;
mod method_call;
mod method_call_http;
mod method_call_iter;
mod method_call_map;
mod method_call_optres;
mod method_call_regex;
mod method_call_set;
mod pattern_match;
mod resource_method;
mod value;

use exec::{deep_clone_value, option_value_from, ControlFlow, Env};
use value::{
    try_write_or_panic, upgrade_weak_to_option, EnumData, FieldCell, SharedStructInner,
    ERROR_TRACE_MAX_DEPTH,
};
pub use value::{ErrorTraceFrame, RuntimeError, TestOutcome, Value};

// ── Interpreter ─────────────────────────────────────────────────

pub struct Interpreter<'a> {
    pub(crate) program: &'a Program,
    #[allow(dead_code)]
    pub(crate) typecheck_result: &'a TypeCheckResult,
    pub(crate) env: Env,
    /// Captured output for testing (when Some, print/println write here instead of stdout)
    pub captured_output: Option<Vec<String>>,
    /// Pending control flow signal (return/break/continue)
    pub(crate) pending_cf: Option<ControlFlow>,
    /// Runtime effect tracking: records effects performed during execution
    pub tracked_effects: Vec<String>,
    /// Tracks variables that have been moved (ownership simulation)
    #[allow(dead_code)]
    pub(crate) moved_vars: std::collections::HashSet<String>,
    /// Error return trace: ring buffer of (file, line, expr_text) for ? propagation
    pub(crate) error_trace: Vec<ErrorTraceFrame>,
    /// Whether oldest entries were dropped from the trace ring buffer
    pub(crate) error_trace_truncated: bool,
    /// Source filename for error trace frames
    pub(crate) source_filename: String,
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
    pub(crate) provider_stack: Vec<HashMap<String, Arc<Value>>>,
    /// Names of `effect resource` declarations in the program, collected
    /// at [`register_items`] time. Used by [`eval_method_call`] to detect
    /// receivers of the form `UserDB.query(...)` — where `UserDB` is not
    /// a value binding — and dispatch via the provider stack instead of
    /// normal method lookup.
    pub(crate) effect_resources: HashSet<String>,
    /// Xorshift64 state backing the default `RandomSource` provider.
    /// Seeded once per [`Interpreter::new`] from the system clock's
    /// sub-second nanoseconds so repeated `cargo test` runs see fresh
    /// sequences. `with_provider[RandomSource](Fake…)` shadows this
    /// entirely; determinism-sensitive tests must opt in via a fake.
    pub(crate) rand_state: u64,
    /// Per-call frame of generic-param substitutions: name → concrete type
    /// name. Pushed at every generic call (using
    /// `TypeCheckResult.call_type_subs` keyed by call span); popped on
    /// return. `T.method()` and bare-call dispatch in trait associated
    /// function bodies look up `T` through this stack to find the concrete
    /// impl to dispatch to. Outer-frame entries are visible (transitive
    /// resolution: a callee's `T → "U"` where `U` is itself a generic param
    /// of the caller resolves via the next frame down).
    pub(crate) type_subs_stack: Vec<HashMap<String, String>>,
    /// `par {}` shared cancellation flag. Set by `eval_par_block` on
    /// each branch interpreter; observed by `eval_block_inner` between
    /// top-level statements as a minimal effect-boundary check. When
    /// observed, the running branch raises `ControlFlow::Cancelled`,
    /// which classifies as `ExitPath::Cancelled(sentinel)` so any
    /// `errdefer(e)` in the active scope binds `e` to the sentinel
    /// during the errdefer phase. None outside `par {}` branches.
    pub(crate) cancel_flag: Option<Arc<AtomicBool>>,
    /// Records the order in which `CleanupAction::Drop` slots fire —
    /// both NLL early-drops (mid-block, after a binding's last use)
    /// and scope-exit drops drained from the unified cleanup stack.
    /// Each entry is the binding's name; tests inspect this trace to
    /// verify drop placement and ordering since the interpreter has
    /// no observable user-`impl Drop` dispatch yet. Always populated
    /// (cheap; small in real programs) — a public accessor is exposed
    /// so test harness functions can read it after `run()`.
    pub drop_trace: Vec<String>,
    /// Full source text of the program being executed. Used by
    /// `eval_builtin_dbg` to slice the argument's `Span.offset/length`
    /// for the `expr` field (terminal mode) and `"expr":"…"` field
    /// (structured mode). Empty until [`set_source_text`] is called by
    /// the CLI; tests may leave it empty in which case `dbg()` falls
    /// back to a placeholder.
    pub(crate) source_text: String,
    /// Format mode for `dbg()` output. `Terminal` (default) prints a
    /// human-readable line; `Json` prints a single JSON object per
    /// call. Selected by the CLI based on `--output=…`. See design.md
    /// § dbg() — Output formats.
    pub(crate) dbg_output_mode: DbgOutputMode,
    /// Per-task identifier for `dbg()` tagging in `par {}` regions.
    /// `None` outside `par {}`; `Some(N)` inside a branch. Allocated
    /// from `task_id_counter` on branch entry; nested `par {}` inside
    /// a branch shadows the parent's id so each `dbg()` reports the
    /// innermost task.
    pub current_task_id: Option<u64>,
    /// Shared monotonic counter for `par {}` task ids. Cloned across
    /// every branch interpreter so nested `par {}` regions allocate
    /// from the same sequence.
    pub(crate) task_id_counter: Arc<AtomicU64>,
    /// Test-only capture buffer for `dbg()` output. When `Some`,
    /// `eval_builtin_dbg` pushes its formatted line here instead of
    /// writing to stderr. Tests inspect this to assert the exact
    /// terminal-mode or JSON-mode output. In `par {}` branches the
    /// parent's buffer is mirrored into each branch and merged on
    /// join (same pattern as `captured_output`).
    pub captured_dbg: Option<Vec<String>>,
}

/// Format mode for [`Interpreter`]'s `dbg()` output. See design.md §
/// dbg() — Output formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbgOutputMode {
    /// Human-readable single line: `[task:N file:line] expr = value`
    /// (task prefix omitted outside `par {}`).
    Terminal,
    /// One JSON object per call:
    /// `{"kind":"dbg","task_id":N,"file":"…","line":N,"expr":"…","type":"…","value":"…"}`.
    Json,
}

/// JSON-string escape with surrounding quotes. Used by the `dbg()`
/// structured output mode. Kept private to interpreter.rs; the cli /
/// doc modules each carry their own copies for the same reason
/// (decoupling, tiny helper).
pub(crate) fn dbg_json_escape(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04x}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Convert a PascalCase identifier to lower_snake_case.
/// `InProgress` → `in_progress`, `Up` → `up`, `HTTPError` → `h_t_t_p_error`.
pub(crate) fn pascal_to_snake(s: &str) -> String {
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
            cancel_flag: None,
            drop_trace: Vec::new(),
            source_text: String::new(),
            dbg_output_mode: DbgOutputMode::Terminal,
            current_task_id: None,
            task_id_counter: Arc::new(AtomicU64::new(0)),
            captured_dbg: None,
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

    /// Set the program's full source text. Used by `dbg()` to slice
    /// the argument's source span into the `expr` field. The CLI
    /// supplies this from the file already read into memory.
    pub fn set_source_text(&mut self, src: &str) {
        self.source_text = src.to_string();
    }

    /// Select `dbg()` output format. Defaults to [`DbgOutputMode::Terminal`].
    pub fn set_dbg_output_mode(&mut self, mode: DbgOutputMode) {
        self.dbg_output_mode = mode;
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

        // Register built-in comparison-Ordering enum variants
        // (Less / Equal / Greater — returned by `Ord.cmp`).
        for variant in ["Less", "Equal", "Greater"] {
            self.env.define(
                format!("Ordering.{}", variant),
                Value::EnumVariant {
                    enum_name: "Ordering".to_string(),
                    variant: variant.to_string(),
                    data: EnumData::Unit,
                },
            );
        }
        // Register built-in MemoryOrdering enum variants
        // (Relaxed / Acquire / Release / AcqRel / SeqCst — used by Atomic[T]).
        for variant in ["Relaxed", "Acquire", "Release", "AcqRel", "SeqCst"] {
            self.env.define(
                format!("MemoryOrdering.{}", variant),
                Value::EnumVariant {
                    enum_name: "MemoryOrdering".to_string(),
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

        // Register impl-method bodies from baked stdlib source. The
        // typechecker reads these via `register_baked_stdlib`; the
        // interpreter does the same dispatch through `Value::Function`
        // entries keyed by `Type.method`. Methods carrying
        // `#[compiler_builtin]` are skipped — their bodies are
        // placeholders, and the real dispatch lives in the path-string
        // match earlier in `eval_call` (`Stats.sum`, `Regex.compile`, …)
        // which fires before the env.get(method_key) fallback. Baked is
        // registered before user items so user-program impls can
        // override via last-write-wins on env.define.
        let baked_items: Vec<Item> = crate::prelude::STDLIB_PROGRAMS
            .iter()
            .flat_map(|(_, p)| p.items.iter().cloned())
            .collect();
        for item in &baked_items {
            match item {
                Item::ImplBlock(imp) => {
                    self.register_impl_methods(imp, /* skip_compiler_builtin = */ true);
                }
                // Register baked-stdlib enum unit variants under their
                // qualified path (e.g. `IoError.NotFound`, `VarError.NotPresent`)
                // so they can be used as expressions, peer to the
                // hand-registered `Ordering.Less` / `MemoryOrdering.Relaxed`
                // entries above. Without this, baked enums are pattern-only
                // (match arms work because the resolver tags them, but
                // construction `let e = IoError.NotFound` fails the
                // env.get(path) lookup in `eval_path`). Tuple/struct variants
                // are handled at call sites, same as user-program enums.
                Item::EnumDef(e) => {
                    for variant in &e.variants {
                        if let VariantKind::Unit = variant.kind {
                            self.env.define(
                                format!("{}.{}", e.name, variant.name),
                                Value::EnumVariant {
                                    enum_name: e.name.clone(),
                                    variant: variant.name.clone(),
                                    data: EnumData::Unit,
                                },
                            );
                        }
                    }
                }
                _ => {}
            }
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
                    self.register_impl_methods(imp, /* skip_compiler_builtin = */ false);
                }
                _ => {}
            }
        }
    }

    /// Walk an impl block's methods and register each as a `Value::Function`
    /// keyed by `Type.method` in the interpreter env. Used by both the user-
    /// program walk and the baked-stdlib walk.
    ///
    /// `skip_compiler_builtin` is `true` for baked source: methods marked
    /// `#[compiler_builtin]` carry placeholder bodies, and dispatch for those
    /// happens in `eval_call`'s path-string match (`Stats.sum`, …) which
    /// fires before the env.get(method_key) fallback. Registering the
    /// placeholder body would still work — the real path-string match
    /// shadows it — but skipping is cleaner.
    fn register_impl_methods(&mut self, imp: &ImplBlock, skip_compiler_builtin: bool) {
        let type_name = match &imp.target_type.kind {
            TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
            _ => return,
        };
        for item in &imp.items {
            let ImplItem::Method(method) = item else {
                continue;
            };
            if skip_compiler_builtin
                && method
                    .attributes
                    .iter()
                    .any(|a| a.name == "compiler_builtin")
            {
                continue;
            }
            let method_key = format!("{}.{}", type_name, method.name);
            // For methods with a receiver, prepend a `self` binding pattern
            // so the unified Call dispatch can bind args[0] to `self` without
            // special-casing. Associated functions (no self_param) stay as-is.
            let mut patterns: Vec<Pattern> = Vec::new();
            if method.self_param.is_some() {
                patterns.push(Pattern {
                    span: method.span.clone(),
                    kind: PatternKind::Binding("self".to_string()),
                });
            }
            patterns.extend(method.params.iter().map(|p| p.pattern.clone()));
            // `self` has no default; align defaults with the extended
            // pattern list (None for the self slot).
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
                    Err(
                        cf @ (ControlFlow::ExitUnwind { .. }
                        | ControlFlow::RuntimeError
                        | ControlFlow::Cancelled),
                    ) => {
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

    // ── Block & Statement evaluation ──────────────────────────────

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

    fn value_type_name(&self, val: &Value) -> String {
        match val {
            Value::Struct { name, .. } => name.clone(),
            Value::SharedStruct(inner) => inner.name.clone(),
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

    fn find_struct_def(&self, name: &str) -> Option<&StructDef> {
        for item in &self.program.items {
            if let Item::StructDef(s) = item {
                if s.name == name {
                    return Some(s);
                }
            }
        }
        None
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

    /// Read a field from a struct value. Out of line from `eval_expr_inner`
    /// to keep the recursive evaluator's stack frame small.
    fn read_field(&mut self, obj: Value, field: &str, span: &Span) -> Value {
        match obj {
            Value::Struct { fields, .. } => fields.get(field).cloned().unwrap_or_else(|| {
                unreachable!(
                    "field '{}' not found at {}:{}; should be caught by typechecker",
                    field, span.line, span.column
                )
            }),
            Value::SharedStruct(inner) => {
                if let Some(v) = inner.immutable_fields.get(field) {
                    return v.clone();
                }
                if let Some(cell) = inner.mut_fields.get(field) {
                    // Spec: reads are shared; multiple simultaneous
                    // readers OK. `try_read` fails iff a writer is
                    // active — runtime panic on conflict.
                    match cell.value.try_read() {
                        Ok(guard) => return guard.clone(),
                        Err(_) => {
                            return self.record_runtime_error(
                                format!(
                                    "shared struct field '{}.{}' read while a write borrow is active",
                                    inner.name, field
                                ),
                                span,
                            );
                        }
                    }
                }
                // Weak field reads are the upgrade point per spec
                // § Shared Types — Weak references. Yields
                // `Some(strong_ref)` if the referent is alive (RC
                // bumped), or `None` if it has been deallocated.
                if let Some(weak) = inner.weak_immutable_fields.get(field) {
                    return upgrade_weak_to_option(weak);
                }
                if let Some(slot) = inner.weak_mut_fields.get(field) {
                    match slot.try_read() {
                        Ok(guard) => return upgrade_weak_to_option(&guard),
                        Err(_) => {
                            return self.record_runtime_error(
                                format!(
                                    "shared struct field '{}.{}' read while a write borrow is active",
                                    inner.name, field
                                ),
                                span,
                            );
                        }
                    }
                }
                unreachable!(
                    "field '{}' not found on shared struct '{}' at {}:{}; should be caught by typechecker",
                    field, inner.name, span.line, span.column
                )
            }
            _ => unreachable!(
                "field access on non-struct at {}:{}; should be caught by typechecker",
                span.line, span.column
            ),
        }
    }

    /// Build a struct literal value, dispatching on `is_shared` from the
    /// struct's definition. Out of line from `eval_expr_inner` to keep
    /// the recursive evaluator's stack frame small (debug builds default
    /// test stack is 2 MB; deep `fib`-style recursion is sensitive).
    fn eval_struct_literal(
        &mut self,
        path: &[String],
        fields: &[FieldInit],
        spread: Option<&Expr>,
    ) -> Value {
        let name = path.last().cloned().unwrap_or_default();
        let mut field_vals: HashMap<String, Value> = HashMap::new();
        // Weak handles encountered via spread or explicit field init are
        // routed here so the construction step can re-store them as
        // `Weak<SharedStructInner>` without an upgrade-then-downgrade
        // round trip (which would lose the "is the referent alive"
        // signal at the spread point).
        let mut weak_overrides: HashMap<String, std::sync::Weak<SharedStructInner>> =
            HashMap::new();
        if let Some(spread_expr) = spread {
            match self.eval_expr_inner(spread_expr) {
                Value::Struct {
                    fields: base_fields,
                    ..
                } => {
                    field_vals = base_fields;
                }
                Value::SharedStruct(inner) => {
                    for (k, v) in &inner.immutable_fields {
                        field_vals.insert(k.clone(), v.clone());
                    }
                    for (k, cell) in &inner.mut_fields {
                        let v = cell.value.try_read().expect(
                            "shared struct field write-locked during spread — unreachable in single-task interpreter",
                        );
                        field_vals.insert(k.clone(), v.clone());
                    }
                    for (k, weak) in &inner.weak_immutable_fields {
                        weak_overrides.insert(k.clone(), weak.clone());
                    }
                    for (k, slot) in &inner.weak_mut_fields {
                        let weak = slot.try_read().expect(
                            "shared struct weak field write-locked during spread — unreachable in single-task interpreter",
                        );
                        weak_overrides.insert(k.clone(), weak.clone());
                    }
                }
                _ => {}
            }
        }
        for field in fields {
            let val = self.eval_expr_inner(&field.value);
            field_vals.insert(field.name.clone(), val);
        }
        if let Some(def) = self.find_struct_def(&name) {
            if def.is_shared {
                let mut_field_names: HashSet<String> = def
                    .fields
                    .iter()
                    .filter(|f| f.is_mut)
                    .map(|f| f.name.clone())
                    .collect();
                let weak_field_names: HashSet<String> = def
                    .fields
                    .iter()
                    .filter(|f| matches!(f.ty.kind, TypeKind::Weak(_)))
                    .map(|f| f.name.clone())
                    .collect();
                let mut immutable_fields: HashMap<String, Value> = HashMap::new();
                let mut mut_fields: HashMap<String, FieldCell> = HashMap::new();
                let mut weak_immutable_fields: HashMap<String, std::sync::Weak<SharedStructInner>> =
                    HashMap::new();
                let mut weak_mut_fields: HashMap<
                    String,
                    RwLock<std::sync::Weak<SharedStructInner>>,
                > = HashMap::new();
                for (k, v) in field_vals {
                    let is_mut = mut_field_names.contains(&k);
                    let is_weak = weak_field_names.contains(&k);
                    if is_weak {
                        // Spec § Shared Types: assignment to a weak
                        // field accepts a strong reference and
                        // downgrades. Non-shared rhs is a typechecker
                        // error; we record a runtime error as
                        // defense-in-depth. Spread-only weak fields
                        // (no explicit init in the literal) are
                        // handled by the `weak_overrides` drain below.
                        let weak = match &v {
                            Value::SharedStruct(arc) => Arc::downgrade(arc),
                            _ => {
                                self.runtime_errors.push(RuntimeError {
                                    message: format!(
                                        "weak field '{}.{}' initialized with non-shared value",
                                        name, k
                                    ),
                                    span: Span::default(),
                                    left: None,
                                    right: None,
                                });
                                std::sync::Weak::new()
                            }
                        };
                        weak_overrides.insert(k, weak);
                    } else if is_mut {
                        mut_fields.insert(k, FieldCell::new(v));
                    } else {
                        immutable_fields.insert(k, v);
                    }
                }
                for (k, weak) in weak_overrides {
                    if mut_field_names.contains(&k) {
                        weak_mut_fields.insert(k, RwLock::new(weak));
                    } else {
                        weak_immutable_fields.insert(k, weak);
                    }
                }
                return Value::SharedStruct(Arc::new(SharedStructInner {
                    name,
                    immutable_fields,
                    mut_fields,
                    weak_immutable_fields,
                    weak_mut_fields,
                }));
            }
        }
        Value::Struct {
            name,
            fields: field_vals,
        }
    }

    fn set_field(&mut self, object: &Expr, field: &str, val: Value) {
        let target_name: Option<&str> = match &object.kind {
            ExprKind::Identifier(name) => Some(name.as_str()),
            ExprKind::SelfValue => Some("self"),
            _ => None,
        };
        if let Some(name) = target_name {
            match self.env.get(name) {
                Some(Value::Struct { name: sn, fields }) => {
                    let mut fields = fields;
                    fields.insert(field.to_string(), val);
                    self.env.set(name, Value::Struct { name: sn, fields });
                }
                Some(Value::SharedStruct(inner)) => {
                    // Aliasing: `inner` is a clone of the Arc held by `name`'s
                    // slot. Both point to the same allocation; mutating
                    // through `inner` is visible to every other holder.
                    if inner.immutable_fields.contains_key(field)
                        || inner.weak_immutable_fields.contains_key(field)
                    {
                        // Defense-in-depth: typechecker already rejects
                        // writes to non-`mut` fields. If we reach here,
                        // the static check missed.
                        self.record_runtime_error(
                            format!(
                                "shared struct field '{}.{}' is not declared mut",
                                inner.name, field
                            ),
                            &object.span,
                        );
                        return;
                    }
                    if let Some(cell) = inner.mut_fields.get(field) {
                        // Spec: writes are exclusive — panic if any other
                        // borrow (read or write) of the same field is
                        // active when a write begins.
                        match cell.value.try_write() {
                            Ok(mut guard) => {
                                *guard = val;
                            }
                            Err(_) => {
                                self.record_runtime_error(
                                    format!(
                                        "shared struct field '{}.{}' write while another borrow is active",
                                        inner.name, field
                                    ),
                                    &object.span,
                                );
                            }
                        }
                    } else if let Some(slot) = inner.weak_mut_fields.get(field) {
                        // Spec § Shared Types: assignment to a weak
                        // field accepts a strong reference and
                        // downgrades it. `Weak::new()` (an empty weak)
                        // is the safe fallback for a non-shared rhs;
                        // typechecker should reject that case but
                        // record a runtime error as defense-in-depth.
                        let weak = match &val {
                            Value::SharedStruct(arc) => Arc::downgrade(arc),
                            _ => {
                                self.record_runtime_error(
                                    format!(
                                        "weak field '{}.{}' assigned a non-shared value",
                                        inner.name, field
                                    ),
                                    &object.span,
                                );
                                std::sync::Weak::new()
                            }
                        };
                        match slot.try_write() {
                            Ok(mut guard) => {
                                *guard = weak;
                            }
                            Err(_) => {
                                self.record_runtime_error(
                                    format!(
                                        "shared struct field '{}.{}' write while another borrow is active",
                                        inner.name, field
                                    ),
                                    &object.span,
                                );
                            }
                        }
                    } else {
                        unreachable!(
                            "shared struct field '{}.{}' not found at {}:{}; should be caught by typechecker",
                            inner.name, field, object.span.line, object.span.column
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn set_index(&mut self, object: &Expr, index: &Expr, val: Value) {
        if let ExprKind::Identifier(name) = &object.kind {
            let idx = self.eval_expr_inner(index);
            match (self.env.get(name), idx) {
                (Some(Value::Array(rc)), Value::Int(i)) => {
                    let mut guard = try_write_or_panic(&rc, name);
                    let i = i as usize;
                    if i < guard.len() {
                        guard[i] = val;
                    }
                }
                (
                    Some(Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    }),
                    Value::Int(i),
                ) => {
                    let mut guard = try_write_or_panic(&storage, name);
                    let i = i as usize;
                    if i < len {
                        guard[start + i] = val;
                    }
                }
                _ => {}
            }
        }
    }

    /// `Vec.filled(n, val)` — produce `n` clones of `val` per
    /// design.md:1631. Extracted to its own frame so the surrounding
    /// path-call match doesn't grow (debug-mode `eval_expr_inner`
    /// recursive callers are stack-budget-tight). Per-slot clone
    /// satisfies the spec's `T: Clone` requirement; negative length
    /// is a runtime error (Kāra has no usize).
    pub(crate) fn eval_vec_filled(&mut self, args: &[CallArg], span: &Span) -> Value {
        let Some(n_arg) = args.first() else {
            return self
                .record_runtime_error("Vec.filled expects 2 arguments (n, val), found 0", span);
        };
        let Some(val_arg) = args.get(1) else {
            return self
                .record_runtime_error("Vec.filled expects 2 arguments (n, val), found 1", span);
        };
        let n_val = self.eval_expr_inner(&n_arg.value);
        let val = self.eval_expr_inner(&val_arg.value);
        let Value::Int(n) = n_val else {
            return self.record_runtime_error("Vec.filled length must be i64", span);
        };
        if n < 0 {
            return self.record_runtime_error("Vec.filled length must be non-negative", span);
        }
        let len = n as usize;
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            // `deep_clone_value` rather than `.clone()` so nested-
            // collection element types (e.g., `Vec[Vec[i64]]`) get
            // independent storage per slot — the derived `Value::Clone`
            // bumps the `Arc<RwLock<...>>` for `Value::Array`, which
            // would alias every entry to the same underlying Vec.
            items.push(deep_clone_value(&val));
        }
        Value::array_of(items)
    }

    /// `VecDeque[T]` mutation methods — `push_back` / `push_front` /
    /// `pop_back` / `pop_front`. Caller already verified the receiver
    /// is `Value::Array`. Extracted so its locals don't bloat
    /// `eval_method_call`'s stack frame.
    pub(crate) fn eval_vec_deque_method(
        &mut self,
        method: &str,
        obj: &Value,
        object: &Expr,
        args: &[CallArg],
    ) -> Value {
        let Value::Array(rc) = obj else {
            return Value::Unit;
        };
        let label = match &object.kind {
            ExprKind::Identifier(n) => n.clone(),
            _ => "<value>".to_string(),
        };
        match method {
            "push_back" => {
                let val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                try_write_or_panic(rc, &label).push(val);
                Value::Unit
            }
            "push_front" => {
                let val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                try_write_or_panic(rc, &label).insert(0, val);
                Value::Unit
            }
            "pop_back" => {
                let popped = try_write_or_panic(rc, &label).pop();
                option_value_from(popped)
            }
            "pop_front" => {
                let mut guard = try_write_or_panic(rc, &label);
                let popped = if guard.is_empty() {
                    None
                } else {
                    Some(guard.remove(0))
                };
                option_value_from(popped)
            }
            _ => Value::Unit,
        }
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::exec::{cancelled_sentinel, ControlFlow, ExitPath};
    use super::value::{EnumData, Value};

    #[test]
    fn cancelled_cf_classifies_with_sentinel_payload() {
        // Sub-step 4 wiring: ControlFlow::Cancelled must map to
        // ExitPath::Cancelled with the Cancelled-sentinel value,
        // so errdefer(e) binds e to the sentinel during the
        // errdefer phase. Deterministic — does not rely on threading.
        let path = ExitPath::classify(&ControlFlow::Cancelled);
        match path {
            ExitPath::Cancelled(Value::EnumVariant {
                enum_name,
                variant,
                data,
            }) => {
                assert_eq!(enum_name, "Cancelled");
                assert_eq!(variant, "Cancelled");
                assert!(matches!(data, EnumData::Unit));
            }
            other => panic!("expected Cancelled(EnumVariant), got {:?}", other),
        }
    }

    #[test]
    fn cancelled_path_is_an_error_path() {
        // Drives the errdefer phase in run_cleanup. Sub-step 4 sets
        // is_error()=true so the errdefer drain executes.
        assert!(ExitPath::Cancelled(cancelled_sentinel()).is_error());
    }
}
