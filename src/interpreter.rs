use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use regex::Regex as RustRegex;

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;
use crate::typechecker::{type_display, TypeCheckResult};

mod exec;
mod helpers;
mod value;

use exec::{
    add_pattern_bindings, collect_free_idents_expr, compute_block_last_use, deep_clone_value,
    option_value_from, push_drops_for_stmt, slice_pattern_view, CleanupAction, ControlFlow, Env,
    ErrDeferEntry, EvalResult, ExitPath,
};
use helpers::{
    base64_decode, base64_encode, decode_err, decode_ok_bytes, decode_ok_string, eval_http_get,
    eval_http_post, eval_stats_fn, hex_decode, hex_encode, io_err_value, io_error_from_std, io_ok,
    kara_json_to_serde_json, make_json_error, serde_json_to_kara_json, url_decode, url_encode,
    value_compare,
};
use value::{
    primitive_const_to_value, try_write_or_panic, upgrade_weak_to_option, EnumData, FieldCell,
    IteratorSource, IteratorStep, OrdValue, SharedStructInner, ERROR_TRACE_MAX_DEPTH,
};
pub use value::{ErrorTraceFrame, RuntimeError, TestOutcome, Value};

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
    /// `par {}` shared cancellation flag. Set by `eval_par_block` on
    /// each branch interpreter; observed by `eval_block_inner` between
    /// top-level statements as a minimal effect-boundary check. When
    /// observed, the running branch raises `ControlFlow::Cancelled`,
    /// which classifies as `ExitPath::Cancelled(sentinel)` so any
    /// `errdefer(e)` in the active scope binds `e` to the sentinel
    /// during the errdefer phase. None outside `par {}` branches.
    cancel_flag: Option<Arc<AtomicBool>>,
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
    source_text: String,
    /// Format mode for `dbg()` output. `Terminal` (default) prints a
    /// human-readable line; `Json` prints a single JSON object per
    /// call. Selected by the CLI based on `--output=…`. See design.md
    /// § dbg() — Output formats.
    dbg_output_mode: DbgOutputMode,
    /// Per-task identifier for `dbg()` tagging in `par {}` regions.
    /// `None` outside `par {}`; `Some(N)` inside a branch. Allocated
    /// from `task_id_counter` on branch entry; nested `par {}` inside
    /// a branch shadows the parent's id so each `dbg()` reports the
    /// innermost task.
    pub current_task_id: Option<u64>,
    /// Shared monotonic counter for `par {}` task ids. Cloned across
    /// every branch interpreter so nested `par {}` regions allocate
    /// from the same sequence.
    task_id_counter: Arc<AtomicU64>,
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
fn dbg_json_escape(s: &str) -> String {
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
                // Short-circuit `and`/`or` are documented design intent
                // (roadmap.md:425, 429) — RHS only evaluates when the
                // LHS doesn't already determine the result. Routed
                // through a helper to keep `eval_expr_inner`'s debug-
                // mode stack frame from bloating recursive callers.
                if matches!(op, BinOp::And | BinOp::Or) {
                    return self.eval_short_circuit(op, left, right, &expr.span);
                }
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

            ExprKind::Path { segments, .. } => {
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
                Value::array_of(vals)
            }

            // Prefix collection literal: `Vec[e1, e2, ...]` / `Array[e1, ...]`
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                let vals: Vec<Value> = items.iter().map(|e| self.eval_expr_inner(e)).collect();
                Value::array_of(vals)
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
                Value::array_of(vec![v; n])
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
            } => self.eval_struct_literal(path, fields, spread.as_deref()),

            // Field access
            ExprKind::FieldAccess { object, field } => {
                // Primitive-type associated constants — `i64.MAX` /
                // `f64.INFINITY` / etc. parse as `FieldAccess(Identifier("i64"),
                // "MAX")`. Intercept before `eval_expr_inner(object)` would
                // panic on the bare primitive identifier (which has no
                // env binding). Falls through to the normal field-access
                // path when the lookup misses (so a regular struct field
                // with the same shape still resolves correctly).
                if let ExprKind::Identifier(name) = &object.kind {
                    if let Some(cv) = crate::prelude::lookup_primitive_const(name, field) {
                        return primitive_const_to_value(cv);
                    }
                }
                let obj = self.eval_expr_inner(object);
                self.read_field(obj, field, &expr.span)
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
                    let (storage, source_len) = match &obj {
                        Value::Array(rc) => (rc.clone(), rc.read().unwrap().len()),
                        Value::Slice {
                            storage,
                            start,
                            len,
                            ..
                        } => {
                            // Re-slicing — produce a window into the same
                            // storage with offset adjustment.
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
                                *len
                            };
                            let end_i = if *inclusive { raw_end + 1 } else { raw_end };
                            if start_i > end_i || end_i > *len {
                                return self.record_runtime_error(
                                    format!(
                                        "slice bounds {}..{} out of range (len {})",
                                        start_i, end_i, len,
                                    ),
                                    &expr.span,
                                );
                            }
                            return Value::Slice {
                                storage: storage.clone(),
                                start: start + start_i,
                                len: end_i - start_i,
                                mutable: false,
                            };
                        }
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
                        source_len
                    };
                    let end_i = if *inclusive { raw_end + 1 } else { raw_end };
                    if start_i > end_i || end_i > source_len {
                        return self.record_runtime_error(
                            format!(
                                "slice bounds {}..{} out of range (len {})",
                                start_i, end_i, source_len,
                            ),
                            &expr.span,
                        );
                    }
                    return Value::Slice {
                        storage,
                        start: start_i,
                        len: end_i - start_i,
                        mutable: false,
                    };
                }
                let obj = self.eval_expr_inner(object);
                let idx = self.eval_expr_inner(index);
                match (&obj, &idx) {
                    (Value::Array(rc), Value::Int(i)) => {
                        let i = *i as usize;
                        let vals = rc.read().unwrap();
                        let len = vals.len();
                        vals.get(i).cloned().unwrap_or_else(|| {
                            self.record_runtime_error(
                                format!("index {} out of bounds (len {})", i, len),
                                &expr.span,
                            )
                        })
                    }
                    (
                        Value::Slice {
                            storage,
                            start,
                            len,
                            ..
                        },
                        Value::Int(i),
                    ) => {
                        let i = *i as usize;
                        if i >= *len {
                            return self.record_runtime_error(
                                format!("index {} out of bounds (len {})", i, len),
                                &expr.span,
                            );
                        }
                        let vals = storage.read().unwrap();
                        vals[start + i].clone()
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
                    Value::Array(rc) => match Arc::try_unwrap(rc) {
                        Ok(cell) => cell.into_inner().unwrap(),
                        Err(rc) => rc.read().unwrap().clone(),
                    },
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => storage.read().unwrap()[start..start + len].to_vec(),
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
                    // String iterates per Unicode scalar value, matching the
                    // canonical `s.chars()` surface — design.md § Character
                    // type (line 2299) pins `for c in s` and `s.chars()` as
                    // semantic peers.
                    Value::String(s) => s.chars().map(Value::Char).collect(),
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

            // Range — evaluates to a `Value::Iterator` for bounded ranges
            // (so `(0..10).step_by(2)` and the rest of the adaptor surface
            // dispatch through the same path as `xs.iter()`), or a runtime
            // error for unbounded forms used as values. The for-loop iterable
            // path drains `Value::Iterator` via `iterator_step` (see the
            // `ExprKind::For` arm above), so `for x in 0..n { ... }` keeps
            // working unchanged.
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
                        Value::Iterator {
                            source: IteratorSource::Eager { items, cursor: 0 },
                            steps: Vec::new(),
                        }
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

            ExprKind::Try(block) => {
                // v1 stub — typechecker rejects every `try { ... }` use
                // with E_TRY_BLOCK_NOT_IMPLEMENTED_YET; the interpreter
                // never sees a valid try block in a typechecker-clean
                // program. We still walk the body for any debug-mode use
                // that bypasses the typechecker so the form has a defined
                // shape until P1 ships ?-retargeting.
                match self.eval_block_inner(block) {
                    Ok(v) => v,
                    Err(cf) => self.set_cf(cf),
                }
            }

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

            // LBC4 — `label: { body }`. Routes the existing
            // `ControlFlow::Break { label, value }` signal: a `break label
            // expr` inside the body matches by label, returns the value
            // (or `Value::Unit` when bare `break label`); any non-matching
            // control-flow signal (outer-label break, return, cancel,
            // exit, runtime error) propagates unchanged. No new
            // `ControlFlow` variants needed.
            ExprKind::LabeledBlock { label, body, .. } => match self.eval_block_inner(body) {
                Ok(v) => v,
                Err(ControlFlow::Break {
                    label: Some(ref l),
                    ref value,
                }) if l == label => value.clone().unwrap_or(Value::Unit),
                Err(cf) => self.set_cf(cf),
            },

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
    fn eval_par_block(&mut self, block: &Block) -> EvalResult {
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
        if let ExprKind::Path { segments, .. } = &callee.kind {
            if segments.len() == 2 && self.effect_resources.contains(&segments[0]) {
                return self.eval_resource_method(&segments[0], &segments[1], args, span);
            }
        }

        // Built-in path-qualified functions (e.g. process.exit, Ordering.Relaxed, F64.from)
        if let ExprKind::Path { segments, .. } = &callee.kind {
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
                // Debugger Contract slice 5: `std.runtime` introspection
                // surface (`runtime/stdlib/runtime.kara`). The tree-walk
                // interpreter has its own par-block evaluation path and does
                // not construct `KaracFrame` / `ACTIVE_FRAMES` state, so all
                // three return the empty / false form per design.md's
                // try-then-degrade contract — generic tooling sees no frames
                // and falls back to an alternate code path. Real values flow
                // through the codegen-side dispatch in `compile_assoc_call`,
                // which calls into `karac_runtime_*` extern fns to read the
                // slice-3 globals + slice-4 active-frames registry.
                "Runtime.has_debug_metadata" => {
                    return Value::Bool(false);
                }
                "Runtime.list_par_blocks" | "Runtime.list_tasks" => {
                    return Value::array_of(Vec::new());
                }
                // Slice F (`std.json`): `Json.parse(s)` parses via
                // `serde_json` and builds a Kāra `Json` enum tree. The
                // runtime crate exposes the same impl through
                // `karac_runtime_json_parse` for the codegen path; the
                // interpreter calls `serde_json` directly to avoid the
                // FFI cross-over (both link the same crate). Returns
                // `Result[Json, JsonError]` per the signature in
                // `runtime/stdlib/json.kara`.
                "Json.parse" => {
                    let s = if let Some(arg) = args.first() {
                        match self.eval_expr_inner(&arg.value) {
                            Value::String(s) => s,
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    return match serde_json::from_str::<serde_json::Value>(&s) {
                        Ok(v) => Value::EnumVariant {
                            enum_name: "Result".to_string(),
                            variant: "Ok".to_string(),
                            data: EnumData::Tuple(vec![serde_json_to_kara_json(&v)]),
                        },
                        Err(e) => Value::EnumVariant {
                            enum_name: "Result".to_string(),
                            variant: "Err".to_string(),
                            data: EnumData::Tuple(vec![make_json_error(&e)]),
                        },
                    };
                }
                "Map.new" => {
                    return Value::Map(Vec::new());
                }
                "Vec.new" => {
                    return Value::array_of(Vec::new());
                }
                // `VecDeque.new() -> VecDeque[T]` — runtime shape mirrors
                // `Vec.new`'s shared `Arc<RwLock<Vec<Value>>>` storage.
                // Front-end ops (`push_front`/`pop_front`) translate to
                // `Vec::insert(0, …)` / `Vec::remove(0)` at the
                // method-dispatch layer (see `eval_method_call`'s `_front`
                // arms). The asymptotic O(n) cost is acceptable for the
                // tree-walk interpreter — perf-relevant workloads run
                // through codegen, where a real `VecDeque` lowering lands
                // as a peer slice.
                "VecDeque.new" => {
                    return Value::array_of(Vec::new());
                }
                // `Vec.filled(n: i64, val: T) -> Vec[T] where T: Clone` —
                // spec at design.md:1631. Routed through a helper so
                // its locals don't bloat the surrounding `eval_call`
                // match's debug-mode stack frame (the inline form
                // overflowed `test_e2e_fibonacci`, same shape as the
                // `and`/`or` short-circuit fix).
                "Vec.filled" => return self.eval_vec_filled(args, span),
                // `Vec.from_slice(src) -> Vec[T]` — clone the source's
                // elements into a fresh Vec. Mirrors codegen's bulk-copy
                // shape; here the storage isn't shared so a fresh clone
                // of the inner Vec<Value> is correct (matches what the
                // `push_loop_from_iter` shape produces semantically).
                "Vec.from_slice" => {
                    let src = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    let elements: Vec<Value> = match src {
                        Value::Array(rc) => rc.read().unwrap().clone(),
                        Value::Slice {
                            storage,
                            start,
                            len,
                            ..
                        } => storage.read().unwrap()[start..start + len].to_vec(),
                        _ => Vec::new(),
                    };
                    return Value::array_of(elements);
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
                            Value::Array(rc) => rc
                                .read()
                                .unwrap()
                                .iter()
                                .map(|v| match v {
                                    Value::Float(f) => *f,
                                    Value::Int(i) => *i as f64,
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
                            Value::Array(rc) => rc
                                .read()
                                .unwrap()
                                .iter()
                                .map(|v| match v {
                                    Value::Int(i) => *i as u8,
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
                    // Check for Ordering / MemoryOrdering qualified-variant pattern
                    if segments.len() == 2
                        && (segments[0] == "Ordering" || segments[0] == "MemoryOrdering")
                    {
                        return Value::EnumVariant {
                            enum_name: segments[0].clone(),
                            variant: segments[1].clone(),
                            data: EnumData::Unit,
                        };
                    }
                    // Slice F (`std.json`): qualified `Json.Variant(args)`
                    // construction. The bare-name path (`Bool(true)`)
                    // collides with `bool::from`, so users must qualify
                    // every Json variant. The interpreter's generic
                    // `find_enum_for_variant` fallback only fires when
                    // the callee evaluates to a non-callable, but
                    // `eval_expr_inner(Path)` panics before that on
                    // unknown enum variants — so we build the variant
                    // directly here. Mirrors the Ordering arm above.
                    if segments.len() == 2 && segments[0] == "Json" {
                        let variant = segments[1].clone();
                        let arg_vals: Vec<Value> = args
                            .iter()
                            .map(|a| self.eval_expr_inner(&a.value))
                            .collect();
                        let data = if variant == "Null" {
                            EnumData::Unit
                        } else {
                            EnumData::Tuple(arg_vals)
                        };
                        return Value::EnumVariant {
                            enum_name: "Json".to_string(),
                            variant,
                            data,
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
                "print" | "println" | "eprintln" => {
                    return self.eval_builtin_print(name, args, span);
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
                    ExprKind::Path { segments, .. } => segments.last().cloned().unwrap_or_default(),
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
            ExprKind::Path { segments, .. } => segments.as_slice() == ["with_provider"],
            _ => false,
        };
        if !is_with_provider {
            return None;
        }
        let resource = match &index.kind {
            ExprKind::Identifier(n) => n.clone(),
            ExprKind::Path { segments, .. } => segments.last().cloned()?,
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
        // Stateful steps (Enumerate / Take / Skip) mutate this clone in
        // place; whatever state changes survive — closure rejection,
        // `take` exhaustion, multiple pulls in one call — get written
        // back to the iterator's stored chain just before return.
        let mut steps = match iter {
            Value::Iterator { steps, .. } => steps.clone(),
            _ => return None,
        };
        let yielded = 'pull: loop {
            let Some(raw_item) = self.pull_source(iter) else {
                break 'pull None;
            };
            let mut item = raw_item;
            let mut keep = true;
            let mut stop = false;
            for step in steps.iter_mut() {
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
                    IteratorStep::Enumerate(idx) => {
                        item = Value::Tuple(vec![Value::Int(*idx as i64), item]);
                        *idx += 1;
                    }
                    IteratorStep::Take(remaining) => {
                        if *remaining == 0 {
                            stop = true;
                            keep = false;
                            break;
                        }
                        *remaining -= 1;
                    }
                    IteratorStep::Skip(remaining) => {
                        if *remaining > 0 {
                            *remaining -= 1;
                            keep = false;
                            break;
                        }
                    }
                    IteratorStep::TakeWhile { pred, done } => {
                        if *done {
                            // Sticky-stop: predicate already tripped on
                            // an earlier element, so every subsequent
                            // pull short-circuits without firing pred.
                            stop = true;
                            keep = false;
                            break;
                        }
                        let result = self.invoke_function_value(pred.clone(), vec![item.clone()]);
                        if !matches!(result, Value::Bool(true)) {
                            *done = true;
                            stop = true;
                            keep = false;
                            break;
                        }
                    }
                    IteratorStep::SkipWhile { pred, done } => {
                        if *done {
                            // Sticky-pass: predicate failed on an
                            // earlier element, so every subsequent
                            // item goes through unconditionally.
                            continue;
                        }
                        let result = self.invoke_function_value(pred.clone(), vec![item.clone()]);
                        if matches!(result, Value::Bool(true)) {
                            keep = false;
                            break;
                        }
                        *done = true;
                    }
                    IteratorStep::StepBy { n, remaining_skip } => {
                        if *remaining_skip > 0 {
                            *remaining_skip -= 1;
                            keep = false;
                            break;
                        }
                        // Yield this item, then skip the next n-1.
                        // n ≥ 1 by construction (clamped at dispatch),
                        // so the subtraction never underflows.
                        *remaining_skip = *n - 1;
                    }
                    IteratorStep::Inspect(f) => {
                        // Side-effect-only step: invoke f and discard
                        // the result; the item passes through.
                        self.invoke_function_value(f.clone(), vec![item.clone()]);
                    }
                    IteratorStep::Scan { f, state, done } => {
                        if *done {
                            stop = true;
                            keep = false;
                            break;
                        }
                        let result = self
                            .invoke_function_value(f.clone(), vec![state.clone(), item.clone()]);
                        // Closure returns Option<(A, U)>: Some carries
                        // (new_state, yielded); None signals stop.
                        let parsed = match result {
                            Value::EnumVariant {
                                variant,
                                data: EnumData::Tuple(mut vals),
                                ..
                            } if variant == "Some" && vals.len() == 1 => match vals.remove(0) {
                                Value::Tuple(mut tuple) if tuple.len() == 2 => {
                                    let yielded = tuple.remove(1);
                                    let new_state = tuple.remove(0);
                                    Some((new_state, yielded))
                                }
                                _ => None,
                            },
                            _ => None,
                        };
                        match parsed {
                            Some((new_state, yielded)) => {
                                *state = new_state;
                                item = yielded;
                            }
                            None => {
                                *done = true;
                                stop = true;
                                keep = false;
                                break;
                            }
                        }
                    }
                }
            }
            if stop {
                // `take` exhaustion — drain the source so subsequent
                // calls also return None without touching downstream
                // adaptor state.
                self.drain_source(iter);
                break 'pull None;
            }
            if keep {
                break 'pull Some(item);
            }
        };
        // Write the (possibly mutated) step chain back so per-call
        // counter state persists across `next()` pulls.
        if let Value::Iterator {
            steps: stored_steps,
            ..
        } = iter
        {
            *stored_steps = steps;
        }
        yielded
    }

    /// Pull the next raw item from an iterator's source layer. Eager
    /// walks `items[cursor]`; Chain advances through its parts, calling
    /// `iterator_step` recursively on each so per-part adaptor chains
    /// fire; Zip pulls from both sides in lockstep, yielding a tuple or
    /// stopping when either side ends.
    fn pull_source(&mut self, iter: &mut Value) -> Option<Value> {
        let Value::Iterator { source, .. } = iter else {
            return None;
        };
        match source {
            IteratorSource::Eager { items, cursor } => {
                if *cursor >= items.len() {
                    return None;
                }
                let it = items[*cursor].clone();
                *cursor += 1;
                Some(it)
            }
            IteratorSource::Chain { .. } => {
                // Walk the current part until it yields or exhausts; on
                // exhaust, advance to the next. Take parts out of the
                // source while recursing so we can pass `&mut self` to
                // iterator_step without aliasing the iter binding.
                loop {
                    let Value::Iterator {
                        source: IteratorSource::Chain { parts, current },
                        ..
                    } = iter
                    else {
                        return None;
                    };
                    if *current >= parts.len() {
                        return None;
                    }
                    let idx = *current;
                    let mut part = std::mem::replace(&mut parts[idx], Value::Unit);
                    let yielded = self.iterator_step(&mut part);
                    let Value::Iterator {
                        source: IteratorSource::Chain { parts, current },
                        ..
                    } = iter
                    else {
                        return None;
                    };
                    parts[idx] = part;
                    if yielded.is_some() {
                        return yielded;
                    }
                    *current += 1;
                }
            }
            IteratorSource::Zip { .. } => {
                // Take both sides out so we can pass &mut self into
                // iterator_step twice without aliasing the iter binding.
                let (mut left, mut right) = if let Value::Iterator {
                    source: IteratorSource::Zip { left, right },
                    ..
                } = iter
                {
                    (
                        std::mem::replace(left.as_mut(), Value::Unit),
                        std::mem::replace(right.as_mut(), Value::Unit),
                    )
                } else {
                    return None;
                };
                let l = self.iterator_step(&mut left);
                let r = self.iterator_step(&mut right);
                if let Value::Iterator {
                    source:
                        IteratorSource::Zip {
                            left: l_box,
                            right: r_box,
                        },
                    ..
                } = iter
                {
                    **l_box = left;
                    **r_box = right;
                }
                match (l, r) {
                    (Some(a), Some(b)) => Some(Value::Tuple(vec![a, b])),
                    _ => None,
                }
            }
            IteratorSource::FlatMap { .. } => {
                // Drain the in-flight inner iterator first; if it
                // yields, that's our item. If exhausted, advance the
                // outer (recursively iterator_step on it), apply f to
                // the outer item, store the resulting iterator as the
                // new inner, and retry. Same `mem::replace` ceremony
                // as Zip: pull each sub-iterator out of the source,
                // recurse with `&mut self`, write back.
                loop {
                    let inner_yield = if let Value::Iterator {
                        source: IteratorSource::FlatMap { current_inner, .. },
                        ..
                    } = iter
                    {
                        if let Some(boxed) = current_inner.as_mut() {
                            let mut inner = std::mem::replace(boxed.as_mut(), Value::Unit);
                            let yielded = self.iterator_step(&mut inner);
                            if let Value::Iterator {
                                source: IteratorSource::FlatMap { current_inner, .. },
                                ..
                            } = iter
                            {
                                if let Some(boxed) = current_inner.as_mut() {
                                    **boxed = inner;
                                }
                            }
                            Some(yielded)
                        } else {
                            None
                        }
                    } else {
                        return None;
                    };
                    if let Some(Some(v)) = inner_yield {
                        return Some(v);
                    }
                    if let Value::Iterator {
                        source: IteratorSource::FlatMap { current_inner, .. },
                        ..
                    } = iter
                    {
                        *current_inner = None;
                    }
                    let outer_yield = if let Value::Iterator {
                        source: IteratorSource::FlatMap { outer, .. },
                        ..
                    } = iter
                    {
                        let mut o = std::mem::replace(outer.as_mut(), Value::Unit);
                        let yielded = self.iterator_step(&mut o);
                        if let Value::Iterator {
                            source: IteratorSource::FlatMap { outer, .. },
                            ..
                        } = iter
                        {
                            **outer = o;
                        }
                        yielded
                    } else {
                        return None;
                    };
                    let item = outer_yield?;
                    let f_clone = if let Value::Iterator {
                        source: IteratorSource::FlatMap { f, .. },
                        ..
                    } = iter
                    {
                        (**f).clone()
                    } else {
                        return None;
                    };
                    let new_inner = self.invoke_function_value(f_clone, vec![item]);
                    if !matches!(new_inner, Value::Iterator { .. }) {
                        return None;
                    }
                    if let Value::Iterator {
                        source: IteratorSource::FlatMap { current_inner, .. },
                        ..
                    } = iter
                    {
                        *current_inner = Some(Box::new(new_inner));
                    }
                }
            }
            IteratorSource::Cycle { .. } => {
                // Pull from `current`. If yielded, return. If
                // exhausted, replace `current` with a fresh
                // `template.clone()` and try once more — if THAT
                // also yields None, the template is empty; set
                // `exhausted = true` and stop forever (avoids the
                // infinite-empty-loop trap).
                if let Value::Iterator {
                    source: IteratorSource::Cycle { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let first = if let Value::Iterator {
                    source: IteratorSource::Cycle { current, .. },
                    ..
                } = iter
                {
                    let mut c = std::mem::replace(current.as_mut(), Value::Unit);
                    let y = self.iterator_step(&mut c);
                    if let Value::Iterator {
                        source: IteratorSource::Cycle { current, .. },
                        ..
                    } = iter
                    {
                        **current = c;
                    }
                    y
                } else {
                    return None;
                };
                if first.is_some() {
                    return first;
                }
                // Reset to a fresh template clone.
                let fresh = if let Value::Iterator {
                    source: IteratorSource::Cycle { template, .. },
                    ..
                } = iter
                {
                    (**template).clone()
                } else {
                    return None;
                };
                if let Value::Iterator {
                    source: IteratorSource::Cycle { current, .. },
                    ..
                } = iter
                {
                    **current = fresh;
                }
                let second = if let Value::Iterator {
                    source: IteratorSource::Cycle { current, .. },
                    ..
                } = iter
                {
                    let mut c = std::mem::replace(current.as_mut(), Value::Unit);
                    let y = self.iterator_step(&mut c);
                    if let Value::Iterator {
                        source: IteratorSource::Cycle { current, .. },
                        ..
                    } = iter
                    {
                        **current = c;
                    }
                    y
                } else {
                    return None;
                };
                if second.is_some() {
                    return second;
                }
                // Template is empty — sticky-stop.
                if let Value::Iterator {
                    source: IteratorSource::Cycle { exhausted, .. },
                    ..
                } = iter
                {
                    *exhausted = true;
                }
                None
            }
            IteratorSource::Peekable { .. } => {
                // Drain the buffered slot first; on miss, recurse into
                // `inner` via iterator_step. `mem::replace` ceremony
                // mirrors Chain/Zip so we can pass `&mut self` into
                // iterator_step without aliasing the iter binding.
                if let Value::Iterator {
                    source: IteratorSource::Peekable { buffered, .. },
                    ..
                } = iter
                {
                    if let Some(boxed) = buffered.take() {
                        return Some(*boxed);
                    }
                }
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return None;
                };
                let yielded = self.iterator_step(&mut inner_taken);
                if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, .. },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                }
                yielded
            }
            IteratorSource::Chunks { .. } => {
                // Pull up to n items from inner; emit a fresh Vec.
                // Sticky-stop once we get an empty chunk (inner
                // exhausted with nothing in flight). Heap allocation
                // is the per-chunk Vec; effect-checker carries
                // `allocates(Heap)`.
                if let Value::Iterator {
                    source: IteratorSource::Chunks { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let n = if let Value::Iterator {
                    source: IteratorSource::Chunks { n, .. },
                    ..
                } = iter
                {
                    *n
                } else {
                    return None;
                };
                let mut chunk: Vec<Value> = Vec::with_capacity(n);
                for _ in 0..n {
                    let mut inner_taken = if let Value::Iterator {
                        source: IteratorSource::Chunks { inner, .. },
                        ..
                    } = iter
                    {
                        std::mem::replace(inner.as_mut(), Value::Unit)
                    } else {
                        return None;
                    };
                    let pulled = self.iterator_step(&mut inner_taken);
                    if let Value::Iterator {
                        source: IteratorSource::Chunks { inner, .. },
                        ..
                    } = iter
                    {
                        **inner = inner_taken;
                    }
                    match pulled {
                        Some(v) => chunk.push(v),
                        None => break,
                    }
                }
                if chunk.is_empty() {
                    if let Value::Iterator {
                        source: IteratorSource::Chunks { exhausted, .. },
                        ..
                    } = iter
                    {
                        *exhausted = true;
                    }
                    None
                } else {
                    if chunk.len() < n {
                        if let Value::Iterator {
                            source: IteratorSource::Chunks { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                    }
                    Some(Value::array_of(chunk))
                }
            }
            IteratorSource::Windows { .. } => {
                // Sliding window of size n. First pull primes the
                // buffer by collecting n items; subsequent pulls
                // drop the front and push one new item. If the
                // source has fewer than n items at any priming /
                // refill point, sticky-stop (no partial windows).
                if let Value::Iterator {
                    source: IteratorSource::Windows { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let (n, primed) = if let Value::Iterator {
                    source: IteratorSource::Windows { n, primed, .. },
                    ..
                } = iter
                {
                    (*n, *primed)
                } else {
                    return None;
                };
                if !primed {
                    // Prime: pull n items.
                    let mut filled = 0usize;
                    for _ in 0..n {
                        let mut inner_taken = if let Value::Iterator {
                            source: IteratorSource::Windows { inner, .. },
                            ..
                        } = iter
                        {
                            std::mem::replace(inner.as_mut(), Value::Unit)
                        } else {
                            return None;
                        };
                        let pulled = self.iterator_step(&mut inner_taken);
                        if let Value::Iterator {
                            source: IteratorSource::Windows { inner, .. },
                            ..
                        } = iter
                        {
                            **inner = inner_taken;
                        }
                        match pulled {
                            Some(v) => {
                                if let Value::Iterator {
                                    source: IteratorSource::Windows { buffer, .. },
                                    ..
                                } = iter
                                {
                                    buffer.push(v);
                                    filled += 1;
                                }
                            }
                            None => break,
                        }
                    }
                    if filled < n {
                        if let Value::Iterator {
                            source: IteratorSource::Windows { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                        return None;
                    }
                    if let Value::Iterator {
                        source: IteratorSource::Windows { primed, buffer, .. },
                        ..
                    } = iter
                    {
                        *primed = true;
                        return Some(Value::array_of(buffer.clone()));
                    }
                    return None;
                }
                // Already primed — pull one item and slide.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Windows { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return None;
                };
                let pulled = self.iterator_step(&mut inner_taken);
                if let Value::Iterator {
                    source: IteratorSource::Windows { inner, .. },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                }
                match pulled {
                    Some(v) => {
                        if let Value::Iterator {
                            source: IteratorSource::Windows { buffer, .. },
                            ..
                        } = iter
                        {
                            buffer.remove(0);
                            buffer.push(v);
                            return Some(Value::array_of(buffer.clone()));
                        }
                        None
                    }
                    None => {
                        if let Value::Iterator {
                            source: IteratorSource::Windows { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                        None
                    }
                }
            }
            IteratorSource::ChunkBy { .. } => {
                // Build one group per pull: seed from any pending
                // (item, key) carried over from the previous pull
                // (the lookahead element that triggered the last
                // group boundary), then keep pulling from `inner`
                // and applying `key_fn` until the key changes (stash
                // that item as the next pending) or the inner
                // exhausts (set sticky-exhausted and emit the
                // trailing group). Heap allocation is the per-group
                // `Vec`; effect-checker carries `allocates(Heap)`.
                if let Value::Iterator {
                    source: IteratorSource::ChunkBy { exhausted, .. },
                    ..
                } = iter
                {
                    if *exhausted {
                        return None;
                    }
                } else {
                    return None;
                }
                let mut group: Vec<Value> = Vec::new();
                let mut group_key: Option<Value> = None;
                if let Value::Iterator {
                    source:
                        IteratorSource::ChunkBy {
                            pending_item,
                            pending_key,
                            ..
                        },
                    ..
                } = iter
                {
                    if let (Some(item_box), Some(key_box)) =
                        (pending_item.take(), pending_key.take())
                    {
                        group.push(*item_box);
                        group_key = Some(*key_box);
                    }
                }
                loop {
                    let mut inner_taken = if let Value::Iterator {
                        source: IteratorSource::ChunkBy { inner, .. },
                        ..
                    } = iter
                    {
                        std::mem::replace(inner.as_mut(), Value::Unit)
                    } else {
                        return None;
                    };
                    let pulled = self.iterator_step(&mut inner_taken);
                    if let Value::Iterator {
                        source: IteratorSource::ChunkBy { inner, .. },
                        ..
                    } = iter
                    {
                        **inner = inner_taken;
                    }
                    let Some(item) = pulled else {
                        // Inner exhausted — sticky-stop and emit the
                        // final group if non-empty.
                        if let Value::Iterator {
                            source: IteratorSource::ChunkBy { exhausted, .. },
                            ..
                        } = iter
                        {
                            *exhausted = true;
                        }
                        if group.is_empty() {
                            return None;
                        } else {
                            return Some(Value::array_of(group));
                        }
                    };
                    let key_fn = if let Value::Iterator {
                        source: IteratorSource::ChunkBy { key_fn, .. },
                        ..
                    } = iter
                    {
                        (**key_fn).clone()
                    } else {
                        return None;
                    };
                    let key = self.invoke_function_value(key_fn, vec![item.clone()]);
                    match &group_key {
                        None => {
                            // First element of a fresh group.
                            group.push(item);
                            group_key = Some(key);
                        }
                        Some(prev) if *prev == key => {
                            group.push(item);
                        }
                        Some(_) => {
                            // Key change — stash this item (with its
                            // already-computed key) as the seed for
                            // the next pull, return current group.
                            if let Value::Iterator {
                                source:
                                    IteratorSource::ChunkBy {
                                        pending_item,
                                        pending_key,
                                        ..
                                    },
                                ..
                            } = iter
                            {
                                *pending_item = Some(Box::new(item));
                                *pending_key = Some(Box::new(key));
                            }
                            return Some(Value::array_of(group));
                        }
                    }
                }
            }
        }
    }

    /// Force an iterator's source to "exhausted" — used by `take(0)` so
    /// subsequent pulls return None without re-firing downstream adaptors.
    fn drain_source(&mut self, iter: &mut Value) {
        let Value::Iterator { source, .. } = iter else {
            return;
        };
        match source {
            IteratorSource::Eager { items, cursor } => *cursor = items.len(),
            IteratorSource::Chain { parts, current } => *current = parts.len(),
            IteratorSource::Zip { left, right } => {
                let mut l = std::mem::replace(left.as_mut(), Value::Unit);
                let mut r = std::mem::replace(right.as_mut(), Value::Unit);
                self.drain_source(&mut l);
                self.drain_source(&mut r);
                if let Value::Iterator {
                    source:
                        IteratorSource::Zip {
                            left: l_box,
                            right: r_box,
                        },
                    ..
                } = iter
                {
                    **l_box = l;
                    **r_box = r;
                }
            }
            IteratorSource::FlatMap { outer, .. } => {
                // Drain the outer and clear the in-flight inner;
                // pull_source's loop returns None at the outer-pull
                // step on every subsequent call.
                let mut o = std::mem::replace(outer.as_mut(), Value::Unit);
                self.drain_source(&mut o);
                if let Value::Iterator {
                    source:
                        IteratorSource::FlatMap {
                            outer,
                            current_inner,
                            ..
                        },
                    ..
                } = iter
                {
                    **outer = o;
                    *current_inner = None;
                }
            }
            IteratorSource::Cycle { exhausted, .. } => {
                // Just trip the sticky-stop flag; pull_source's
                // first check returns None on every subsequent call.
                *exhausted = true;
            }
            IteratorSource::Peekable { .. } => {
                // Drain the inner and clear any buffered element. After
                // this, pull_source: buffered is None → falls through
                // to the inner pull which returns None forever.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source: IteratorSource::Peekable { inner, buffered },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    *buffered = None;
                }
            }
            IteratorSource::Chunks { .. } => {
                // Drain the inner and trip sticky-exhausted.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Chunks { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source:
                        IteratorSource::Chunks {
                            inner, exhausted, ..
                        },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    *exhausted = true;
                }
            }
            IteratorSource::Windows { .. } => {
                // Drain the inner, clear the rolling buffer, trip
                // sticky-exhausted.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::Windows { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source:
                        IteratorSource::Windows {
                            inner,
                            buffer,
                            exhausted,
                            ..
                        },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    buffer.clear();
                    *exhausted = true;
                }
            }
            IteratorSource::ChunkBy { .. } => {
                // Drain the inner and trip the sticky-exhausted flag;
                // also clear any in-flight pending so the trailing
                // group isn't emitted after a forced drain.
                let mut inner_taken = if let Value::Iterator {
                    source: IteratorSource::ChunkBy { inner, .. },
                    ..
                } = iter
                {
                    std::mem::replace(inner.as_mut(), Value::Unit)
                } else {
                    return;
                };
                self.drain_source(&mut inner_taken);
                if let Value::Iterator {
                    source:
                        IteratorSource::ChunkBy {
                            inner,
                            pending_item,
                            pending_key,
                            exhausted,
                            ..
                        },
                    ..
                } = iter
                {
                    **inner = inner_taken;
                    *pending_item = None;
                    *pending_key = None;
                    *exhausted = true;
                }
            }
        }
    }

    /// `Peekable.peek()` — look one element ahead without consuming.
    /// Returns `Option<T>` (Some/None Value::EnumVariant). Pulls from
    /// the buffered slot if present; otherwise pulls one element from
    /// the inner iterator via `iterator_step`, stores it in the
    /// buffer, and returns a clone. The buffer stays populated so the
    /// next `peek()` (or `next()`) sees the same element. Once the
    /// inner is exhausted and the buffer is empty, returns
    /// `None` on every subsequent call.
    fn peek_value(&mut self, iter: &mut Value) -> Value {
        let some = |v: Value| Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "Some".to_string(),
            data: EnumData::Tuple(vec![v]),
        };
        let none = || Value::EnumVariant {
            enum_name: "Option".to_string(),
            variant: "None".to_string(),
            data: EnumData::Unit,
        };
        if let Value::Iterator {
            source: IteratorSource::Peekable { buffered, .. },
            ..
        } = iter
        {
            if let Some(boxed) = buffered.as_ref() {
                return some((**boxed).clone());
            }
        }
        let mut inner_taken = if let Value::Iterator {
            source: IteratorSource::Peekable { inner, .. },
            ..
        } = iter
        {
            std::mem::replace(inner.as_mut(), Value::Unit)
        } else {
            return none();
        };
        let yielded = self.iterator_step(&mut inner_taken);
        if let Value::Iterator {
            source: IteratorSource::Peekable { inner, buffered },
            ..
        } = iter
        {
            **inner = inner_taken;
            match yielded {
                Some(v) => {
                    *buffered = Some(Box::new(v.clone()));
                    some(v)
                }
                None => none(),
            }
        } else {
            none()
        }
    }

    /// Shared body for `Entry.or_insert(default)` and the vacant arm of
    /// `Entry.or_insert_with(f)`. On Vacant, push the new (key, default)
    /// pair onto the live Map (re-fetched by `map_var`) and write back.
    /// On Occupied, return the existing slot value cloned. Either way,
    /// returns the inserted-or-existing value as a Value (NOT a true
    /// `mut ref V`); chained mutation through the return is only fully
    /// supported by the codegen path. Returns `Value::Unit` if the entry
    /// has no `map_var` (chain rooted at a non-identifier receiver) or
    /// the binding doesn't resolve to a Map.
    fn entry_or_insert_value(
        &mut self,
        map_var: Option<String>,
        key: Value,
        slot_idx: Option<usize>,
        default: Value,
    ) -> Value {
        let Some(name) = map_var else {
            return Value::Unit;
        };
        let Some(Value::Map(mut m)) = self.env.get(&name) else {
            return Value::Unit;
        };
        if let Some(idx) = slot_idx {
            if let Some((_, v)) = m.get(idx) {
                return v.clone();
            }
        }
        m.push((key, default.clone()));
        self.env.set(&name, Value::Map(m));
        default
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

    /// Invoke a comparator closure (a `Value::Function` produced by an
    /// `ExprKind::Closure`) on two values and translate the returned
    /// `Ordering` enum variant into `std::cmp::Ordering`. Used by the
    /// closure-taking sort methods (`sort_by`, `sorted_by`) to bridge
    /// the user's `|a, b| ... -> Ordering` to Rust's `Vec::sort_by`.
    ///
    /// **Caller invariant — no `RwLock` held.** `std::sync::RwLock` is
    /// non-reentrant on the same thread; the user closure body may
    /// re-enter the interpreter on the same array (e.g. an inner
    /// `.len()` call), which would deadlock or panic against a held
    /// write guard. Each call site snapshots the source vector before
    /// invoking sort so no lock is live during the comparator callbacks.
    fn invoke_value_comparator(
        &mut self,
        cmp_val: &Value,
        a: Value,
        b: Value,
        method_label: &str,
    ) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let result = self.invoke_function_value(cmp_val.clone(), vec![a, b]);
        match &result {
            Value::EnumVariant {
                enum_name, variant, ..
            } if enum_name == "Ordering" => match variant.as_str() {
                "Less" => Ordering::Less,
                "Equal" => Ordering::Equal,
                "Greater" => Ordering::Greater,
                other => panic!(
                    "{method_label}: comparator returned Ordering.{other} \
                     which is not one of Less/Equal/Greater"
                ),
            },
            _ => panic!(
                "{method_label}: comparator must return Ordering, returned a different value"
            ),
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

        // Slice 3 — mut-Slice mutation methods that route their writes
        // back to the original storage. These dispatch BEFORE the
        // Slice→Array normalization below; the normalization is for
        // read-only methods that can safely operate on a fresh snapshot.
        if let Value::Slice {
            storage,
            start,
            len,
            ..
        } = &obj
        {
            if method == "swap" {
                let i_val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                let j_val = args
                    .get(1)
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                if let (Value::Int(i_v), Value::Int(j_v)) = (i_val, j_val) {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let mut guard = try_write_or_panic(storage, &label);
                    let i = i_v as usize;
                    let j = j_v as usize;
                    if i < *len && j < *len {
                        guard.swap(start + i, start + j);
                    }
                }
                return Value::Unit;
            }
        }

        // Slice 3 — methods on `Slice[T]` / `mut Slice[T]` dispatch via
        // the existing Array-method surface. The interpreter snapshots
        // the slice's window into a fresh `Value::Array` so each
        // read-only method (`first` / `last` / `get` / `contains` /
        // `chunks` / `windows` / `len` / `is_empty` / `iter` / etc.)
        // sees a uniform shape. The slice itself is preserved by the
        // `.as_slice` / `.as_slice_mut` MethodCall arm above (which
        // detects the Slice receiver and rebuilds the view) and by the
        // Index expression path for read/write through `[i]`. Mutation
        // methods that need source-aliasing semantics (`swap`) dispatch
        // above this fence.
        let obj = match obj {
            Value::Slice {
                storage,
                start,
                len,
                ..
            } if !matches!(method, "as_slice" | "as_slice_mut") => {
                let snap = storage.read().unwrap()[start..start + len].to_vec();
                Value::array_of(snap)
            }
            other => other,
        };

        // Slice F (`std.json`): `j.stringify()` on a `Json`-typed
        // receiver. Walks the enum tree to a `serde_json::Value` and
        // calls `serde_json::to_string`. Locked design (ii)'s insertion-
        // order property is preserved because the receiver's `Object`
        // payload is a `Vec[(String, Json)]` and the runtime crate's
        // `serde_json` is built with `preserve_order`, so the
        // intermediate `serde_json::Map` round-trips key ordering.
        if method == "stringify" {
            if let Value::EnumVariant { ref enum_name, .. } = obj {
                if enum_name == "Json" {
                    let v = kara_json_to_serde_json(&obj);
                    let s = serde_json::to_string(&v).unwrap_or_else(|_| "null".to_string());
                    return Value::String(s);
                }
            }
        }

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
                    Value::Array(rc) => Value::Int(rc.read().unwrap().len() as i64),
                    Value::Slice { len, .. } => Value::Int(*len as i64),
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
            "chars" => {
                // `String.chars() -> Iterator[char]`. Snapshot the chars
                // eagerly into a Value::Iterator so adaptor chains (`map`,
                // `filter`, …) and `for c in s.chars()` go through the same
                // step-machine as other collections. Peer of design.md
                // § Character type (line 2299): the design pins `for c in s`
                // and `s.chars()` as semantic peers; both route here in the
                // tree-walk interpreter (the `for` site dispatches on
                // `Value::String` directly via the same `s.chars()` shape).
                return match &obj {
                    Value::String(s) => {
                        let items: Vec<Value> = s.chars().map(Value::Char).collect();
                        Value::Iterator {
                            source: IteratorSource::Eager { items, cursor: 0 },
                            steps: Vec::new(),
                        }
                    }
                    _ => unreachable!(
                        "chars() on unsupported type at {}:{}; should be caught by typechecker",
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
                //
                // Iterator receivers (e.g. the redundant `(0..10).iter()`
                // call shape now that Range evaluates to `Value::Iterator`)
                // pass through unchanged — calling iter() on an iterator
                // returns the iterator itself.
                if matches!(&obj, Value::Iterator { .. }) {
                    return obj;
                }
                let items = match &obj {
                    Value::Array(rc) => rc.read().unwrap().clone(),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => storage.read().unwrap()[*start..*start + *len].to_vec(),
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
                    source: IteratorSource::Eager { items, cursor: 0 },
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
                    let Value::Iterator { source, mut steps } = obj else {
                        unreachable!()
                    };
                    steps.push(match method {
                        "map" => IteratorStep::Map(closure),
                        "filter" => IteratorStep::Filter(closure),
                        _ => unreachable!(),
                    });
                    return Value::Iterator { source, steps };
                }
            }
            "enumerate" => {
                // Lazy positional adaptor — append `Enumerate(0)` to the
                // chain. iterator_step wraps each yielded item into
                // `(idx, item)` and bumps the counter.
                if matches!(obj, Value::Iterator { .. }) {
                    let Value::Iterator { source, mut steps } = obj else {
                        unreachable!()
                    };
                    steps.push(IteratorStep::Enumerate(0));
                    return Value::Iterator { source, steps };
                }
            }
            "take" | "skip" => {
                // Lazy count-bounded adaptors. Negative `n` clamps to
                // zero — `take(-1)` yields nothing; `skip(-1)` skips
                // nothing. The typechecker accepts any i64 so this
                // matters at runtime.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            format!("Iterator.{}() requires an integer argument", method),
                            span,
                        );
                    };
                    let n = match self.eval_expr_inner(&arg.value) {
                        Value::Int(n) => n.max(0) as usize,
                        v => {
                            return self.record_runtime_error(
                                format!("Iterator.{}() expects an integer; got {}", method, v),
                                span,
                            );
                        }
                    };
                    let Value::Iterator { source, mut steps } = obj else {
                        unreachable!()
                    };
                    steps.push(match method {
                        "take" => IteratorStep::Take(n),
                        "skip" => IteratorStep::Skip(n),
                        _ => unreachable!(),
                    });
                    return Value::Iterator { source, steps };
                }
            }
            "take_while" | "skip_while" => {
                // Lazy predicate-bounded adaptors. `take_while` stops
                // on the first false; `skip_while` drops items while
                // pred holds, then yields the rest unconditionally.
                // Both share the closure-validation path of map/filter.
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
                    let Value::Iterator { source, mut steps } = obj else {
                        unreachable!()
                    };
                    steps.push(match method {
                        "take_while" => IteratorStep::TakeWhile {
                            pred: closure,
                            done: false,
                        },
                        "skip_while" => IteratorStep::SkipWhile {
                            pred: closure,
                            done: false,
                        },
                        _ => unreachable!(),
                    });
                    return Value::Iterator { source, steps };
                }
            }
            "flat_map" => {
                // Lazy flatten-after-map combinator. Wraps `self` (the
                // outer) plus the closure into a fresh
                // `IteratorSource::FlatMap`. Each pull from the
                // resulting iterator drains the in-flight inner
                // iterator (filling it from `f(outer_item)` when
                // exhausted) and yields one item per pull.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.flat_map() requires a closure argument".to_string(),
                            span,
                        );
                    };
                    let closure = self.eval_expr_inner(&arg.value);
                    if !matches!(closure, Value::Function { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.flat_map() expects a closure; got {}", closure),
                            span,
                        );
                    }
                    return Value::Iterator {
                        source: IteratorSource::FlatMap {
                            outer: Box::new(obj),
                            f: Box::new(closure),
                            current_inner: None,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "step_by" => {
                // Lazy stride adaptor — yields every n-th item. Negative
                // or zero `n` clamps to 1 at the runtime layer (the
                // typechecker accepts any i64). n=1 makes step_by an
                // observable no-op; n>len yields just the first item.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.step_by() requires an integer argument".to_string(),
                            span,
                        );
                    };
                    let n = match self.eval_expr_inner(&arg.value) {
                        Value::Int(n) => n.max(1) as usize,
                        v => {
                            return self.record_runtime_error(
                                format!("Iterator.step_by() expects an integer; got {}", v),
                                span,
                            );
                        }
                    };
                    let Value::Iterator { source, mut steps } = obj else {
                        unreachable!()
                    };
                    steps.push(IteratorStep::StepBy {
                        n,
                        remaining_skip: 0,
                    });
                    return Value::Iterator { source, steps };
                }
            }
            "cycle" => {
                // Restart-on-exhaust combinator. Snapshots `self`
                // (deep-clone via Value's derived Clone) into a
                // `template`; each restart re-clones the template
                // into `current`, which resets adaptor counters
                // (Enumerate / Take / Skip / TakeWhile / SkipWhile /
                // StepBy) for that cycle. Downstream adaptors append
                // to the wrapping iterator's empty steps and apply
                // uniformly across cycles.
                if matches!(obj, Value::Iterator { .. }) {
                    if !args.is_empty() {
                        return self.record_runtime_error(
                            format!("Iterator.cycle() takes no arguments, got {}", args.len()),
                            span,
                        );
                    }
                    let template = obj.clone();
                    return Value::Iterator {
                        source: IteratorSource::Cycle {
                            template: Box::new(template.clone()),
                            current: Box::new(template),
                            exhausted: false,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "inspect" => {
                // Lazy side-effect adaptor — appends an
                // `IteratorStep::Inspect(closure)` that fires `f` on
                // each yielded item and passes the item through
                // unchanged.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.inspect() requires a closure argument".to_string(),
                            span,
                        );
                    };
                    let closure = self.eval_expr_inner(&arg.value);
                    if !matches!(closure, Value::Function { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.inspect() expects a closure; got {}", closure),
                            span,
                        );
                    }
                    let Value::Iterator { source, mut steps } = obj else {
                        unreachable!()
                    };
                    steps.push(IteratorStep::Inspect(closure));
                    return Value::Iterator { source, steps };
                }
            }
            "scan" => {
                // Lazy stateful adaptor — appends an
                // `IteratorStep::Scan { f, state, done }`. Closure
                // signature is `Fn(A, T) -> Option<(A, U)>`; the
                // first arg is the initial state, the second is the
                // closure.
                if matches!(obj, Value::Iterator { .. }) {
                    if args.len() != 2 {
                        return self.record_runtime_error(
                            format!("Iterator.scan() requires 2 arguments, got {}", args.len()),
                            span,
                        );
                    }
                    let init = self.eval_expr_inner(&args[0].value);
                    let closure = self.eval_expr_inner(&args[1].value);
                    if !matches!(closure, Value::Function { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.scan() expects a closure; got {}", closure),
                            span,
                        );
                    }
                    let Value::Iterator { source, mut steps } = obj else {
                        unreachable!()
                    };
                    steps.push(IteratorStep::Scan {
                        f: closure,
                        state: init,
                        done: false,
                    });
                    return Value::Iterator { source, steps };
                }
            }
            "chunk_by" => {
                // Lazy buffering adaptor — wraps the receiver into a
                // ChunkBy source. Each pull yields a freshly allocated
                // `Vec[T]` containing the next run of consecutive
                // items whose `key_fn(item)` produces equal keys.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.chunk_by() requires a closure argument".to_string(),
                            span,
                        );
                    };
                    let closure = self.eval_expr_inner(&arg.value);
                    if !matches!(closure, Value::Function { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.chunk_by() expects a closure; got {}", closure),
                            span,
                        );
                    }
                    return Value::Iterator {
                        source: IteratorSource::ChunkBy {
                            inner: Box::new(obj),
                            key_fn: Box::new(closure),
                            pending_item: None,
                            pending_key: None,
                            exhausted: false,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "peekable" => {
                // Wraps the receiver into a Peekable source with an
                // empty buffer. Adaptor calls after this return
                // Iterator[U] at the type layer (peekable-ness lost),
                // so the wrapping iterator's `steps` stays empty in
                // well-typed programs and pull_source can route
                // straight to the inner iterator without re-running
                // outer steps.
                if matches!(obj, Value::Iterator { .. }) {
                    if !args.is_empty() {
                        return self.record_runtime_error(
                            format!("Iterator.peekable() takes no arguments, got {}", args.len()),
                            span,
                        );
                    }
                    return Value::Iterator {
                        source: IteratorSource::Peekable {
                            inner: Box::new(obj),
                            buffered: None,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "peek" => {
                // Look one element ahead without consuming. Pull from
                // the buffer if present; otherwise pull one item from
                // the inner iterator, store it in the buffer, and
                // return a clone wrapped in `Some`. Sticky-empty
                // (returns None forever once the inner is exhausted
                // and the buffer is empty). Writeback to the binding
                // mirrors `next()` so subsequent calls observe the
                // populated buffer.
                if let Value::Iterator {
                    source: IteratorSource::Peekable { .. },
                    ..
                } = &obj
                {
                    if !args.is_empty() {
                        return self.record_runtime_error(
                            format!("Peekable.peek() takes no arguments, got {}", args.len()),
                            span,
                        );
                    }
                    let mut iter_val = obj;
                    let result = self.peek_value(&mut iter_val);
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, iter_val);
                    }
                    return result;
                }
            }
            "chain" => {
                // Lazy two-source combinator. Wraps `self` and `other`
                // into an `IteratorSource::Chain` so each side keeps
                // its own (already-applied) step chain. Downstream
                // adaptors append to the new wrapper's empty steps.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.chain() requires an iterator argument".to_string(),
                            span,
                        );
                    };
                    let other = self.eval_expr_inner(&arg.value);
                    if !matches!(other, Value::Iterator { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.chain() expects an iterator; got {}", other),
                            span,
                        );
                    }
                    return Value::Iterator {
                        source: IteratorSource::Chain {
                            parts: vec![obj, other],
                            current: 0,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "zip" => {
                // Lazy synchronous-pair combinator. Each pull from the
                // resulting iterator pulls one item from each side and
                // yields a `(a, b)` tuple; either side ending stops the
                // zip. Each side retains its own step chain.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.zip() requires an iterator argument".to_string(),
                            span,
                        );
                    };
                    let other = self.eval_expr_inner(&arg.value);
                    if !matches!(other, Value::Iterator { .. }) {
                        return self.record_runtime_error(
                            format!("Iterator.zip() expects an iterator; got {}", other),
                            span,
                        );
                    }
                    return Value::Iterator {
                        source: IteratorSource::Zip {
                            left: Box::new(obj),
                            right: Box::new(other),
                        },
                        steps: Vec::new(),
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
                    return Value::array_of(out);
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
                // Slice 3 — produce a Value::Slice that shares the
                // source's `Arc<RwLock<Vec<Value>>>` storage. Mutation
                // through a `mut Slice[T]` propagates back to the source
                // because the storage is the same handle, and the
                // runtime guard fires on aliased writes via
                // try_write_or_panic.
                let mutable = method == "as_slice_mut";
                return match &obj {
                    Value::Array(rc) => {
                        let len = rc.read().unwrap().len();
                        Value::Slice {
                            storage: rc.clone(),
                            start: 0,
                            len,
                            mutable,
                        }
                    }
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => Value::Slice {
                        storage: storage.clone(),
                        start: *start,
                        len: *len,
                        mutable,
                    },
                    _ => unreachable!(
                        "{}() on unsupported type at {}:{}; should be caught by typechecker",
                        method, span.line, span.column
                    ),
                };
            }
            "push" => {
                if let Value::Array(rc) = &obj {
                    let val = if let Some(arg) = args.first() {
                        self.eval_expr_inner(&arg.value)
                    } else {
                        Value::Unit
                    };
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    try_write_or_panic(rc, &label).push(val);
                    return Value::Unit;
                }
            }
            // ── VecDeque[T] surface (design.md). The runtime shape is
            //    the same `Value::Array` storage as `Vec[T]`; front-end
            //    ops translate to `Vec::insert(0, …)` / `Vec::remove(0)`
            //    (O(n) — acceptable for the tree-walk interpreter). The
            //    typechecker is permissive, so `Vec[T]` receivers can
            //    also reach these arms; valid Kāra source guards via
            //    receiver type. Routed through helpers to keep
            //    `eval_method_call`'s debug-mode stack frame compact.
            "push_back" | "push_front" | "pop_back" | "pop_front" => {
                if matches!(&obj, Value::Array(_)) {
                    return self.eval_vec_deque_method(method, &obj, object, args);
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
                if let Value::Array(ref rc) = obj {
                    return Value::Bool(rc.read().unwrap().is_empty());
                }
                if let Value::Slice { len, .. } = &obj {
                    return Value::Bool(*len == 0);
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
                let elem = match &obj {
                    Value::Array(rc) => rc.read().unwrap().first().cloned(),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => {
                        if *len > 0 {
                            Some(storage.read().unwrap()[*start].clone())
                        } else {
                            None
                        }
                    }
                    _ => return Value::Unit,
                };
                return match elem {
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
            "last" => {
                let elem = match &obj {
                    Value::Array(rc) => rc.read().unwrap().last().cloned(),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => {
                        if *len > 0 {
                            Some(storage.read().unwrap()[*start + *len - 1].clone())
                        } else {
                            None
                        }
                    }
                    _ => return Value::Unit,
                };
                return match elem {
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
            "get_unchecked" => {
                // Direct-index read with no bounds check. Codegen returns
                // garbage on OOB; the interpreter mirror panics with the
                // standard out-of-bounds message rather than return Value::Unit,
                // so misuse surfaces immediately under `karac run` even though
                // the codegen path is "UB" by design.
                let array_view: Option<Vec<Value>> = match &obj {
                    Value::Array(rc) => Some(rc.read().unwrap().clone()),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => Some(storage.read().unwrap()[*start..*start + *len].to_vec()),
                    _ => None,
                };
                if let Some(v) = array_view {
                    let idx = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(0));
                    if let Value::Int(i) = idx {
                        let i = i as usize;
                        if i >= v.len() {
                            panic!(
                                "Vec.get_unchecked: index {} out of bounds (len={}) — \
                                 caller broke the unsafe precondition",
                                i,
                                v.len()
                            );
                        }
                        return v[i].clone();
                    }
                    return Value::Unit;
                }
                return Value::Unit;
            }
            "get" => {
                let array_view: Option<Vec<Value>> = match &obj {
                    Value::Array(rc) => Some(rc.read().unwrap().clone()),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => Some(storage.read().unwrap()[*start..*start + *len].to_vec()),
                    _ => None,
                };
                if let Some(v) = array_view {
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
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
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
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
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
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let idx = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(0));
                    return if let Value::Int(i) = idx {
                        let i = (i as usize).min(v.len());
                        let left = Value::array_of(v[..i].to_vec());
                        let right = Value::array_of(v[i..].to_vec());
                        Value::Tuple(vec![left, right])
                    } else {
                        Value::Unit
                    };
                }
            }
            "chunks" => {
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let n = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(1));
                    if let Value::Int(n) = n {
                        let n = if n > 0 { n as usize } else { 1 };
                        let chunks: Vec<Value> =
                            v.chunks(n).map(|c| Value::array_of(c.to_vec())).collect();
                        return Value::array_of(chunks);
                    }
                }
                // Iterator-trait variant — lazy chunks; wraps the
                // receiver into an `IteratorSource::Chunks`. Each
                // pull yields a freshly allocated `Vec[T]`. n is
                // clamped to `n.max(1)`, matching `step_by`'s policy.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.chunks() requires an integer argument".to_string(),
                            span,
                        );
                    };
                    let n = match self.eval_expr_inner(&arg.value) {
                        Value::Int(n) => n.max(1) as usize,
                        v => {
                            return self.record_runtime_error(
                                format!("Iterator.chunks() expects an integer; got {}", v),
                                span,
                            );
                        }
                    };
                    return Value::Iterator {
                        source: IteratorSource::Chunks {
                            inner: Box::new(obj),
                            n,
                            exhausted: false,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "windows" => {
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let n = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(1));
                    if let Value::Int(n) = n {
                        let n = if n > 0 && (n as usize) <= v.len() {
                            n as usize
                        } else {
                            return Value::array_of(vec![]);
                        };
                        let wins: Vec<Value> =
                            v.windows(n).map(|w| Value::array_of(w.to_vec())).collect();
                        return Value::array_of(wins);
                    }
                }
                // Iterator-trait variant — lazy sliding window; each
                // pull yields a freshly cloned buffer of size n. n=0
                // and n>source-length both produce zero windows; we
                // clamp to n.max(1) at the dispatch site so the
                // first-prime-pull naturally trips the
                // sticky-exhausted path on a too-small source.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.windows() requires an integer argument".to_string(),
                            span,
                        );
                    };
                    let n = match self.eval_expr_inner(&arg.value) {
                        Value::Int(n) => n.max(1) as usize,
                        v => {
                            return self.record_runtime_error(
                                format!("Iterator.windows() expects an integer; got {}", v),
                                span,
                            );
                        }
                    };
                    return Value::Iterator {
                        source: IteratorSource::Windows {
                            inner: Box::new(obj),
                            n,
                            buffer: Vec::with_capacity(n),
                            primed: false,
                            exhausted: false,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "sort" => {
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    try_write_or_panic(rc, &label).sort_by(value_compare);
                    return Value::Unit;
                }
            }
            "sort_by" => {
                // sort_by(|a, b| -> Ordering) — snapshot the vec so the user
                // closure can re-enter the interpreter freely (std::sync::RwLock
                // is non-reentrant on the same thread), sort the snapshot via
                // the user comparator, then write the result back.
                if args.len() != 1 {
                    panic!(
                        "sort_by expects 1 argument (comparator closure), got {}",
                        args.len()
                    );
                }
                let cmp_val = self.eval_expr_inner(&args[0].value);
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let mut snapshot = rc.read().unwrap().clone();
                    snapshot.sort_by(|a, b| {
                        self.invoke_value_comparator(&cmp_val, a.clone(), b.clone(), "sort_by")
                    });
                    *try_write_or_panic(rc, &label) = snapshot;
                    return Value::Unit;
                }
            }
            "sorted" => {
                if let Value::String(ref s) = obj {
                    let mut chars: Vec<char> = s.chars().collect();
                    chars.sort_unstable();
                    return Value::String(chars.into_iter().collect());
                }
                if let Value::Array(ref rc) = obj {
                    let mut v = rc.read().unwrap().clone();
                    v.sort_by(value_compare);
                    return Value::array_of(v);
                }
            }
            "sorted_by" => {
                // sorted_by(|a, b| -> Ordering) — same snapshot-then-sort
                // pattern as `sort_by`, but returns a new collection instead
                // of mutating in place. The `.clone()` releases the read
                // guard before the comparator runs, so the user closure can
                // re-enter the interpreter freely.
                if args.len() != 1 {
                    panic!(
                        "sorted_by expects 1 argument (comparator closure), got {}",
                        args.len()
                    );
                }
                let cmp_val = self.eval_expr_inner(&args[0].value);
                if let Value::String(ref s) = obj {
                    let mut chars: Vec<char> = s.chars().collect();
                    chars.sort_by(|a, b| {
                        self.invoke_value_comparator(
                            &cmp_val,
                            Value::Char(*a),
                            Value::Char(*b),
                            "sorted_by",
                        )
                    });
                    return Value::String(chars.into_iter().collect());
                }
                if let Value::Array(ref rc) = obj {
                    let mut v = rc.read().unwrap().clone();
                    v.sort_by(|a, b| {
                        self.invoke_value_comparator(&cmp_val, a.clone(), b.clone(), "sorted_by")
                    });
                    return Value::array_of(v);
                }
            }
            "sort_by_key" => {
                // sort_by_key(|t| -> K) where K: Ord — precompute keys once
                // (Rust's `sort_by_key` semantics: each element's key is
                // computed exactly once, not on every comparator invocation),
                // sort the (key, value) pairs by key via `value_compare`, write
                // the values back. Snapshot-then-replace mirrors `sort_by` to
                // keep the user closure free to re-enter the interpreter.
                if args.len() != 1 {
                    panic!(
                        "sort_by_key expects 1 argument (key closure), got {}",
                        args.len()
                    );
                }
                let key_val = self.eval_expr_inner(&args[0].value);
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let snapshot = rc.read().unwrap().clone();
                    let mut keyed: Vec<(Value, Value)> = snapshot
                        .into_iter()
                        .map(|v| {
                            let k = self.invoke_function_value(key_val.clone(), vec![v.clone()]);
                            (k, v)
                        })
                        .collect();
                    keyed.sort_by(|(k1, _), (k2, _)| value_compare(k1, k2));
                    let sorted: Vec<Value> = keyed.into_iter().map(|(_, v)| v).collect();
                    *try_write_or_panic(rc, &label) = sorted;
                    return Value::Unit;
                }
            }
            "sorted_by_key" => {
                // sorted_by_key(|t| -> K) where K: Ord — same precompute-keys
                // pattern as `sort_by_key`, but returns a new Vec instead of
                // mutating in place.
                if args.len() != 1 {
                    panic!(
                        "sorted_by_key expects 1 argument (key closure), got {}",
                        args.len()
                    );
                }
                let key_val = self.eval_expr_inner(&args[0].value);
                if let Value::Array(ref rc) = obj {
                    let snapshot = rc.read().unwrap().clone();
                    let mut keyed: Vec<(Value, Value)> = snapshot
                        .into_iter()
                        .map(|v| {
                            let k = self.invoke_function_value(key_val.clone(), vec![v.clone()]);
                            (k, v)
                        })
                        .collect();
                    keyed.sort_by(|(k1, _), (k2, _)| value_compare(k1, k2));
                    let sorted: Vec<Value> = keyed.into_iter().map(|(_, v)| v).collect();
                    return Value::array_of(sorted);
                }
            }
            "reverse" => {
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    try_write_or_panic(rc, &label).reverse();
                    return Value::Unit;
                }
            }
            "fill" => {
                let fill_val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let mut guard = try_write_or_panic(rc, &label);
                    for elem in guard.iter_mut() {
                        *elem = fill_val.clone();
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
                    if let Value::Array(ref rc) = obj {
                        let label = match &object.kind {
                            ExprKind::Identifier(n) => n.clone(),
                            _ => "<value>".to_string(),
                        };
                        let mut guard = try_write_or_panic(rc, &label);
                        let i = i_val as usize;
                        let j = j_val as usize;
                        if i < guard.len() && j < guard.len() {
                            guard.swap(i, j);
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
            // clone() — Sender creates an additional producer sharing the
            // same queue Arc. For collection types (Array/String/Map/Set/
            // SortedSet) the canonical Clone impl is a structural deep
            // copy: each `Value` variant is itself `Clone` so
            // `obj.clone()` does the right thing without per-type
            // unrolling. Non-Clone payloads (closures, iterators, refs,
            // entries, shared cells) fall through; the typechecker
            // rejects `clone()` on those receivers via `clone_self_type_for`.
            "clone" => {
                if let Value::Sender(ref queue) = obj {
                    return Value::Sender(Arc::clone(queue));
                }
                match &obj {
                    Value::Array(rc) => {
                        // Deep copy — clone the inner Vec into a fresh
                        // shared cell so the clone has independent
                        // storage. Slice 3: this matches the v1
                        // value-semantics rule that `arr.clone()`
                        // produces a structurally independent array.
                        return Value::array_of(rc.read().unwrap().clone());
                    }
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => {
                        return Value::array_of(
                            storage.read().unwrap()[*start..*start + *len].to_vec(),
                        );
                    }
                    Value::String(s) => return Value::String(s.clone()),
                    Value::Map(m) => return Value::Map(m.clone()),
                    Value::Set(s) => return Value::Set(s.clone()),
                    Value::SortedSet(s) => return Value::SortedSet(s.clone()),
                    _ => {}
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
                    return Value::array_of(m.iter().map(|(k, _)| k.clone()).collect());
                }
            }
            "values" => {
                if let Value::Map(ref m) = obj {
                    return Value::array_of(m.iter().map(|(_, v)| v.clone()).collect());
                }
            }
            "entries" => {
                if let Value::Map(ref m) = obj {
                    return Value::array_of(
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
            // ── Map.entry(k) and the Entry[K, V] method surface ────────────
            //
            // `entry(k)` returns a `Value::Entry` carrying the original Map's
            // binding name (so write-back can target the right slot via
            // `env.set`), the key, and the slot index when the key is
            // already present. The chain methods (`or_insert`,
            // `or_insert_with`, `and_modify`) dispatch on `Value::Entry` and
            // re-fetch the Map from the env each call so any mutation that
            // happened earlier in the chain (or in user code between calls)
            // is visible.
            //
            // The interpreter's `mut ref V` semantics on `or_insert*`'s
            // return are partial: `or_insert` returns the cloned slot value,
            // not a true alias into the map. The fully-aliased form
            // (`m.entry(k).or_insert_with(Vec.new).push(row)` mutating the
            // slot in place) is gated on Subtask 6 (codegen) where mut-ref-V
            // is realised as a raw slot pointer; the typechecker accepts the
            // chain shape regardless. Tests at the interpreter layer verify
            // map state after the chain runs, not the returned-slot ergonomics.
            "entry" => {
                if let Value::Map(ref m) = obj {
                    let key = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    let slot_idx = m.iter().position(|(k, _)| *k == key);
                    let map_var = if let ExprKind::Identifier(name) = &object.kind {
                        Some(name.clone())
                    } else {
                        None
                    };
                    return Value::Entry {
                        map_var,
                        key: Box::new(key),
                        slot_idx,
                    };
                }
            }
            "or_insert" => {
                if let Value::Entry {
                    map_var,
                    key,
                    slot_idx,
                } = obj
                {
                    let default = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return self.entry_or_insert_value(map_var, *key, slot_idx, default);
                }
            }
            "or_insert_with" => {
                if let Value::Entry {
                    map_var,
                    key,
                    slot_idx,
                } = obj
                {
                    if slot_idx.is_some() {
                        // Occupied — closure not invoked. Pull the existing
                        // slot value out of the live Map (it may have been
                        // mutated by an earlier chain step).
                        if let Some(name) = map_var.as_deref() {
                            if let Some(Value::Map(m)) = self.env.get(name) {
                                if let Some(idx) = slot_idx {
                                    if let Some((_, v)) = m.get(idx) {
                                        return v.clone();
                                    }
                                }
                            }
                        }
                        return Value::Unit;
                    }
                    // Vacant — invoke the no-arg closure to produce the
                    // default value, then insert.
                    let f = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    let default = self.invoke_function_value(f, vec![]);
                    return self.entry_or_insert_value(map_var, *key, slot_idx, default);
                }
            }
            "and_modify" => {
                if let Value::Entry {
                    map_var,
                    key,
                    slot_idx,
                } = obj
                {
                    if let (Some(name), Some(idx)) = (map_var.as_deref(), slot_idx) {
                        // Occupied — invoke closure with a SharedCell aliased
                        // to the slot value so `|v| { v += 1 }` mutates
                        // through. Read the cell back and write the result
                        // into the Map slot.
                        let f = args
                            .first()
                            .map(|a| self.eval_expr_inner(&a.value))
                            .unwrap_or(Value::Unit);
                        if let Some(Value::Map(mut m)) = self.env.get(name) {
                            if let Some((_, slot_v)) = m.get(idx) {
                                let cell = Arc::new(Mutex::new(slot_v.clone()));
                                let _ = self.invoke_function_value(
                                    f,
                                    vec![Value::SharedCell(cell.clone())],
                                );
                                let new_v = cell.lock().unwrap().clone();
                                m[idx].1 = new_v;
                                self.env.set(name, Value::Map(m));
                            }
                        }
                    }
                    // Return self for chaining — vacant case is a no-op pass-
                    // through. slot_idx and key are unchanged in either case.
                    return Value::Entry {
                        map_var,
                        key,
                        slot_idx,
                    };
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
                                return Value::array_of(matches);
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
            // ── Request method dispatch (HTTP handler ABI trampoline, 2026-05-09) ──
            // F2 owned-String contract: each call returns a freshly-cloned
            // `Value::String`, so multiple calls to `req.path()` / `.method()`
            // never collide on a borrowed buffer. v1 returns an empty String
            // — the interpreter doesn't run a real HTTP server, so there's
            // no real path/method to surface. Pinned by
            // `tests/interpreter.rs::test_server_serve_handler_request_path_returns_owned_string`.
            "path" | "method" if matches!(&obj, Value::Struct { name, .. } if name == "Request") => {
                return Value::String(String::new());
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

        // Primitive value-receiver dispatch for the builtin Eq/Ord methods.
        // The typechecker registers `eq`/`ne`/`lt`/`le`/`gt`/`ge`/`cmp` for
        // every integer width, bool, char, String, and the F32/F64 total-
        // order wrappers (`register_builtin_impl("Ord", ...)` in
        // src/typechecker.rs) — but those registrations live in the
        // typechecker's env, not the interpreter's, so a call like
        // `b.cmp(a)` with a primitive receiver would otherwise fall through
        // to the impl-block lookup below and panic. The type-name receiver
        // form `i64.cmp(a, b)` already routes through `dispatch_lowered_op`;
        // this mirrors that path for the value-receiver form (one arg
        // instead of two) so `xs.sort_by(|a, b| b.cmp(a))` works.
        if matches!(
            &obj,
            Value::Int(_)
                | Value::Char(_)
                | Value::Bool(_)
                | Value::String(_)
                | Value::TotalFloat32(_)
                | Value::TotalFloat64(_)
        ) {
            if method == "cmp" && args.len() == 1 {
                let other = self.eval_expr_inner(&args[0].value);
                let ord = value_compare(&obj, &other);
                return Value::EnumVariant {
                    enum_name: "Ordering".to_string(),
                    variant: match ord {
                        std::cmp::Ordering::Less => "Less".to_string(),
                        std::cmp::Ordering::Equal => "Equal".to_string(),
                        std::cmp::Ordering::Greater => "Greater".to_string(),
                    },
                    data: EnumData::Unit,
                };
            }
            let bin_op = match method {
                "eq" => Some(BinOp::Eq),
                "ne" => Some(BinOp::NotEq),
                "lt" => Some(BinOp::Lt),
                "le" => Some(BinOp::LtEq),
                "gt" => Some(BinOp::Gt),
                "ge" => Some(BinOp::GtEq),
                _ => None,
            };
            if let Some(op) = bin_op {
                if args.len() == 1 {
                    let rhs = self.eval_expr_inner(&args[0].value);
                    return self.eval_binary(&op, obj.clone(), rhs, span);
                }
            }
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
        let arg_vals: Vec<Value> = args
            .iter()
            .map(|a| self.eval_expr_inner(&a.value))
            .collect();
        if self.check_cf() {
            return Value::Unit;
        }
        self.dispatch_resource_method_with_values(resource, method, arg_vals, span)
    }

    /// Pre-evaluated-args entry into the provider-stack dispatch path.
    /// Same lookup / BuiltinDefault / user-provider routing as
    /// [`eval_resource_method`], but skips argument evaluation so callers
    /// that compute their args via a different path (e.g. the print
    /// router that formats a `Display` value into a `String` before
    /// dispatch) can share the same final dispatch.
    fn dispatch_resource_method_with_values(
        &mut self,
        resource: &str,
        method: &str,
        mut arg_vals: Vec<Value>,
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
            return self.dispatch_builtin_resource_method_with_values(
                resource_name,
                method,
                arg_vals,
                span,
            );
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

        // Prepend the provider as the implicit `self` argument.
        arg_vals.insert(0, provider);

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
    /// BuiltinDefault dispatch path. Used by the provider-stack router
    /// when no user `with_provider` has shadowed the resource — and by
    /// the print/println router which formats a `Display` value into a
    /// `String` and calls through the same arms a direct
    /// `Stdout.println(s)` call would hit.
    fn dispatch_builtin_resource_method_with_values(
        &mut self,
        resource: &str,
        method: &str,
        arg_vals: Vec<Value>,
        span: &Span,
    ) -> Value {
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
                Value::array_of(vals)
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
            ("Env", "set") => {
                // `env.set(name, value) -> Unit` with `writes(Env)`. POSIX
                // `setenv` shape — overwrites if already present, creates if
                // absent. Companion to `Env.var` and `Env.args`. The runtime
                // crate is Rust 2021 edition, where `std::env::set_var` is
                // safe; the safety contract (no concurrent reads of the
                // environment block on other threads) is upheld here because
                // the interpreter is single-threaded at this surface.
                self.track_effect("writes(Env)");
                let name = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Env.set expects a String name argument".to_string(),
                            span,
                        );
                    }
                };
                let value = match arg_vals.get(1) {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Env.set expects a String value argument".to_string(),
                            span,
                        );
                    }
                };
                std::env::set_var(&name, &value);
                Value::Unit
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
            ("Stdout", "print") => {
                self.track_effect("writes(Stdout)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stdout.print expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stdout(&s, false);
                Value::Unit
            }
            ("Stdout", "println") => {
                self.track_effect("writes(Stdout)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stdout.println expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stdout(&s, true);
                Value::Unit
            }
            ("Stdout", "flush") => {
                self.track_effect("writes(Stdout)");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Value::Unit
            }
            ("Stderr", "print") => {
                self.track_effect("writes(Stderr)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stderr.print expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stderr(&s, false);
                Value::Unit
            }
            ("Stderr", "println") => {
                self.track_effect("writes(Stderr)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stderr.println expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stderr(&s, true);
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
            PatternKind::RangePattern {
                start,
                end,
                inclusive,
            } => Self::value_in_range_pattern(value, start.as_ref(), end.as_ref(), *inclusive),
            PatternKind::AtBinding { pattern, .. } => self.try_match_pattern(pattern, value),
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let Some((storage, offset, total_len, _)) = slice_pattern_view(value) else {
                    return false;
                };
                let min_len = prefix.len() + suffix.len();
                if rest.is_none() {
                    if total_len != min_len {
                        return false;
                    }
                } else if total_len < min_len {
                    return false;
                }
                let storage_read = storage.read().unwrap();
                for (i, sub) in prefix.iter().enumerate() {
                    if !self.try_match_pattern(sub, &storage_read[offset + i]) {
                        return false;
                    }
                }
                for (i, sub) in suffix.iter().enumerate() {
                    let idx = offset + total_len - suffix.len() + i;
                    if !self.try_match_pattern(sub, &storage_read[idx]) {
                        return false;
                    }
                }
                true
            }
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
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let Some((storage, offset, total_len, source_mutable)) = slice_pattern_view(&value)
                else {
                    return;
                };
                let prefix_vals: Vec<Value>;
                let suffix_vals: Vec<Value>;
                {
                    let storage_read = storage.read().unwrap();
                    prefix_vals = (0..prefix.len())
                        .map(|i| storage_read[offset + i].clone())
                        .collect();
                    suffix_vals = (0..suffix.len())
                        .map(|i| storage_read[offset + total_len - suffix.len() + i].clone())
                        .collect();
                }
                for (sub, val) in prefix.iter().zip(prefix_vals) {
                    self.bind_pattern(sub, val);
                }
                for (sub, val) in suffix.iter().zip(suffix_vals) {
                    self.bind_pattern(sub, val);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    let rest_start = offset + prefix.len();
                    let rest_len = total_len - prefix.len() - suffix.len();
                    let rest_value = Value::Slice {
                        storage,
                        start: rest_start,
                        len: rest_len,
                        mutable: source_mutable,
                    };
                    self.env.define(name.clone(), rest_value);
                }
            }
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

    fn eval_builtin_print(&mut self, name: &str, args: &[CallArg], span: &Span) -> Value {
        // Route through the Stdout / Stderr provider stack so a
        // `with_provider[Stdout]` / `[Stderr]` install can intercept idiomatic
        // `println(x)` calls — not just direct `Stdout.println(s)` calls.
        // The user's provider method receives an already-formatted String;
        // the BuiltinDefault arm writes through `write_stdout` /
        // `write_stderr` (honoring `captured_output` for the test harness).
        let val = if let Some(arg) = args.first() {
            format!("{}", self.eval_expr_inner(&arg.value))
        } else {
            String::new()
        };
        if self.check_cf() {
            return Value::Unit;
        }
        let (resource, method) = match name {
            "eprintln" => ("Stderr", "println"),
            "println" => ("Stdout", "println"),
            _ => ("Stdout", "print"),
        };
        self.dispatch_resource_method_with_values(resource, method, vec![Value::String(val)], span)
    }

    /// Write to stdout, honoring `captured_output` when the test harness
    /// installed it. Used by both the free `print` / `println` router
    /// and the `Stdout.print` / `Stdout.println` resource methods so the
    /// two surfaces share one capture path.
    fn write_stdout(&mut self, s: &str, newline: bool) {
        if let Some(ref mut output) = self.captured_output {
            if newline {
                output.push(format!("{}\n", s));
            } else {
                output.push(s.to_string());
            }
        } else if newline {
            println!("{}", s);
        } else {
            print!("{}", s);
        }
    }

    /// Write to stderr. No capture buffer today — `captured_output` is
    /// stdout-only and the test harness does not currently snapshot stderr.
    /// Mirrors `write_stdout` so the `Stderr` arms have the same shape as
    /// `Stdout`'s without forcing every Stderr test to learn a new pattern.
    fn write_stderr(&mut self, s: &str, newline: bool) {
        if newline {
            eprintln!("{}", s);
        } else {
            eprint!("{}", s);
        }
    }

    fn eval_builtin_dbg(&mut self, args: &[CallArg], span: &Span) -> Value {
        // dbg() uses the transparent `debugs` effect (design.md § dbg() —
        // transparent and stripped in release builds), but the underlying
        // I/O still writes stderr. The track_effect call records that for
        // any future runtime instrumentation; transparency is enforced by
        // the static effect checker, not here.
        self.track_effect("writes(Stderr)");
        let arg_expr = args.first().map(|a| &a.value);
        let val = if let Some(expr) = arg_expr {
            self.eval_expr_inner(expr)
        } else {
            Value::Unit
        };

        // Source slice for the `expr` field. Falls back to "<expr>" when
        // the interpreter was constructed without a source-text setter
        // (some unit tests bypass the CLI) or the slice would be empty.
        let expr_text = arg_expr
            .and_then(|e| {
                let off = e.span.offset;
                let end = off.saturating_add(e.span.length);
                self.source_text.get(off..end)
            })
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "<expr>".to_string());

        // Type lookup via the typecheck side table. "?" when unavailable;
        // not all expression kinds reach the typechecker's recording path,
        // and ad-hoc test harnesses sometimes synthesize a TypeCheckResult
        // without populating expr_types.
        let type_text = arg_expr
            .and_then(|e| {
                self.typecheck_result
                    .expr_types
                    .get(&SpanKey::from_span(&e.span))
            })
            .map(type_display)
            .unwrap_or_else(|| "?".to_string());

        let file = if self.source_filename.is_empty() {
            "<unknown>".to_string()
        } else {
            self.source_filename.clone()
        };
        let value_str = val.debug_fmt();

        let line = match self.dbg_output_mode {
            DbgOutputMode::Terminal => match self.current_task_id {
                Some(tid) => format!(
                    "[task:{} {}:{}] {} = {}\n",
                    tid, file, span.line, expr_text, value_str
                ),
                None => format!("[{}:{}] {} = {}\n", file, span.line, expr_text, value_str),
            },
            DbgOutputMode::Json => {
                let task_id = match self.current_task_id {
                    Some(tid) => tid.to_string(),
                    None => "null".to_string(),
                };
                format!(
                    "{{\"kind\":\"dbg\",\"task_id\":{},\"file\":{},\"line\":{},\"expr\":{},\"type\":{},\"value\":{}}}\n",
                    task_id,
                    dbg_json_escape(&file),
                    span.line,
                    dbg_json_escape(&expr_text),
                    dbg_json_escape(&type_text),
                    dbg_json_escape(&value_str),
                )
            }
        };

        if let Some(ref mut cap) = self.captured_dbg {
            cap.push(line);
        } else {
            // Single atomic write — POSIX guarantees writes up to
            // PIPE_BUF bytes (4096 on Linux) are atomic at the
            // syscall level, so sibling-task lines never tear.
            use std::io::Write;
            let stderr = std::io::stderr();
            let mut handle = stderr.lock();
            let _ = handle.write_all(line.as_bytes());
        }

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

    /// Match `value` against a range pattern with optional `start` / `end`
    /// bounds. Bounds are integer or char literals (the parser limits
    /// `LiteralPattern` in range position to those two forms). Half-open
    /// forms — `lo..` (`end = None`), `..hi` (`start = None`) — accept
    /// everything past the present bound. Bounded-exclusive (`lo..hi`),
    /// bounded-inclusive (`lo..=hi`), and the half-open inclusive form
    /// (`..=hi`) all share the same comparison.
    fn value_in_range_pattern(
        value: &Value,
        start: Option<&LiteralPattern>,
        end: Option<&LiteralPattern>,
        inclusive: bool,
    ) -> bool {
        // Project the scrutinee value into a sortable scalar key (i128 to
        // accommodate i64 + char in the same comparison space).
        let key: i128 = match value {
            Value::Int(n) => *n as i128,
            Value::Char(c) => (*c as u32) as i128,
            _ => return false,
        };
        let bound_key = |lit: &LiteralPattern| -> Option<i128> {
            match lit {
                LiteralPattern::Integer(n, _) => Some(*n as i128),
                LiteralPattern::Char(c) => Some((*c as u32) as i128),
                _ => None,
            }
        };
        if let Some(lo) = start {
            let Some(lo_key) = bound_key(lo) else {
                return false;
            };
            if key < lo_key {
                return false;
            }
        }
        if let Some(hi) = end {
            let Some(hi_key) = bound_key(hi) else {
                return false;
            };
            if inclusive {
                if key > hi_key {
                    return false;
                }
            } else if key >= hi_key {
                return false;
            }
        }
        true
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

    /// Evaluate `lhs and rhs` / `lhs or rhs` with short-circuit
    /// semantics — RHS is only evaluated when the LHS doesn't already
    /// determine the result, so RHS side-effects (panicking index,
    /// dropped fn call) don't fire when short-circuited.
    fn eval_short_circuit(&mut self, op: &BinOp, left: &Expr, right: &Expr, span: &Span) -> Value {
        let lhs = match self.eval_expr_inner(left) {
            Value::Bool(b) => b,
            _ => unreachable!(
                "type mismatch in `{:?}` LHS at {}:{}; should be caught by typechecker",
                op, span.line, span.column
            ),
        };
        match (op, lhs) {
            (BinOp::And, false) => Value::Bool(false),
            (BinOp::Or, true) => Value::Bool(true),
            (BinOp::And, true) | (BinOp::Or, false) => self.eval_expr_inner(right),
            _ => unreachable!("eval_short_circuit only handles And/Or"),
        }
    }

    /// `Vec.filled(n, val)` — produce `n` clones of `val` per
    /// design.md:1631. Extracted to its own frame so the surrounding
    /// path-call match doesn't grow (debug-mode `eval_expr_inner`
    /// recursive callers are stack-budget-tight). Per-slot clone
    /// satisfies the spec's `T: Clone` requirement; negative length
    /// is a runtime error (Kāra has no usize).
    fn eval_vec_filled(&mut self, args: &[CallArg], span: &Span) -> Value {
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
    fn eval_vec_deque_method(
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

            // Comparison (String) — lexicographic via Rust's `Ord for String`.
            // Matches the typechecker's builtin Ord registration for `String`
            // (see `register_builtin_impl("Ord", "String", ...)`).
            (BinOp::Eq, Value::String(a), Value::String(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::String(a), Value::String(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::String(a), Value::String(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::String(a), Value::String(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::String(a), Value::String(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::String(a), Value::String(b)) => Value::Bool(a >= b),

            // Comparison (Char) — codepoint order via Rust's `Ord for char`.
            // Matches the typechecker's builtin Ord registration for `char`.
            (BinOp::Eq, Value::Char(a), Value::Char(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Char(a), Value::Char(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Char(a), Value::Char(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::Char(a), Value::Char(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Char(a), Value::Char(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::Char(a), Value::Char(b)) => Value::Bool(a >= b),

            // Logical (Bool)
            (BinOp::And, Value::Bool(a), Value::Bool(b)) => Value::Bool(a && b),
            (BinOp::Or, Value::Bool(a), Value::Bool(b)) => Value::Bool(a || b),
            (BinOp::Eq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Bool(a), Value::Bool(b)) => Value::Bool(!a & b),
            (BinOp::LtEq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Bool(a), Value::Bool(b)) => Value::Bool(a & !b),
            (BinOp::GtEq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a >= b),

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
            ExprKind::Identifier(_) | ExprKind::Path { .. } => {
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
