//! CLI command dispatch and compiler pipeline orchestration.
//!
//! Handles subcommand parsing, output modes (text/json/jsonl),
//! and running the appropriate compiler phases.

use crate::ast::EffectVerbKind;
use crate::ast::{Item, Program};
use crate::concurrency::ConcurrencyAnalysis;
use crate::effectchecker::{DeclaredEffects, EffectCheckResult, EffectErrorKind};
use crate::interpreter::{DbgOutputMode, ErrorTraceFrame, Interpreter, TestOutcome};
use crate::manifest;
use crate::module::{
    self, BuildTreeError, BuildTreeOk, BuildTreeOpts, Cycle, ModuleId, ModuleParseErrors,
    ProgramTree,
};
use crate::ownership::{OwnershipCheckResult, OwnershipMode};
use crate::parser::ParseResult;
use crate::resolver::ResolveResult;
use crate::resolver::{ResolveError, ResolveErrorKind, Resolver};
use crate::scaffold::{self, ScaffoldOpts, Template};
use crate::token::Span;
use crate::typechecker::TypeCheckResult;
use crate::walker::{self, EntryKind, WalkResult, WalkerOpts};
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::PathBuf;
use std::process;

mod args;
mod explain;
mod help;

pub use args::parse_args;
use help::print_help;

// ── Output Mode ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputMode {
    Text,
    Json,
    Jsonl,
}

// ── Subcommands ─────────────────────────────────────────────────

#[derive(Debug)]
pub enum Command {
    Run {
        file: String,
        output: OutputMode,
        sequential: bool,
    },
    RunExample {
        name: String,
        output: OutputMode,
        sequential: bool,
    },
    Check {
        file: String,
        output: OutputMode,
        /// Optional list of profiles to typecheck against. `None` means
        /// "use the default behavior — single pass at the manifest's
        /// (or `default`) profile". `Some(list)` means "run the full
        /// pipeline once per profile and group diagnostics per profile".
        /// `--profiles=all` expands to every known profile.
        profiles: Option<Vec<crate::manifest::CompileProfile>>,
        /// `--concurrency-report` (Slice D, 2026-05-08): also emit the
        /// human-readable concurrency analysis to stdout after checks
        /// complete. Already runs `concurrencycheck()` via
        /// `Pipeline::run_all_checks`, so wiring is purely render-side.
        concurrency_report: bool,
    },
    Build {
        file: String,
        output: OutputMode,
        /// `--concurrency-report` (Slice D, 2026-05-08): emit the
        /// human-readable concurrency analysis to stdout alongside the
        /// binary build. Pairs with the auto-par execution path landed
        /// in Slice A to make the compiler's reasoning visible alongside
        /// the speedup. See `docs/demo_ideas.md:80-88` for the locked
        /// output shape.
        concurrency_report: bool,
        /// `--offline`: read resolved dependencies only from the
        /// project-root `vendor/` directory (populated by
        /// `karac vendor`) and refuse any network access. Air-gap
        /// workflow per `design.md § Package System > Vendoring`.
        /// v1 surface — actual offline gating wires up alongside the
        /// dependency-resolution slice; v1 honors the flag at the
        /// arg-parsing layer and emits a "not yet wired" notice from
        /// the build command body so callers can scaffold their CI
        /// config against the canonical flag name today.
        offline: bool,
    },
    /// Project-mode build: no file argument. Walks up from CWD to find
    /// `kara.toml`, loads the manifest, and (once CR-24 slices 3+ land) runs
    /// the multi-file pipeline. In slice 2 this is a stub that loads the
    /// manifest and reports. Missing manifest → E0227 NotInsideKaraProject.
    BuildProject {
        output: OutputMode,
        /// `--offline` — see `Build.offline` above. Same v1 contract.
        offline: bool,
    },
    Query {
        kind: QueryKind,
        file: String,
        function: String,
    },
    Fmt {
        file: String,
    },
    /// Apply machine-applicable suggestions back into the source file.
    /// v1 covers `did you mean` corrections on undefined names / types
    /// emitted by the resolver. With `--dry-run`, prints the would-be
    /// rewrites without touching disk.
    Fix {
        file: String,
        dry_run: bool,
    },
    /// Scaffold a new Kāra project. Bare `karac init` scaffolds into the
    /// current directory; `karac init <name>` creates `./<name>/` first. See
    /// `docs/design.md § Package System § Project Scaffolding`.
    Init {
        /// When `Some(name)`, create `./<name>/` and scaffold there.
        directory: Option<String>,
        template: Template,
        force: bool,
    },
    /// Run the project's tests. Walks the project root, discovers
    /// `_test.kara` files, merges them into their production sibling
    /// modules, and invokes every `test_*` function via the interpreter.
    /// Output schema documented in `docs/design.md § Testing › Test
    /// runner output format`.
    Test {
        /// Optional substring filter — only tests whose fully-qualified ID
        /// (`<module_path>::<fn_name>`) contains this substring run.
        filter: Option<String>,
        /// Promote skipped tests to failures. Tests gated by
        /// `#[test(requires = [...])]` skip silently when their resources
        /// are unavailable; with `--all` the runner instead emits
        /// `test_fail` (with `reason: "unsatisfied_requires"`) and the
        /// process exits non-zero. Used in CI when every required service
        /// must be live.
        all: bool,
    },
    /// Launch the interactive REPL over the tree-walk interpreter. P0
    /// delivery item per `roadmap.md § Interactive Development`. See
    /// `src/repl.rs` for the cell-scope semantics. Flags mirror
    /// `repl::ReplOptions` and are surfaced through the `--auto-clone`
    /// CLI form (and, eventually, `%set auto-clone on` once the kernel
    /// magic ships).
    Repl {
        /// `--auto-clone`: opt-in cross-cell ownership ergonomics — the
        /// REPL auto-inserts `.clone()` at consume sites flagged by a
        /// cross-cell `UseAfterMove`. Each insertion emits a
        /// `perf[auto-clone-in-repl]` note (never silent).
        auto_clone: bool,
    },
    /// Walk the project, parse every module, render one HTML page per
    /// documented item under `dist/doc/`. v1 MVP — no cross-references,
    /// no effect display, flat per-module directory layout.
    Doc,
    /// Remove the project's build artifact cache. Bare form deletes the
    /// project-local `dist/` directory (idempotent — a missing directory
    /// is not an error). `--global` instead targets the user-wide cache
    /// at `~/.kara/cache/` per `design.md § Package System > Build
    /// artifact cache`.
    Clean {
        global: bool,
    },
    /// Build a binary package and install it into `~/.kara/bin/`. The
    /// `spec` accepts `path = ...`, `git = ...`, or a registry-proxy
    /// reference per the manifest dependency spec shape. v1 surface —
    /// the full resolver wiring lands in a follow-up alongside the
    /// dependency-resolution slice; this arm parses the invocation and
    /// emits a "not yet wired" diagnostic until then.
    Install {
        spec: String,
    },
    /// Copy all resolved dependencies into a project-root `vendor/`
    /// directory. Subsequent `karac build --offline` reads from
    /// `vendor/` and refuses network access. v1 surface — the resolver
    /// wiring lands in a follow-up; this arm currently emits a
    /// "not yet wired" diagnostic.
    Vendor,
    /// Concept-level explainer surface. `karac explain --concept=closures`
    /// renders a per-concept page covering the relevant analysis rules,
    /// diagnostic shapes, and inspection commands. The concept name is
    /// validated against the registered set at render time so a typo
    /// produces a focused diagnostic listing the supported set.
    Explain {
        concept: String,
    },
    Help,
    Version,
}

#[derive(Debug)]
pub enum QueryKind {
    Effects,
    Ownership,
    Concurrency,
    /// Whole-file cost-surface aggregator. Unlike the per-function query
    /// kinds above, this one ignores the `function` slot — the static
    /// counts are reported per-function inside the JSON envelope.
    CostSummary,
}

// ── Command Execution ───────────────────────────────────────────

pub fn execute(cmd: Command) {
    match cmd {
        Command::Help => print_help(),
        Command::Version => println!("karac 0.1.0"),
        Command::Run {
            file,
            output,
            sequential,
        } => cmd_run(&file, output, sequential),
        Command::RunExample {
            name,
            output,
            sequential,
        } => cmd_run_example(&name, output, sequential),
        Command::Check {
            file,
            output,
            profiles,
            concurrency_report,
        } => cmd_check(&file, output, profiles, concurrency_report),
        Command::Build {
            file,
            output,
            concurrency_report,
            offline,
        } => cmd_build(&file, output, concurrency_report, offline),
        Command::BuildProject { output, offline } => cmd_build_project(output, offline),
        Command::Query {
            kind,
            file,
            function,
        } => cmd_query(kind, &file, &function),
        Command::Fmt { file } => cmd_fmt(&file),
        Command::Fix { file, dry_run } => cmd_fix(&file, dry_run),
        Command::Init {
            directory,
            template,
            force,
        } => cmd_init(directory, template, force),
        Command::Test { filter, all } => cmd_test(filter, all),
        Command::Repl { auto_clone } => {
            crate::repl::run_with_options(crate::repl::ReplOptions { auto_clone })
        }
        Command::Doc => cmd_doc(),
        Command::Clean { global } => cmd_clean(global),
        Command::Install { spec } => cmd_install(&spec),
        Command::Vendor => cmd_vendor(),
        Command::Explain { concept } => explain::render(&concept),
    }
}

// ── Read Source ──────────────────────────────────────────────────

fn read_source(filename: &str) -> String {
    match fs::read_to_string(filename) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read '{filename}': {e}");
            process::exit(1);
        }
    }
}

// ── Pipeline Phases ─────────────────────────────────────────────

struct Pipeline {
    filename: String,
    parsed: ParseResult,
    resolved: Option<ResolveResult>,
    typed: Option<TypeCheckResult>,
    effects: Option<EffectCheckResult>,
    ownership: Option<OwnershipCheckResult>,
    concurrency: Option<ConcurrencyAnalysis>,
    provider_escape: Option<Vec<crate::provider_escape::EscapeError>>,
    profile: crate::manifest::CompileProfile,
}

impl Pipeline {
    fn new(filename: &str, source: &str) -> Self {
        let parsed = crate::parse(source);
        Pipeline {
            filename: filename.to_string(),
            parsed,
            resolved: None,
            typed: None,
            effects: None,
            ownership: None,
            concurrency: None,
            provider_escape: None,
            profile: crate::manifest::CompileProfile::Default,
        }
    }

    fn has_parse_errors(&self) -> bool {
        !self.parsed.errors.is_empty()
    }

    fn resolve(&mut self) {
        if self.has_parse_errors() {
            return;
        }
        self.resolved = Some(crate::resolve(&self.parsed.program));
    }

    fn has_resolve_errors(&self) -> bool {
        self.resolved.as_ref().is_some_and(|r| !r.errors.is_empty())
    }

    fn typecheck(&mut self) {
        if self.resolved.is_none() || self.has_resolve_errors() {
            return;
        }
        self.typed = Some(crate::typecheck(
            &self.parsed.program,
            self.resolved.as_ref().unwrap(),
        ));
    }

    /// Apply the operator-lowering pass. Runs after typecheck (uses inferred
    /// operand types) and before any downstream phase that consumes the AST
    /// (effectcheck / ownership / interpreter / codegen).
    fn lower(&mut self) {
        if let Some(ref typed) = self.typed {
            crate::lower(&mut self.parsed.program, typed);
        }
    }

    fn effectcheck(&mut self) {
        if self.has_parse_errors() {
            return;
        }
        // Thread the typechecker's `method_callee_types` resolution table so
        // method-call sites can reach the same `with E` / Fn-slot / polymorphic
        // arg propagation paths the free-call form already gets. Falls back to
        // an empty map when typecheck didn't run (e.g. resolve errors aborted
        // earlier in the pipeline). `call_type_subs` is threaded alongside so
        // E0404 diagnostics on compound polymorphic calls can render a fully
        // monomorphized callee signature (Round 10.3 step 7).
        let method_types = self
            .typed
            .as_ref()
            .map(|t| t.method_callee_types.clone())
            .unwrap_or_default();
        let call_type_subs = self
            .typed
            .as_ref()
            .map(|t| t.call_type_subs.clone())
            .unwrap_or_default();
        self.effects = Some(crate::effectcheck_with_typecheck_data(
            &self.parsed.program,
            crate::effectchecker::PublicEffectsPolicy::default(),
            self.profile,
            method_types,
            call_type_subs,
        ));
        // Populate `Program.callee_effectful` from the effect-check result so
        // codegen can narrow the par-branch cooperative cancel-check to calls
        // whose callee actually carries reads/writes/sends/receives. Mirrors
        // the wiring of `Program.question_conversions` from the lowering pass.
        if let Some(ref effects) = self.effects {
            self.parsed.program.callee_effectful = build_callee_effectful_table(effects);
        }
    }

    fn ownershipcheck(&mut self) {
        if self.typed.is_none() {
            return;
        }
        self.ownership = Some(crate::ownershipcheck(
            &self.parsed.program,
            self.typed.as_ref().unwrap(),
        ));
    }

    fn concurrencycheck(&mut self) {
        if self.effects.is_none() {
            return;
        }
        self.concurrency = Some(crate::concurrency_analyze(
            &self.parsed.program,
            self.effects.as_ref().unwrap(),
        ));
    }

    fn provider_escape_check(&mut self) {
        if self.has_parse_errors() {
            return;
        }
        self.provider_escape = Some(crate::provider_escape_check(
            &self.parsed.program,
            self.typed.as_ref(),
        ));
    }

    /// Run all analysis phases (no execution).
    fn run_all_checks(&mut self) {
        self.resolve();
        self.typecheck();
        self.lower();
        self.effectcheck();
        self.ownershipcheck();
        self.concurrencycheck();
        self.provider_escape_check();
    }

    /// Collect all errors across phases.
    fn has_fatal_errors(&self) -> bool {
        self.has_parse_errors() || self.has_resolve_errors()
    }

    fn total_errors(&self) -> usize {
        let mut n = self.parsed.errors.len();
        if let Some(ref r) = self.resolved {
            n += r.errors.len();
        }
        if let Some(ref t) = self.typed {
            n += t.errors.len();
        }
        if let Some(ref e) = self.effects {
            n += e
                .errors
                .iter()
                .filter(|e| e.kind != EffectErrorKind::FfiLintHint)
                .count();
        }
        if let Some(ref o) = self.ownership {
            n += o.errors.len();
        }
        if let Some(ref esc) = self.provider_escape {
            n += esc.len();
        }
        n
    }
}

// ── Text Output ─────────────────────────────────────────────────

fn print_text_diagnostics(pipeline: &Pipeline) {
    let filename = &pipeline.filename;
    for err in &pipeline.parsed.errors {
        eprintln!(
            "error[parse]: {}:{}:{}: {}",
            filename, err.span.line, err.span.column, err.message
        );
    }
    if let Some(ref r) = pipeline.resolved {
        for err in &r.errors {
            eprintln!(
                "error[resolve]: {}:{}:{}: {}",
                filename, err.span.line, err.span.column, err.message
            );
        }
    }
    if let Some(ref t) = pipeline.typed {
        for err in &t.errors {
            eprintln!(
                "error[typecheck]: {}:{}:{}: {}",
                filename, err.span.line, err.span.column, err.message
            );
        }
    }
    if let Some(ref e) = pipeline.effects {
        for err in &e.errors {
            if err.kind == EffectErrorKind::FfiLintHint {
                eprintln!(
                    "note[effect]: {}:{}:{}: {}",
                    filename, err.span.line, err.span.column, err.message
                );
            } else {
                eprintln!(
                    "error[effect]: {}:{}:{}: {}",
                    filename, err.span.line, err.span.column, err.message
                );
            }
        }
    }
    if let Some(ref o) = pipeline.ownership {
        for err in &o.errors {
            eprintln!(
                "error[ownership]: {}:{}:{}: {}",
                filename, err.span.line, err.span.column, err.message
            );
        }
    }
    if let Some(ref esc) = pipeline.provider_escape {
        for err in esc {
            eprintln!(
                "error[provider_escape]: {}:{}:{}: {}",
                filename,
                err.closure_span.line,
                err.closure_span.column,
                err.message()
            );
        }
    }
}

// ── JSON Output ─────────────────────────────────────────────────

fn span_to_json(span: &Span, filename: &str) -> String {
    format!(
        "\"file\":{},\"line\":{},\"column\":{}",
        json_string(filename),
        span.line,
        span.column
    )
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\x20' => {
                write!(out, "\\u{:04x}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_string_list(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| json_string(s)).collect();
    format!("[{}]", parts.join(","))
}

/// Build the per-callee "is effectful" side-table from an `EffectCheckResult`.
///
/// A callee is "effectful" iff its inferred or declared effect set contains
/// any `reads` / `writes` / `sends` / `receives` verb (the four verbs that
/// drive cooperative-cancellation observability). `allocates`, `panics`,
/// `blocks`, `suspends`, `UserDefined` are excluded — they don't motivate
/// the per-call cancel check.
fn build_callee_effectful_table(
    effects: &EffectCheckResult,
) -> std::collections::HashMap<String, bool> {
    fn set_is_effectful(set: &crate::effectchecker::EffectSet) -> bool {
        set.effects.iter().any(|t| {
            matches!(
                t.effect.verb,
                EffectVerbKind::Reads
                    | EffectVerbKind::Writes
                    | EffectVerbKind::Sends
                    | EffectVerbKind::Receives
            )
        })
    }
    let mut table = std::collections::HashMap::new();
    for (name, set) in &effects.inferred_effects {
        table.insert(name.clone(), set_is_effectful(set));
    }
    for (name, decl) in &effects.declared_effects {
        // Polymorphic / PolymorphicWithFixed callees may carry effects per
        // monomorphization — treat them as effectful (conservative).
        let effectful = match decl {
            DeclaredEffects::Explicit(set) => set_is_effectful(set),
            // The polymorphic portion may pick up any effect at a
            // monomorphization site, so treat as effectful even if the fixed
            // set is empty.
            DeclaredEffects::PolymorphicWithFixed(_) | DeclaredEffects::Polymorphic => true,
            DeclaredEffects::None => false,
        };
        table
            .entry(name.clone())
            .and_modify(|v| *v = *v || effectful)
            .or_insert(effectful);
    }
    table
}

fn effect_verb_str(v: &EffectVerbKind) -> &str {
    match v {
        EffectVerbKind::Reads => "reads",
        EffectVerbKind::Writes => "writes",
        EffectVerbKind::Sends => "sends",
        EffectVerbKind::Receives => "receives",
        EffectVerbKind::Allocates => "allocates",
        EffectVerbKind::Panics => "panics",
        EffectVerbKind::Blocks => "blocks",
        EffectVerbKind::Suspends => "suspends",
        EffectVerbKind::UserDefined(s) => s.as_str(),
    }
}

fn ownership_mode_str(m: &OwnershipMode) -> &str {
    match m {
        OwnershipMode::Own => "own",
        OwnershipMode::Ref => "ref",
        OwnershipMode::MutRef => "mut_ref",
    }
}

struct DiagEntry<'a> {
    id: &'a str,
    severity: &'a str,
    phase: &'a str,
    code: &'a str,
    category: &'a str,
    message: &'a str,
    filename: &'a str,
    span: &'a Span,
    suggestion: Option<&'a str>,
    /// Optional pre-formatted JSON fields appended verbatim to the entry object.
    extra_json: Option<String>,
}

struct DiagnosticJson {
    entries: Vec<String>,
}

impl DiagnosticJson {
    fn new() -> Self {
        DiagnosticJson {
            entries: Vec::new(),
        }
    }

    fn add(&mut self, d: DiagEntry<'_>) {
        let mut entry = format!(
            "{{\"id\":{},\"severity\":{},\"primary\":true,\"phase\":{},\"code\":{},\"category\":{},{},\"message\":{}",
            json_string(d.id),
            json_string(d.severity),
            json_string(d.phase),
            json_string(d.code),
            json_string(d.category),
            span_to_json(d.span, d.filename),
            json_string(d.message),
        );
        if let Some(s) = d.suggestion {
            write!(entry, ",\"hints\":[{{\"description\":{}}}]", json_string(s)).unwrap();
        }
        if let Some(ref extra) = d.extra_json {
            write!(entry, ",{}", extra).unwrap();
        }
        entry.push('}');
        self.entries.push(entry);
    }

    fn to_json_array(&self) -> String {
        if self.entries.is_empty() {
            return "[]".to_string();
        }
        format!("[{}]", self.entries.join(","))
    }
}

fn collect_diagnostics(pipeline: &Pipeline) -> DiagnosticJson {
    let mut diags = DiagnosticJson::new();
    let filename = &pipeline.filename;
    let mut id_counter = 0u32;

    for err in &pipeline.parsed.errors {
        id_counter += 1;
        diags.add(DiagEntry {
            id: &format!("d{id_counter}"),
            severity: "error",
            phase: "parse",
            code: "E0001",
            category: "parse",
            message: &err.message,
            filename,
            span: &err.span,
            suggestion: None,
            extra_json: None,
        });
    }

    if let Some(ref r) = pipeline.resolved {
        for err in &r.errors {
            id_counter += 1;
            let code = match err.kind {
                crate::resolver::ResolveErrorKind::UndefinedName => "E0100",
                crate::resolver::ResolveErrorKind::DuplicateDefinition => "E0101",
                crate::resolver::ResolveErrorKind::ReservedIdentifier => "E0102",
                crate::resolver::ResolveErrorKind::PrivateAccess => "E0103",
                crate::resolver::ResolveErrorKind::UndefinedType => "E0104",
                crate::resolver::ResolveErrorKind::UndefinedVariant => "E0105",
                crate::resolver::ResolveErrorKind::UndefinedField => "E0106",
                crate::resolver::ResolveErrorKind::UndefinedLabel => "E0107",
                crate::resolver::ResolveErrorKind::OperatorTraitImplRestricted => "E0108",
                crate::resolver::ResolveErrorKind::IntoTraitImplNotAllowed => "E0109",
                crate::resolver::ResolveErrorKind::ImplLevelEffectVarNotAllowed => "E0110",
                crate::resolver::ResolveErrorKind::UnknownModule => "E0224",
                crate::resolver::ResolveErrorKind::UnknownItemInModule => "E0225",
                crate::resolver::ResolveErrorKind::PrivateItemAccess => "E0222",
                crate::resolver::ResolveErrorKind::ReservedEffectResource => "E0228",
                crate::resolver::ResolveErrorKind::CompilerBuiltinReserved => "E0237",
                crate::resolver::ResolveErrorKind::ContinueOnBlockLabel => "E0238",
                crate::resolver::ResolveErrorKind::NonExhaustiveInvalidTarget => "E0239",
                crate::resolver::ResolveErrorKind::TrackCallerInvalidTarget => "E0240",
                crate::resolver::ResolveErrorKind::DeprecatedOnImpl => "E0241",
                crate::resolver::ResolveErrorKind::DeprecatedOnField => "E0242",
            };
            // Surface the machine-applicable replacement (when present)
            // alongside the human-readable suggestion. Consumers like
            // `karac fix` and IDE quick-fix UIs read this directly to
            // produce one-click rewrites.
            let replacement_json = err.replacement.as_ref().map(|r| {
                format!(
                    "\"replacement\":{{\"offset\":{},\"length\":{},\"text\":{}}}",
                    r.offset,
                    r.length,
                    json_string(&r.replacement),
                )
            });
            diags.add(DiagEntry {
                id: &format!("d{id_counter}"),
                severity: "error",
                phase: "resolve",
                code,
                category: "resolve",
                message: &err.message,
                filename,
                span: &err.span,
                suggestion: err.suggestion.as_deref(),
                extra_json: replacement_json,
            });
        }
    }

    if let Some(ref t) = pipeline.typed {
        for err in &t.errors {
            id_counter += 1;
            let code = match err.kind {
                crate::typechecker::TypeErrorKind::TypeMismatch => "E0200",
                crate::typechecker::TypeErrorKind::UndefinedField => "E0201",
                crate::typechecker::TypeErrorKind::WrongNumberOfArgs => "E0202",
                crate::typechecker::TypeErrorKind::MissingField => "E0203",
                crate::typechecker::TypeErrorKind::ExtraField => "E0204",
                crate::typechecker::TypeErrorKind::NonExhaustiveMatch => "E0205",
                crate::typechecker::TypeErrorKind::NotCallable => "E0206",
                crate::typechecker::TypeErrorKind::NotAStruct => "E0207",
                crate::typechecker::TypeErrorKind::InvalidBinaryOp => "E0208",
                crate::typechecker::TypeErrorKind::InvalidUnaryOp => "E0209",
                crate::typechecker::TypeErrorKind::InvalidCast => "E0210",
                crate::typechecker::TypeErrorKind::ConditionNotBool => "E0211",
                crate::typechecker::TypeErrorKind::BranchTypeMismatch => "E0212",
                crate::typechecker::TypeErrorKind::ReturnTypeMismatch => "E0213",
                crate::typechecker::TypeErrorKind::InvalidTupleIndex => "E0214",
                crate::typechecker::TypeErrorKind::LabelMismatch => "E0215",
                crate::typechecker::TypeErrorKind::NonContiguousLabels => "E0216",
                crate::typechecker::TypeErrorKind::InvalidPipePlaceholder => "E0217",
                crate::typechecker::TypeErrorKind::MissingMutMarker => "E0218",
                crate::typechecker::TypeErrorKind::InvalidMutMarker => "E0219",
                crate::typechecker::TypeErrorKind::UnsupportedNumericSuffix => "E0220",
                crate::typechecker::TypeErrorKind::PrivateTypeInPublicSignature => "E0221",
                crate::typechecker::TypeErrorKind::RefutablePattern => "E0222",
                crate::typechecker::TypeErrorKind::MissingSupertrait => "E0229",
                crate::typechecker::TypeErrorKind::TraitBoundNotSatisfied => "E0232",
                crate::typechecker::TypeErrorKind::AmbiguousAssocFn => "E0233",
                crate::typechecker::TypeErrorKind::CannotInferAssocFn => "E0234",
                crate::typechecker::TypeErrorKind::OnceFnIntoFnSlot => "E0235",
                crate::typechecker::TypeErrorKind::NoMethodFound => "E0236",
                crate::typechecker::TypeErrorKind::UnreachableArm => "W0237",
                crate::typechecker::TypeErrorKind::CannotInferTypeParam => "E0238",
                crate::typechecker::TypeErrorKind::AmbiguousMethod => "E0239",
                crate::typechecker::TypeErrorKind::ConflictingImpl => "E0240",
            };
            diags.add(DiagEntry {
                id: &format!("d{id_counter}"),
                severity: "error",
                phase: "typecheck",
                code,
                category: "typecheck",
                message: &err.message,
                filename,
                span: &err.span,
                suggestion: None,
                extra_json: None,
            });
        }
        for warn in &t.warnings {
            id_counter += 1;
            let code = match warn.kind {
                crate::typechecker::TypeErrorKind::UnreachableArm => "W0237",
                // Other kinds aren't expected to appear as warnings today.
                _ => "W0299",
            };
            diags.add(DiagEntry {
                id: &format!("d{id_counter}"),
                severity: "warning",
                phase: "typecheck",
                code,
                category: "typecheck",
                message: &warn.message,
                filename,
                span: &warn.span,
                suggestion: None,
                extra_json: None,
            });
        }
    }

    if let Some(ref e) = pipeline.effects {
        for err in &e.errors {
            id_counter += 1;
            let (code, severity) = match err.kind {
                crate::effectchecker::EffectErrorKind::MissingEffectDeclaration => {
                    ("E0400", "error")
                }
                crate::effectchecker::EffectErrorKind::OverDeclaredEffect => ("E0401", "error"),
                crate::effectchecker::EffectErrorKind::CircularEffectGroup => ("E0402", "error"),
                crate::effectchecker::EffectErrorKind::UndefinedEffectGroup => ("E0403", "error"),
                crate::effectchecker::EffectErrorKind::EffectSubtypeViolation => ("E0404", "error"),
                crate::effectchecker::EffectErrorKind::ProfileViolation => ("E0405", "error"),
                crate::effectchecker::EffectErrorKind::ImplExceedsTraitCeiling => {
                    ("E0230", "error")
                }
                crate::effectchecker::EffectErrorKind::TraitDefaultExceedsCeiling => {
                    ("E0231", "error")
                }
                crate::effectchecker::EffectErrorKind::FfiLintHint => ("L0001", "note"),
                crate::effectchecker::EffectErrorKind::EffectVariableConflict => ("E0406", "error"),
            };
            let extra_json = err.subtype_trace.as_ref().map(|t| {
                let slot = json_string_list(&t.slot_effects);
                let arg = json_string_list(&t.argument_effects);
                let offending = json_string_list(&t.offending_effects);
                let signature_json = match &t.monomorphized_signature {
                    Some(sig) => format!(",\"signature\":{}", json_string(sig)),
                    None => String::new(),
                };
                format!(
                    "\"effect-subset-fail\":{{\"slot\":{slot},\"argument\":{arg},\"offending\":{offending}{signature_json}}}"
                )
            });
            diags.add(DiagEntry {
                id: &format!("d{id_counter}"),
                severity,
                phase: "effect",
                code,
                category: "effects",
                message: &err.message,
                filename,
                span: &err.span,
                suggestion: None,
                extra_json,
            });
        }
    }

    if let Some(ref o) = pipeline.ownership {
        for err in &o.errors {
            id_counter += 1;
            let code = match err.kind {
                crate::ownership::OwnershipErrorKind::UseAfterMove => "E0500",
                crate::ownership::OwnershipErrorKind::OwnershipCycle => "E0501",
                crate::ownership::OwnershipErrorKind::NoRcViolation => "E0502",
                crate::ownership::OwnershipErrorKind::RcFallbackNote => "N0503",
                crate::ownership::OwnershipErrorKind::CaptureModeViolation => "E0504",
                crate::ownership::OwnershipErrorKind::UseOfUninitialized => "E0505",
                crate::ownership::OwnershipErrorKind::ReassignToImmutable => "E0506",
                crate::ownership::OwnershipErrorKind::UnusedMutCaptureNote => "N0507",
                crate::ownership::OwnershipErrorKind::RefCaptureEscapesScope => "E0508",
                crate::ownership::OwnershipErrorKind::SliceFromTemporaryEscapes => {
                    "E_SLICE_FROM_TEMPORARY_ESCAPES"
                }
                crate::ownership::OwnershipErrorKind::SliceBorrowConflict { .. } => {
                    "E_SLICE_BORROW_CONFLICT"
                }
                crate::ownership::OwnershipErrorKind::CrossBorrowConflict => {
                    "E_CROSS_BORROW_CONFLICT"
                }
                crate::ownership::OwnershipErrorKind::ClosureCaptureBorrowConflict => {
                    "E_CLOSURE_CAPTURE_BORROW_CONFLICT"
                }
            };
            let replacement_json = err.replacement.as_ref().map(|r| {
                format!(
                    "\"replacement\":{{\"offset\":{},\"length\":{},\"text\":{}}}",
                    r.offset,
                    r.length,
                    json_string(&r.replacement),
                )
            });
            diags.add(DiagEntry {
                id: &format!("d{id_counter}"),
                severity: "error",
                phase: "ownership",
                code,
                category: "ownership",
                message: &err.message,
                filename,
                span: &err.span,
                suggestion: err.suggestion.as_deref(),
                extra_json: replacement_json,
            });
        }
        for note in &o.notes {
            id_counter += 1;
            let code = match note.kind {
                crate::ownership::OwnershipErrorKind::UnusedMutCaptureNote => "N0507",
                _ => "N0503",
            };
            let replacement_json = note.replacement.as_ref().map(|r| {
                format!(
                    "\"replacement\":{{\"offset\":{},\"length\":{},\"text\":{}}}",
                    r.offset,
                    r.length,
                    json_string(&r.replacement),
                )
            });
            diags.add(DiagEntry {
                id: &format!("d{id_counter}"),
                severity: "note",
                phase: "ownership",
                code,
                category: "ownership",
                message: &note.message,
                filename,
                span: &note.span,
                suggestion: note.suggestion.as_deref(),
                extra_json: replacement_json,
            });
        }
    }

    if let Some(ref esc) = pipeline.provider_escape {
        for err in esc {
            id_counter += 1;
            let message = err.message();
            diags.add(DiagEntry {
                id: &format!("d{id_counter}"),
                severity: "error",
                phase: "provider_escape",
                code: "E0600",
                category: "provider_escape",
                message: &message,
                filename,
                span: &err.closure_span,
                suggestion: None,
                extra_json: None,
            });
        }
    }

    diags
}

fn program_effects_json(pipeline: &Pipeline) -> String {
    match &pipeline.effects {
        Some(effects) => {
            // Collect all effects from main() or program-level
            let mut all_effects: Vec<String> = Vec::new();
            if let Some(main_effects) = effects.inferred_effects.get("main") {
                for te in &main_effects.effects {
                    all_effects.push(format!(
                        "{}({})",
                        effect_verb_str(&te.effect.verb),
                        te.effect.resource
                    ));
                }
            }
            if all_effects.is_empty() {
                "[]".to_string()
            } else {
                json_string_list(&all_effects)
            }
        }
        None => "null".to_string(),
    }
}

fn public_function_effects_json(pipeline: &Pipeline) -> String {
    let Some(effects) = &pipeline.effects else {
        return "{}".to_string();
    };
    let mut names: Vec<&String> = effects
        .function_visibility
        .iter()
        .filter_map(|(n, is_pub)| {
            if *is_pub && n != "main" {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    if names.is_empty() {
        return "{}".to_string();
    }
    let entries: Vec<String> = names
        .iter()
        .map(|name| {
            let list: Vec<String> = effects
                .inferred_effects
                .get(*name)
                .map(|set| {
                    set.effects
                        .iter()
                        .map(|te| {
                            format!(
                                "{}({})",
                                effect_verb_str(&te.effect.verb),
                                te.effect.resource
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!("{}:{}", json_string(name), json_string_list(&list))
        })
        .collect();
    format!("{{{}}}", entries.join(","))
}

fn mutual_recursion_groups_json(pipeline: &Pipeline) -> String {
    match &pipeline.effects {
        Some(effects) => {
            if effects.mutual_recursion_groups.is_empty() {
                return "[]".to_string();
            }
            let groups: Vec<String> = effects
                .mutual_recursion_groups
                .iter()
                .map(|g| {
                    let funcs = json_string_list(&g.functions);
                    let traces: Vec<String> = g
                        .resolution_trace
                        .iter()
                        .map(|r| {
                            format!(
                                "{{\"call_site\":\"{}:{}\",\"resolved_via\":{},\"effect\":{}}}",
                                r.call_site_function,
                                r.call_site_line,
                                json_string(&r.resolved_via),
                                json_string(&r.effect),
                            )
                        })
                        .collect();
                    format!(
                        "{{\"functions\":{},\"resolution_trace\":[{}]}}",
                        funcs,
                        traces.join(","),
                    )
                })
                .collect();
            format!("[{}]", groups.join(","))
        }
        None => "[]".to_string(),
    }
}

/// Render a `crate::unsafe_lint::LintDiagnostic` in rustc-style format:
/// the primary line plus optional `= note:` and `= help:` continuation
/// lines. The `note:` carries the conceptual explanation (e.g. the two
/// distinct roles of `unsafe`); the `help:` carries the actionable
/// suggestion (wrap in `unsafe { ... }` and add a `// Safety:` comment).
fn render_unsafe_lint_diag(diag: &crate::unsafe_lint::LintDiagnostic, filename: &str) {
    eprintln!(
        "{}[{}]: {}:{}:{}: {}",
        if diag.level == crate::unsafe_lint::LintLevel::Error {
            "error"
        } else {
            "warning"
        },
        diag.lint_name,
        filename,
        diag.span.line,
        diag.span.column,
        diag.message
    );
    if let Some(note) = &diag.note {
        eprintln!("   = note: {note}");
    }
    if let Some(help) = &diag.help {
        eprintln!("   = help: {help}");
    }
}

/// Render a `crate::must_use_lint::LintDiagnostic` in the same
/// rustc-style three-piece shape (primary / `= note:` / `= help:`) as
/// `render_unsafe_lint_diag`. Kept parallel rather than unified because
/// each lint module currently owns its own `LintDiagnostic` struct (the
/// pre-existing pattern across `unsafe_lint`, `logical_lint`,
/// `ffi_lint`); a future lint-registry refactor (`docs/implementation_
/// checklist/phase-5-diagnostics.md` § "Lint level attributes") would
/// unify these.
fn render_must_use_lint_diag(diag: &crate::must_use_lint::LintDiagnostic, filename: &str) {
    eprintln!(
        "{}[{}]: {}:{}:{}: {}",
        if diag.level == crate::must_use_lint::LintLevel::Error {
            "error"
        } else {
            "warning"
        },
        diag.lint_name,
        filename,
        diag.span.line,
        diag.span.column,
        diag.message
    );
    if let Some(note) = &diag.note {
        eprintln!("   = note: {note}");
    }
    if let Some(help) = &diag.help {
        eprintln!("   = help: {help}");
    }
}

/// Render a `crate::missing_must_use_lint::LintDiagnostic` in the same
/// rustc-style three-piece shape. Structurally identical to
/// `render_must_use_lint_diag` — the two `LintDiagnostic` types share
/// shape but live in separate modules to keep each lint self-contained
/// (the established pattern across `unsafe_lint`, `must_use_lint`,
/// `logical_lint`, `ffi_lint`). A future lint-registry refactor (per
/// the deferred "Lint level attributes" entry in
/// `phase-5-diagnostics.md`) would unify these renderers.
fn render_missing_must_use_lint_diag(
    diag: &crate::missing_must_use_lint::LintDiagnostic,
    filename: &str,
) {
    eprintln!(
        "{}[{}]: {}:{}:{}: {}",
        if diag.level == crate::missing_must_use_lint::LintLevel::Error {
            "error"
        } else {
            "warning"
        },
        diag.lint_name,
        filename,
        diag.span.line,
        diag.span.column,
        diag.message
    );
    if let Some(note) = &diag.note {
        eprintln!("   = note: {note}");
    }
    if let Some(help) = &diag.help {
        eprintln!("   = help: {help}");
    }
}

fn emit_json_output(pipeline: &Pipeline) {
    let diags = collect_diagnostics(pipeline);
    let effects = program_effects_json(pipeline);
    let pub_effects = public_function_effects_json(pipeline);
    let mrg = mutual_recursion_groups_json(pipeline);
    println!(
        "{{\"program_effects\":{},\"public_function_effects\":{},\"mutual_recursion_groups\":{},\"diagnostics\":{}}}",
        effects,
        pub_effects,
        mrg,
        diags.to_json_array()
    );
}

// ── JSONL Streaming Output ──────────────────────────────────────

fn emit_jsonl_event(event_type: &str, fields: &str) {
    println!("{{\"type\":{},{}}}", json_string(event_type), fields);
}

fn run_pipeline_jsonl(pipeline: &mut Pipeline) {
    let filename = &pipeline.filename.clone();

    // build_start
    emit_jsonl_event(
        "build_start",
        &format!("\"timestamp\":\"\",\"files\":[{}]", json_string(filename)),
    );

    // lex phase (already done during parse)
    emit_jsonl_event(
        "phase_start",
        &format!(
            "\"phase\":\"lex\",\"scope\":{{\"files\":[{}]}}",
            json_string(filename)
        ),
    );
    emit_jsonl_event(
        "phase_complete",
        "\"phase\":\"lex\",\"errors\":0,\"warnings\":0,\"notes\":0",
    );

    // parse phase
    emit_jsonl_event("phase_start", "\"phase\":\"parse\"");
    let parse_errors = pipeline.parsed.errors.len();
    if parse_errors > 0 {
        let diags = collect_diagnostics(pipeline);
        for entry in &diags.entries {
            // Re-emit parse diagnostics as streaming events
            println!("{entry}");
        }
    }
    emit_jsonl_event(
        "phase_complete",
        &format!(
            "\"phase\":\"parse\",\"errors\":{},\"warnings\":0,\"notes\":0",
            parse_errors
        ),
    );

    if pipeline.has_parse_errors() {
        // Skip remaining phases
        for phase in &["resolve", "typecheck", "effect", "ownership"] {
            emit_jsonl_event(
                "phase_skipped",
                &format!(
                    "\"phase\":{},\"reason\":\"parse errors in input\",\"blocking\":[\"d1\"]",
                    json_string(phase)
                ),
            );
        }
        emit_jsonl_event(
            "build_complete",
            &format!(
                "\"success\":false,\"total_errors\":{},\"total_warnings\":0,\"program_effects\":null",
                parse_errors
            ),
        );
        return;
    }

    // resolve phase
    emit_jsonl_event("phase_start", "\"phase\":\"resolve\"");
    pipeline.resolve();
    let resolve_errors = pipeline.resolved.as_ref().map_or(0, |r| r.errors.len());
    emit_jsonl_event(
        "phase_complete",
        &format!(
            "\"phase\":\"resolve\",\"errors\":{},\"warnings\":0,\"notes\":0",
            resolve_errors
        ),
    );

    if pipeline.has_resolve_errors() {
        for phase in &["typecheck", "effect", "ownership"] {
            emit_jsonl_event(
                "phase_skipped",
                &format!(
                    "\"phase\":{},\"reason\":\"resolve errors in input\",\"blocking\":[]",
                    json_string(phase)
                ),
            );
        }
        let total = parse_errors + resolve_errors;
        emit_jsonl_event(
            "build_complete",
            &format!(
                "\"success\":false,\"total_errors\":{},\"total_warnings\":0,\"program_effects\":null",
                total
            ),
        );
        return;
    }

    // typecheck phase
    emit_jsonl_event("phase_start", "\"phase\":\"typecheck\"");
    pipeline.typecheck();
    pipeline.lower();
    let type_errors = pipeline.typed.as_ref().map_or(0, |t| t.errors.len());
    emit_jsonl_event(
        "phase_complete",
        &format!(
            "\"phase\":\"typecheck\",\"errors\":{},\"warnings\":0,\"notes\":0",
            type_errors
        ),
    );

    // effect phase
    emit_jsonl_event("phase_start", "\"phase\":\"effect\"");
    pipeline.effectcheck();
    let (effect_errors, effect_notes) = pipeline.effects.as_ref().map_or((0, 0), |e| {
        let errors = e
            .errors
            .iter()
            .filter(|e| e.kind != EffectErrorKind::FfiLintHint)
            .count();
        let notes = e
            .errors
            .iter()
            .filter(|e| e.kind == EffectErrorKind::FfiLintHint)
            .count();
        (errors, notes)
    });
    emit_jsonl_event(
        "phase_complete",
        &format!(
            "\"phase\":\"effect\",\"errors\":{},\"warnings\":0,\"notes\":{}",
            effect_errors, effect_notes
        ),
    );

    // ownership phase
    emit_jsonl_event("phase_start", "\"phase\":\"ownership\"");
    pipeline.ownershipcheck();
    let ownership_errors = pipeline.ownership.as_ref().map_or(0, |o| o.errors.len());
    emit_jsonl_event(
        "phase_complete",
        &format!(
            "\"phase\":\"ownership\",\"errors\":{},\"warnings\":0,\"notes\":0",
            ownership_errors
        ),
    );

    // provider escape phase
    emit_jsonl_event("phase_start", "\"phase\":\"provider_escape\"");
    pipeline.provider_escape_check();
    let escape_errors = pipeline.provider_escape.as_ref().map_or(0, |e| e.len());
    emit_jsonl_event(
        "phase_complete",
        &format!(
            "\"phase\":\"provider_escape\",\"errors\":{},\"warnings\":0,\"notes\":0",
            escape_errors
        ),
    );

    let total = parse_errors
        + resolve_errors
        + type_errors
        + effect_errors
        + ownership_errors
        + escape_errors;
    let effects = program_effects_json(pipeline);
    emit_jsonl_event(
        "build_complete",
        &format!(
            "\"success\":{},\"total_errors\":{},\"total_warnings\":0,\"program_effects\":{}",
            total == 0,
            total,
            effects,
        ),
    );
}

// ── Commands ────────────────────────────────────────────────────

fn format_error_trace_json(frames: &[ErrorTraceFrame], truncated: bool) -> String {
    let entries: Vec<String> = frames
        .iter()
        .map(|f| {
            format!(
                "{{\"file\":{},\"line\":{},\"column\":{}}}",
                json_string(&f.file),
                f.line,
                f.column,
            )
        })
        .collect();
    if truncated {
        format!("{{\"frames\":[{}],\"truncated\":true}}", entries.join(","))
    } else {
        format!("[{}]", entries.join(","))
    }
}

fn cmd_run_example(name: &str, output: OutputMode, sequential: bool) {
    // Try single-file form first, then project-style directory form.
    let single_file = format!("examples/{name}.kara");
    let dir_entry = format!("examples/{name}/src/main.kara");
    let path = if std::path::Path::new(&single_file).exists() {
        single_file
    } else if std::path::Path::new(&dir_entry).exists() {
        dir_entry
    } else {
        eprintln!("error: example '{name}' not found");
        eprintln!("  looked for: {single_file}");
        eprintln!("              {dir_entry}");
        list_available_examples();
        process::exit(1);
    };
    cmd_run(&path, output, sequential);
}

fn list_available_examples() {
    let names = walker::walk_examples(std::path::Path::new("."));
    if names.is_empty() {
        return;
    }
    eprintln!("available examples:");
    for n in &names {
        eprintln!("  {n}");
    }
}

fn cmd_run(filename: &str, output: OutputMode, sequential: bool) {
    let source = read_source(filename);
    let mut pipeline = Pipeline::new(filename, &source);
    pipeline.resolve();

    if pipeline.has_fatal_errors() {
        match output {
            OutputMode::Text => {
                print_text_diagnostics(&pipeline);
                process::exit(1);
            }
            OutputMode::Json => {
                emit_json_output(&pipeline);
                process::exit(1);
            }
            OutputMode::Jsonl => {
                run_pipeline_jsonl(&mut pipeline);
                process::exit(1);
            }
        }
    }

    // Type-check (non-fatal for interpreter)
    pipeline.typecheck();
    pipeline.lower();

    if output == OutputMode::Text {
        // Print type warnings to stderr
        if let Some(ref t) = pipeline.typed {
            for err in &t.errors {
                eprintln!(
                    "warning[typecheck]: {}:{}:{}: {}",
                    filename, err.span.line, err.span.column, err.message
                );
            }
        }
        // Lint: undocumented_unsafe
        for diag in crate::unsafe_lint::check_undocumented_unsafe(&pipeline.parsed.program, &source)
        {
            render_unsafe_lint_diag(&diag, filename);
        }
        // Lint: unsafe_op_in_unsafe_fn (slice 3) — walks every fn body
        // and rejects raw-pointer deref / unsafe-fn calls outside an
        // `unsafe { }` block. Runs post-typecheck because raw-ptr deref
        // detection consults `expr_types` and method-call dispatch reads
        // `method_callee_types`.
        for diag in crate::unsafe_lint::check_unsafe_op_in_unsafe_fn(
            &pipeline.parsed.program,
            pipeline.typed.as_ref(),
        ) {
            render_unsafe_lint_diag(&diag, filename);
        }
        // Lint: must_use (slice 1 — implicit `#[must_use]` for the two
        // language-level types `Result[T, E]` and `Option[T]`). Walks
        // every fn body and warns on discarded values of either type at
        // statement position. Needs typecheck info to recognise the
        // types from `expr_types`.
        for diag in crate::must_use_lint::check_implicit_must_use(
            &pipeline.parsed.program,
            pipeline.typed.as_ref(),
        ) {
            render_must_use_lint_diag(&diag, filename);
        }
        // Lint: missing_must_use (slice 3 — stdlib-hygiene). Warns on
        // baked stdlib `pub fn` returning iterator-shaped or
        // new-value-from-self values that lack `#[must_use]`. Silent
        // on user code by design (the lint walks `stdlib_origin == true`
        // items only). Today end-user compiles see no output from this
        // pass because baked stdlib items aren't spliced into the user
        // program AST; the lint surfaces during karac's own stdlib-
        // hygiene tests (`tests/missing_must_use_lint.rs`) and during
        // any future bundled-stdlib-source compile mode.
        for diag in crate::missing_must_use_lint::check_missing_must_use(&pipeline.parsed.program) {
            render_missing_must_use_lint_diag(&diag, filename);
        }
        // Lint: ffi_float_eq
        for diag in crate::ffi_lint::check_ffi_float_eq(&pipeline.parsed.program) {
            eprintln!(
                "warning[ffi_float_eq]: {}:{}:{}: {}",
                filename, diag.span.line, diag.span.column, diag.message
            );
        }
        // Lint: ambiguous_not_comparison
        for diag in crate::logical_lint::check_ambiguous_not_comparison(&pipeline.parsed.program) {
            eprintln!(
                "{}[{}]: {}:{}:{}: {}",
                if diag.level == crate::logical_lint::LintLevel::Error {
                    "error"
                } else {
                    "warning"
                },
                diag.lint_name,
                filename,
                diag.span.line,
                diag.span.column,
                diag.message
            );
        }
    }

    // Provider-rooted resource escape — a hard error per design.md §
    // Provider-Rooted Resources. Unlike type errors in the interpreter-
    // first path, escape violations break the language's test-isolation
    // and teardown guarantees, so they abort execution rather than
    // downgrade to a warning.
    pipeline.provider_escape_check();
    if let Some(ref esc) = pipeline.provider_escape {
        if !esc.is_empty() {
            match output {
                OutputMode::Text => {
                    for err in esc {
                        eprintln!(
                            "error[provider_escape]: {}:{}:{}: {}",
                            filename,
                            err.closure_span.line,
                            err.closure_span.column,
                            err.message()
                        );
                    }
                }
                OutputMode::Json => emit_json_output(&pipeline),
                OutputMode::Jsonl => {
                    for err in esc {
                        emit_jsonl_event(
                            "diagnostic",
                            &format!(
                                "\"severity\":\"error\",\"phase\":\"provider_escape\",\"code\":\"E0600\",{},\"message\":{}",
                                span_to_json(&err.closure_span, filename),
                                json_string(&err.message()),
                            ),
                        );
                    }
                }
            }
            process::exit(1);
        }
    }

    // Run
    let mut interp = Interpreter::new(&pipeline.parsed.program, pipeline.typed.as_ref().unwrap());
    interp.set_source_filename(filename);
    interp.set_source_text(&source);
    interp.set_dbg_output_mode(match output {
        OutputMode::Json | OutputMode::Jsonl => DbgOutputMode::Json,
        OutputMode::Text => DbgOutputMode::Terminal,
    });
    interp.sequential_mode = sequential;
    interp.run();

    // Emit error return trace if present
    if !interp.error_trace().is_empty() {
        let trace = format_error_trace_json(interp.error_trace(), interp.error_trace_truncated());
        match output {
            OutputMode::Json => {
                println!("{{\"error_return_trace\":{}}}", trace);
            }
            OutputMode::Jsonl => {
                emit_jsonl_event(
                    "error_return_trace",
                    &format!(
                        "\"frames\":{},\"truncated\":{}",
                        trace,
                        interp.error_trace_truncated()
                    ),
                );
            }
            OutputMode::Text => {
                eprintln!("Error return trace:");
                for frame in interp.error_trace() {
                    let file_part = if frame.file.is_empty() {
                        String::new()
                    } else {
                        format!("{}:", frame.file)
                    };
                    eprintln!("  {}{}:{}", file_part, frame.line, frame.column);
                }
                if interp.error_trace_truncated() {
                    eprintln!("  ... (trace truncated, max {} frames)", 64);
                }
            }
        }
    }
}

fn cmd_check(
    filename: &str,
    output: OutputMode,
    profiles: Option<Vec<crate::manifest::CompileProfile>>,
    concurrency_report: bool,
) {
    let source = read_source(filename);

    if let Some(list) = profiles {
        cmd_check_profiles(filename, &source, output, &list);
        return;
    }

    match output {
        OutputMode::Jsonl => {
            let mut pipeline = Pipeline::new(filename, &source);
            run_pipeline_jsonl(&mut pipeline);
            if pipeline.total_errors() > 0 {
                process::exit(1);
            }
        }
        _ => {
            let mut pipeline = Pipeline::new(filename, &source);
            pipeline.run_all_checks();

            // Slice D: concurrency report fires after `run_all_checks` (which
            // already runs `concurrencycheck()`) and before the final OK /
            // error summary so the report sits with the rest of stdout.
            if concurrency_report {
                emit_concurrency_report(&pipeline);
            }

            match output {
                OutputMode::Text => {
                    print_text_diagnostics(&pipeline);
                    let total = pipeline.total_errors();
                    if total > 0 {
                        eprintln!("\n{total} error(s) found.");
                        process::exit(1);
                    } else {
                        eprintln!("All checks passed.");
                    }
                }
                OutputMode::Json => {
                    emit_json_output(&pipeline);
                    if pipeline.total_errors() > 0 {
                        process::exit(1);
                    }
                }
                OutputMode::Jsonl => unreachable!(),
            }
        }
    }
}

/// Slice D helper: render the human-readable concurrency report from the
/// pipeline's already-populated `concurrency` and `effects` fields and
/// emit it to stdout. No-op when either field is None (the analysis didn't
/// run because earlier phases failed); the build/check paths still surface
/// the upstream errors through the normal diagnostic channel.
fn emit_concurrency_report(pipeline: &Pipeline) {
    let (Some(concurrency), Some(effects)) = (&pipeline.concurrency, &pipeline.effects) else {
        return;
    };
    let report = crate::concurrency_report::render_concurrency_report(
        concurrency,
        effects,
        &pipeline.parsed.program,
    );
    print!("{report}");
}

/// Multi-profile typecheck driver. Runs the full pipeline once per named
/// profile and groups diagnostics by profile so a CI matrix can verify
/// "this library compiles cleanly under default + embedded + kernel" from a
/// single invocation. Exits non-zero if any profile fails. Profile only
/// affects the effect-checker today (extern declarations are validated
/// against the profile's forbidden-effect set per `manifest::CompileProfile`),
/// so the parse / resolve / typecheck phases produce identical output across
/// profiles — only the effect phase diverges. Per-profile grouping keeps the
/// output skimmable when one profile fails and the others pass.
fn cmd_check_profiles(
    filename: &str,
    source: &str,
    output: OutputMode,
    profiles: &[crate::manifest::CompileProfile],
) {
    let mut any_failed = false;
    let mut blocks: Vec<String> = Vec::new();
    for (idx, profile) in profiles.iter().enumerate() {
        let mut pipeline = Pipeline::new(filename, source);
        pipeline.profile = *profile;

        match output {
            OutputMode::Text => {
                pipeline.run_all_checks();
                let total = pipeline.total_errors();
                if total > 0 {
                    any_failed = true;
                }
                if idx > 0 {
                    eprintln!();
                }
                eprintln!("── profile: {} ──", profile.as_str());
                print_text_diagnostics(&pipeline);
                if total > 0 {
                    eprintln!("{total} error(s) under '{}' profile.", profile.as_str());
                } else {
                    eprintln!("All checks passed under '{}' profile.", profile.as_str());
                }
            }
            OutputMode::Json => {
                pipeline.run_all_checks();
                let total = pipeline.total_errors();
                if total > 0 {
                    any_failed = true;
                }
                let diags = collect_diagnostics(&pipeline);
                let block = format!(
                    "{{\"profile\":{},\"success\":{},\"total_errors\":{},\"diagnostics\":{}}}",
                    json_string(profile.as_str()),
                    total == 0,
                    total,
                    diags.to_json_array(),
                );
                blocks.push(block);
            }
            OutputMode::Jsonl => {
                emit_jsonl_event(
                    "profile_start",
                    &format!("\"profile\":{}", json_string(profile.as_str())),
                );
                run_pipeline_jsonl(&mut pipeline);
                let total = pipeline.total_errors();
                if total > 0 {
                    any_failed = true;
                }
                emit_jsonl_event(
                    "profile_complete",
                    &format!(
                        "\"profile\":{},\"success\":{},\"total_errors\":{}",
                        json_string(profile.as_str()),
                        total == 0,
                        total,
                    ),
                );
            }
        }
    }

    if let OutputMode::Json = output {
        println!(
            "{{\"profiles\":[{}],\"success\":{}}}",
            blocks.join(","),
            !any_failed,
        );
    }

    if any_failed {
        process::exit(1);
    }
}

fn cmd_build(filename: &str, output: OutputMode, concurrency_report: bool, offline: bool) {
    if offline {
        // v1 surface gate: the `--offline` flag parses and routes, but
        // the resolver wiring that would consult `vendor/` and refuse
        // network access lands in a follow-up slice. Surface the
        // discrepancy so CI scripts pinning to the flag don't think
        // they're already air-gapped.
        eprintln!(
            "note: --offline parsed but not yet wired (vendor/ consultation + network refusal land alongside dep resolution)"
        );
    }
    let _ = offline;
    #[cfg(feature = "llvm")]
    {
        let source = read_source(filename);
        let mut pipeline = Pipeline::new(filename, &source);
        pipeline.resolve();
        pipeline.typecheck();
        pipeline.lower();
        pipeline.effectcheck();
        pipeline.ownershipcheck();
        // Auto-par codegen (slice 2): populate `pipeline.concurrency` so the
        // codegen call below picks up inferred parallel groups via
        // `Codegen::parallel_groups_for_current_fn`. `concurrencycheck` is a
        // no-op when `effectcheck` produced no result (`self.effects.is_none()`),
        // so phase ordering follows effects → ownership → concurrency.
        pipeline.concurrencycheck();

        // Slice D: emit the human-readable concurrency report before the
        // codegen / link stage so it lands on stdout next to the
        // `Built: <exe>` line, regardless of whether codegen later fails.
        if concurrency_report {
            emit_concurrency_report(&pipeline);
        }

        if pipeline.has_fatal_errors() {
            match output {
                OutputMode::Text => {
                    print_text_diagnostics(&pipeline);
                    process::exit(1);
                }
                OutputMode::Json => {
                    emit_json_output(&pipeline);
                    process::exit(1);
                }
                OutputMode::Jsonl => unreachable!(),
            }
        }

        // Derive output executable name from the source filename.
        let exe_name = std::path::Path::new(filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let obj_path = format!("/tmp/karac_{exe_name}.o");
        let exe_path = if cfg!(windows) {
            format!("{exe_name}.exe")
        } else {
            exe_name.to_string()
        };

        if let Err(e) = crate::codegen::compile_to_object_with_options(
            &pipeline.parsed.program,
            &obj_path,
            pipeline.ownership.as_ref(),
            pipeline.concurrency.as_ref(),
            Some(filename),
            Some(&source),
        ) {
            eprintln!("error: codegen failed: {e}");
            process::exit(1);
        }
        match crate::codegen::link_executable(&obj_path, &exe_path) {
            Err(e) => {
                eprintln!("error: link failed: {e}");
                let _ = std::fs::remove_file(&obj_path);
                process::exit(1);
            }
            Ok(()) => {
                let _ = std::fs::remove_file(&obj_path);
                match output {
                    OutputMode::Text => println!("Built: {exe_path}"),
                    OutputMode::Json => println!("{{\"status\":\"ok\",\"output\":\"{exe_path}\"}}"),
                    OutputMode::Jsonl => unreachable!(),
                }
            }
        }
    }
    #[cfg(not(feature = "llvm"))]
    {
        eprintln!("note: karac build requires the llvm feature; falling back to type check");
        cmd_check(filename, output, None, concurrency_report);
    }
}

/// Project-mode build entry point.
///
/// Discovers the project root via `kara.toml` walk-up, loads the manifest,
/// walks `src/` to map each `.kara` file to a module path (CR-24 slice 3),
/// parses every file into its own `Program`, assembles the module graph
/// Render documentation for the current project. v1 MVP — walks the
/// project tree, parses every module, and emits one HTML page per
/// documented item under `dist/doc/`. Items without `///` doc comments
/// are skipped silently. Resolver / typechecker passes are intentionally
/// not run: doc rendering only needs the AST surface, and producing
/// docs against a project that doesn't fully type-check is useful for
/// a programmer trying to understand half-finished code.
fn cmd_doc() {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot read current directory: {e}");
            process::exit(1);
        }
    };

    let (root, _mf) = match manifest::load_from_cwd(&cwd) {
        Ok(ok) => ok,
        Err(e) => {
            emit_manifest_error(&e, OutputMode::Text);
            process::exit(1);
        }
    };

    let walked = match walker::walk_project(&root, WalkerOpts::default()) {
        Ok(w) => w,
        Err(e) => {
            emit_walker_error(&e, OutputMode::Text);
            process::exit(1);
        }
    };

    let built = match module::build_program_tree(&walked) {
        Ok(ok) => ok,
        Err(e) => {
            emit_build_tree_error(&e, OutputMode::Text);
            process::exit(1);
        }
    };

    let BuildTreeOk { tree, parse_errors } = built;
    if !parse_errors.is_empty() {
        // Surface parse errors but keep going — render docs for what
        // parsed cleanly. The user can iterate.
        print_parse_errors_text(&parse_errors);
    }

    // Run effectcheck once on a merged Program containing every
    // non-synthetic module's items so cross-module callee resolution
    // works at the bare-name level the effectchecker indexes by. See
    // `build_doc_effects_table` for the trade-offs.
    let effects = build_doc_effects_table(&tree);

    let output_dir = root.join("dist").join("doc");
    match crate::doc::build_docs(&tree, &output_dir, Some(&effects)) {
        Ok(result) => {
            println!(
                "rendered {} doc page(s) under {}",
                result.written.len().saturating_sub(1), // minus the index
                output_dir.display()
            );
        }
        Err(e) => {
            eprintln!("error[doc]: {e}");
            process::exit(1);
        }
    }
}

/// Build the `(module_path, fn_name) → EffectDisplay` table consumed
/// by the doc renderer.
///
/// Strategy: merge every non-synthetic module's items into a single
/// `Program` and run `effectcheck` once. The effectchecker indexes
/// functions by bare name, and cross-module call sites also resolve
/// to bare names (Kāra's `import` brings a callee into scope under
/// its bare name). A per-module check would treat every cross-module
/// call as effect-empty — `pub fn`s whose inferred set depends on a
/// callee in another module would surface incomplete `with` clauses
/// in the rendered docs.
///
/// Trade-off: when two modules define functions with the same bare
/// name, the merge keeps only one and the doc display is approximate.
/// This is doc-only; the main pipeline (`build`, `check`, `run`)
/// still runs effectcheck per-module via the regular phase wiring.
/// Effectcheck errors raised by the merged pass (e.g. duplicate
/// resource declarations across modules, missing effect declarations)
/// are deliberately ignored here — the doc renderer is best-effort.
fn build_doc_effects_table(tree: &ProgramTree) -> crate::doc::EffectsByItem {
    use crate::ast::Item;
    use crate::doc::{EffectDisplay, EffectsByItem};
    use crate::effectchecker::{DeclaredEffects, EffectSet};

    let mut merged_items = Vec::new();
    for module in &tree.modules {
        if module.is_synthetic {
            continue;
        }
        merged_items.extend(module.items.iter().cloned());
    }
    let merged_program = Program {
        items: merged_items,
        ..Program::default()
    };
    let effects = crate::effectcheck(&merged_program);

    let mut out: EffectsByItem = std::collections::HashMap::new();
    for module in &tree.modules {
        if module.is_synthetic {
            continue;
        }
        for item in &module.items {
            let Item::Function(f) = item else { continue };
            if !f.is_pub {
                continue;
            }

            // Prefer the declared annotation (the user's contract);
            // fall back to the inferred set if no explicit annotation.
            let display = match effects.declared_effects.get(&f.name) {
                Some(DeclaredEffects::Explicit(set)) => effect_set_to_display(set, false),
                Some(DeclaredEffects::Polymorphic) => EffectDisplay {
                    effects: Vec::new(),
                    polymorphic: true,
                },
                Some(DeclaredEffects::PolymorphicWithFixed(set)) => {
                    effect_set_to_display(set, true)
                }
                Some(DeclaredEffects::None) | None => effects
                    .inferred_effects
                    .get(&f.name)
                    .map(|set: &EffectSet| effect_set_to_display(set, false))
                    .unwrap_or_default(),
            };

            if !display.effects.is_empty() || display.polymorphic {
                out.insert((module.path.clone(), f.name.clone()), display);
            }
        }
    }

    out
}

fn effect_set_to_display(
    set: &crate::effectchecker::EffectSet,
    polymorphic: bool,
) -> crate::doc::EffectDisplay {
    let mut effects: Vec<(crate::ast::EffectVerbKind, String)> = set
        .effects
        .iter()
        .map(|t| (t.effect.verb.clone(), t.effect.resource.clone()))
        .collect();
    // Stable order across runs: by verb name, then resource.
    effects.sort_by(|a, b| {
        let an = effect_verb_str(&a.0);
        let bn = effect_verb_str(&b.0);
        an.cmp(bn).then_with(|| a.1.cmp(&b.1))
    });
    crate::doc::EffectDisplay {
        effects,
        polymorphic,
    }
}

/// (slice 4), runs Tarjan's SCC to reject circular module dependencies
/// (`E0223`), and runs cross-module name resolution per module
/// (slice 5, `E0224` / `E0225`). Visibility enforcement and typechecking
/// across modules arrive in slice 6+.
fn cmd_build_project(output: OutputMode, offline: bool) {
    if offline {
        eprintln!(
            "note: --offline parsed but not yet wired (vendor/ consultation + network refusal land alongside dep resolution)"
        );
    }
    let _ = offline;
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot read current directory: {e}");
            process::exit(1);
        }
    };

    let (root, mf) = match manifest::load_from_cwd(&cwd) {
        Ok(ok) => ok,
        Err(e) => {
            emit_manifest_error(&e, output);
            process::exit(1);
        }
    };

    let walk_opts = WalkerOpts::default();
    let walked = match walker::walk_project(&root, walk_opts) {
        Ok(w) => w,
        Err(e) => {
            emit_walker_error(&e, output);
            process::exit(1);
        }
    };

    let built = match module::build_program_tree(&walked) {
        Ok(ok) => ok,
        Err(e) => {
            emit_build_tree_error(&e, output);
            process::exit(1);
        }
    };

    let BuildTreeOk { tree, parse_errors } = built;

    let cycles = module::detect_cycles(&tree);

    // Slice 5: run cross-module name resolution per module. Only attempt
    // resolution when the graph is acyclic and every file parsed cleanly —
    // otherwise we would cascade dozens of spurious E0224/E0225s atop the
    // real failure.
    let resolve_errors: Vec<ModuleResolveErrors> = if parse_errors.is_empty() && cycles.is_empty() {
        resolve_modules(&tree)
    } else {
        Vec::new()
    };

    // Slice 6 (follow-up): run the typechecker per module with the project
    // tree attached so cross-module `E0221` and the CR-18 field-access rule
    // can fire. Skipped when earlier phases reported errors, since a half-
    // resolved program produces unhelpful type cascades.
    let type_errors: Vec<ModuleTypeErrors> =
        if parse_errors.is_empty() && cycles.is_empty() && resolve_errors.is_empty() {
            typecheck_modules(&tree)
        } else {
            Vec::new()
        };

    // Theme 4 (2026-05-10) — multi-file project-mode codegen. Per-module
    // resolve + typecheck above produce per-file diagnostics; once those
    // pass, the codegen path concatenates all module items (in topological
    // order, dropping `import` declarations + the synthetic prelude) into a
    // single super-program and drives it through the existing single-file
    // pipeline (`lower` → `effectcheck` → `ownershipcheck` →
    // `concurrencycheck` → codegen → link). Per-module wiring of the post-
    // typecheck phases would lose cross-module callee-effect visibility
    // (concurrency analysis depends on knowing imported functions' effects);
    // the super-program approach gives correct cross-module analysis at the
    // cost of less granular file-context in late-phase diagnostics. Symbol
    // mangling deferred to v2 — cross-module function-name collisions
    // surface as duplicate-symbol errors at the LLVM linker (clear failure,
    // ungainly diagnostic; structured detection is a follow-up).
    let mut codegen_status: BuildCodegenStatus = BuildCodegenStatus::Skipped;
    if !cfg!(feature = "llvm") {
        // Mirror the single-file `cmd_build` no-llvm fallback (line ~2393).
        codegen_status = BuildCodegenStatus::NoLlvmFeature;
    } else if parse_errors.is_empty()
        && cycles.is_empty()
        && resolve_errors.is_empty()
        && type_errors.is_empty()
    {
        codegen_status = run_multi_file_codegen(&tree, &mf, &root);
    }

    let failed = !parse_errors.is_empty()
        || !cycles.is_empty()
        || !resolve_errors.is_empty()
        || !type_errors.is_empty()
        || matches!(codegen_status, BuildCodegenStatus::Failed { .. });

    match output {
        OutputMode::Text => {
            for w in &mf.warnings {
                eprintln!("warning[manifest]: {}", w.message);
            }
            print_parse_errors_text(&parse_errors);
            print_cycles_text(&cycles, &tree);
            print_resolve_errors_text(&resolve_errors);
            print_type_errors_text(&type_errors);
            println!("project: {}", mf.name);
            println!("edition: {}", mf.edition);
            println!("root:    {}", root.display());
            println!("target:  {}", walk_opts.target.as_suffix());
            println!("entry:   {}", entry_label(walked.entry));
            println!("modules: {}", walked.modules.len());
            for m in &walked.modules {
                let path = if m.path.is_empty() {
                    "<crate root>".to_string()
                } else {
                    m.path.join(".")
                };
                let plat = match m.platform {
                    Some(p) => format!(" [{}]", p.as_suffix()),
                    None => String::new(),
                };
                println!("  {path}{plat}  {}", m.file.display());
            }
            if failed {
                let total = parse_errors.iter().map(|pe| pe.errors.len()).sum::<usize>()
                    + cycles.len()
                    + resolve_errors
                        .iter()
                        .map(|re| re.errors.len())
                        .sum::<usize>()
                    + type_errors.iter().map(|te| te.errors.len()).sum::<usize>()
                    + codegen_status.error_count();
                if let BuildCodegenStatus::Failed { phase, message } = &codegen_status {
                    eprintln!("error[{phase}]: {message}");
                }
                eprintln!("\n{total} error(s) found.");
                process::exit(1);
            }
            match &codegen_status {
                BuildCodegenStatus::Built { exe_path } => {
                    println!("Built: {}", exe_path.display());
                }
                BuildCodegenStatus::NoLlvmFeature => {
                    eprintln!(
                        "note: karac build requires the llvm feature; project type-checked but no executable was produced."
                    );
                }
                BuildCodegenStatus::Skipped | BuildCodegenStatus::Failed { .. } => {}
            }
        }
        OutputMode::Json => {
            let warnings: Vec<String> = mf
                .warnings
                .iter()
                .map(|w| {
                    format!(
                        "{{\"severity\":\"warning\",\"phase\":\"manifest\",\"message\":{}}}",
                        json_string(&w.message),
                    )
                })
                .collect();
            let mut diags = warnings;
            diags.extend(parse_errors_json(&parse_errors));
            diags.extend(cycles_json(&cycles, &tree));
            diags.extend(resolve_errors_json(&resolve_errors));
            diags.extend(type_errors_json(&type_errors));
            if let BuildCodegenStatus::Failed { phase, message } = &codegen_status {
                diags.push(format!(
                    "{{\"severity\":\"error\",\"phase\":{},\"message\":{}}}",
                    json_string(phase),
                    json_string(message),
                ));
            }
            let modules = render_walked_modules_json(&walked);
            let status = if failed { "error" } else { "ok" };
            let output_field = match &codegen_status {
                BuildCodegenStatus::Built { exe_path } => format!(
                    ",\"output\":{}",
                    json_string(&exe_path.display().to_string()),
                ),
                _ => String::new(),
            };
            println!(
                "{{\"status\":{},\"project\":{},\"edition\":{},\"root\":{},\"target\":{},\"entry\":{},\"modules\":[{}],\"diagnostics\":[{}]{}}}",
                json_string(status),
                json_string(&mf.name),
                json_string(&mf.edition),
                json_string(&root.display().to_string()),
                json_string(walk_opts.target.as_suffix()),
                json_string(entry_label(walked.entry)),
                modules,
                diags.join(","),
                output_field,
            );
            if failed {
                process::exit(1);
            }
        }
        OutputMode::Jsonl => {
            emit_jsonl_event(
                "manifest_loaded",
                &format!(
                    "\"project\":{},\"edition\":{},\"root\":{}",
                    json_string(&mf.name),
                    json_string(&mf.edition),
                    json_string(&root.display().to_string()),
                ),
            );
            for w in &mf.warnings {
                emit_jsonl_event(
                    "manifest_warning",
                    &format!("\"message\":{}", json_string(&w.message)),
                );
            }
            let modules = render_walked_modules_json(&walked);
            emit_jsonl_event(
                "modules_discovered",
                &format!(
                    "\"target\":{},\"entry\":{},\"modules\":[{}]",
                    json_string(walk_opts.target.as_suffix()),
                    json_string(entry_label(walked.entry)),
                    modules,
                ),
            );
            for entry in parse_errors_jsonl(&parse_errors) {
                println!("{entry}");
            }
            for entry in cycles_jsonl(&cycles, &tree) {
                println!("{entry}");
            }
            for entry in resolve_errors_jsonl(&resolve_errors) {
                println!("{entry}");
            }
            for entry in type_errors_jsonl(&type_errors) {
                println!("{entry}");
            }
            if let BuildCodegenStatus::Failed { phase, message } = &codegen_status {
                emit_jsonl_event(
                    "codegen_error",
                    &format!(
                        "\"phase\":{},\"message\":{}",
                        json_string(phase),
                        json_string(message),
                    ),
                );
            }
            if let BuildCodegenStatus::Built { exe_path } = &codegen_status {
                emit_jsonl_event(
                    "build_artifact",
                    &format!(
                        "\"output\":{}",
                        json_string(&exe_path.display().to_string())
                    ),
                );
            }
            emit_jsonl_event(
                "build_complete",
                &format!(
                    "\"success\":{},\"total_errors\":{}",
                    !failed,
                    parse_errors.iter().map(|pe| pe.errors.len()).sum::<usize>()
                        + cycles.len()
                        + resolve_errors
                            .iter()
                            .map(|re| re.errors.len())
                            .sum::<usize>()
                        + type_errors.iter().map(|te| te.errors.len()).sum::<usize>()
                        + codegen_status.error_count(),
                ),
            );
            if failed {
                process::exit(1);
            }
        }
    }
}

/// Result of the Theme 4 multi-file codegen pass appended to
/// [`cmd_build_project`]. Each variant maps to a downstream output mode
/// (text "Built: ..." line / JSON `"output"` field / JSONL
/// `build_artifact` event). `Built` and `Failed` are only constructed
/// under `cfg(feature = "llvm")` since the codegen pass itself is gated
/// on the same feature.
#[cfg_attr(not(feature = "llvm"), allow(dead_code))]
#[derive(Debug, Clone)]
enum BuildCodegenStatus {
    /// Earlier per-module phases failed (parse / cycles / resolve /
    /// typecheck), so codegen never ran. Output modes don't emit anything
    /// extra in this case — the per-phase diagnostics carry the failure.
    Skipped,
    /// `karac` was built without the `llvm` feature; project type-checks
    /// but no executable can be produced. Mirrors the single-file
    /// `cmd_build` no-llvm branch.
    NoLlvmFeature,
    /// All phases succeeded; the linked executable is at `exe_path`.
    Built { exe_path: PathBuf },
    /// Late-phase failure (effect / ownership / concurrency / codegen /
    /// link). `phase` names the failing phase for the diagnostic output;
    /// `message` is the rendered error.
    Failed { phase: String, message: String },
}

impl BuildCodegenStatus {
    fn error_count(&self) -> usize {
        match self {
            BuildCodegenStatus::Failed { .. } => 1,
            _ => 0,
        }
    }
}

/// Drive the multi-file codegen path: concatenate all module items into a
/// single super-program (in topological order, dropping `import`
/// declarations and the synthetic prelude), run the post-typecheck
/// pipeline (lower / effect / ownership / concurrency), then codegen +
/// link. Caller has already verified parse / cycles / resolve / typecheck
/// passed; this function returns a structured status the caller renders
/// per output mode.
///
/// **Multi-module diagnostics.** Late-phase diagnostics (effect /
/// ownership / concurrency / codegen / link) for the merged super-
/// program recover file-of-origin context via a `SpanLookupKey →
/// module_index` table built at concat time and consulted by
/// `format_pipeline_errors`. When a span resolves to exactly one
/// module the diagnostic is prefixed with `file:line:col`; when the
/// span is absent (e.g., synthesized post-concat) or ambiguous
/// (collision across modules — rare in practice but possible when
/// two distinct files have identical leading bytes), the formatter
/// falls back to the file-less `line:col` form. Per-file
/// diagnostics for parse / cycles / resolve / typecheck still fire
/// upstream of this call.
#[cfg(feature = "llvm")]
fn run_multi_file_codegen(
    tree: &ProgramTree,
    mf: &crate::manifest::Manifest,
    project_root: &std::path::Path,
) -> BuildCodegenStatus {
    // 1. Topological emission order — dependencies before dependents.
    let order = module::emission_order(tree);

    // 2. Concatenate items. Drop `import` declarations (their effect was
    // resolved upstream by per-module resolve) and skip synthetic
    // modules. Items keep their original spans, which downstream
    // diagnostics use for line:col reporting.
    //
    // While concatenating, build a `ModuleSpanTable`: for each non-
    // synthetic module we register its file path once, then walk every
    // appended item's spans so late-phase diagnostics can recover the
    // file-of-origin via `format_pipeline_errors`.
    let mut super_items: Vec<Item> = Vec::new();
    let mut span_table = crate::span_visitor::ModuleSpanTable::new();
    for &id in &order {
        let m = &tree.modules[id];
        if m.is_synthetic {
            continue;
        }
        let module_idx = span_table.register_module(m.file.clone());
        for item in &m.items {
            if matches!(item, Item::Import(_)) {
                continue;
            }
            span_table.record_item(module_idx, item);
            super_items.push(item.clone());
        }
    }
    let super_program = Program {
        items: super_items,
        ..Program::default()
    };

    // 3. Drive the rest of the pipeline by hand-constructing a Pipeline
    // with the synthetic ParseResult. This mirrors what `Pipeline::new`
    // would do on a single-file source, except we skip the parse step
    // entirely (we have a pre-built Program already).
    let parsed = ParseResult {
        program: super_program,
        errors: Vec::new(),
    };
    let mut pipeline = Pipeline {
        filename: mf.name.clone(),
        parsed,
        resolved: None,
        typed: None,
        effects: None,
        ownership: None,
        concurrency: None,
        provider_escape: None,
        profile: crate::manifest::CompileProfile::Default,
    };
    pipeline.resolve();
    if pipeline.has_resolve_errors() {
        return BuildCodegenStatus::Failed {
            phase: "resolve".to_string(),
            message: format_pipeline_errors(&pipeline, "resolve", Some(&span_table)),
        };
    }
    pipeline.typecheck();
    if pipeline
        .typed
        .as_ref()
        .is_some_and(|t| !t.errors.is_empty())
    {
        return BuildCodegenStatus::Failed {
            phase: "typecheck".to_string(),
            message: format_pipeline_errors(&pipeline, "typecheck", Some(&span_table)),
        };
    }
    pipeline.lower();
    pipeline.effectcheck();
    if pipeline
        .effects
        .as_ref()
        .is_some_and(|e| !e.errors.is_empty())
    {
        return BuildCodegenStatus::Failed {
            phase: "effect".to_string(),
            message: format_pipeline_errors(&pipeline, "effect", Some(&span_table)),
        };
    }
    pipeline.ownershipcheck();
    if pipeline
        .ownership
        .as_ref()
        .is_some_and(|o| !o.errors.is_empty())
    {
        return BuildCodegenStatus::Failed {
            phase: "ownership".to_string(),
            message: format_pipeline_errors(&pipeline, "ownership", Some(&span_table)),
        };
    }
    pipeline.concurrencycheck();
    if pipeline.has_fatal_errors() {
        return BuildCodegenStatus::Failed {
            phase: "checks".to_string(),
            message: format_pipeline_errors(&pipeline, "checks", Some(&span_table)),
        };
    }

    // 4. Codegen — write to a temp object then link to the manifest's
    // `name` field as the binary basename in the project root.
    let exe_path = project_root.join(&mf.name);
    let obj_path = std::env::temp_dir().join(format!(
        "karac_proj_{}_{}.o",
        std::process::id(),
        mf.name.replace(['/', '\\'], "_"),
    ));

    if let Err(e) = crate::codegen::compile_to_object_with_options(
        &pipeline.parsed.program,
        &obj_path.to_string_lossy(),
        pipeline.ownership.as_ref(),
        pipeline.concurrency.as_ref(),
        None,
        None,
    ) {
        let _ = std::fs::remove_file(&obj_path);
        return BuildCodegenStatus::Failed {
            phase: "codegen".to_string(),
            message: format!("codegen failed: {e}"),
        };
    }
    if let Err(e) =
        crate::codegen::link_executable(&obj_path.to_string_lossy(), &exe_path.to_string_lossy())
    {
        let _ = std::fs::remove_file(&obj_path);
        return BuildCodegenStatus::Failed {
            phase: "link".to_string(),
            message: format!("link failed: {e}"),
        };
    }
    let _ = std::fs::remove_file(&obj_path);
    BuildCodegenStatus::Built { exe_path }
}

/// Stub for the no-llvm build — never invoked because the caller gates
/// on `cfg!(feature = "llvm")`. Kept as a parallel signature so the call
/// site doesn't need cfg gating itself.
#[cfg(not(feature = "llvm"))]
fn run_multi_file_codegen(
    _tree: &ProgramTree,
    _mf: &crate::manifest::Manifest,
    _project_root: &std::path::Path,
) -> BuildCodegenStatus {
    BuildCodegenStatus::NoLlvmFeature
}

/// Render a structured error list across the post-typecheck pipeline
/// phases for the multi-file project-mode build path. `table` is the
/// per-module span lookup built at concat time in
/// `run_multi_file_codegen` — when present and the span resolves to
/// exactly one module, the diagnostic line is prefixed with
/// `file:line:col`; otherwise it falls back to bare `line:col` so
/// callers without a table (or with a span absent from the table /
/// shared across modules) still get a useful location.
#[cfg(feature = "llvm")]
fn format_pipeline_errors(
    pipeline: &Pipeline,
    phase: &str,
    table: Option<&crate::span_visitor::ModuleSpanTable>,
) -> String {
    use std::fmt::Write;
    let mut out = format!("multi-file {phase} failed:");
    let prefix = |span: &crate::token::Span| -> String {
        if let Some(t) = table {
            if let Some(p) = t.lookup(span) {
                return format!("{}:", p.display());
            }
        }
        String::new()
    };
    match phase {
        "resolve" => {
            if let Some(r) = &pipeline.resolved {
                for e in &r.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&e.span),
                        e.span.line,
                        e.span.column,
                        e.message,
                    );
                }
            }
        }
        "typecheck" => {
            if let Some(t) = &pipeline.typed {
                for e in &t.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&e.span),
                        e.span.line,
                        e.span.column,
                        e.message,
                    );
                }
            }
        }
        "effect" => {
            if let Some(e) = &pipeline.effects {
                for err in &e.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&err.span),
                        err.span.line,
                        err.span.column,
                        err.message,
                    );
                }
            }
        }
        "ownership" => {
            if let Some(o) = &pipeline.ownership {
                for err in &o.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&err.span),
                        err.span.line,
                        err.span.column,
                        err.message,
                    );
                }
            }
        }
        // The "checks" branch is reached when `has_fatal_errors`
        // returns true after a late-phase pass; today that flag is
        // driven by parse + resolve errors only (concurrency analysis
        // emits structured decisions, not errors), but we surface
        // every accumulated error here so the user gets file-context
        // wherever a span is available rather than the generic
        // "late-phase analysis failed" stub.
        "checks" => {
            if let Some(r) = &pipeline.resolved {
                for e in &r.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&e.span),
                        e.span.line,
                        e.span.column,
                        e.message,
                    );
                }
            }
            if let Some(t) = &pipeline.typed {
                for e in &t.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&e.span),
                        e.span.line,
                        e.span.column,
                        e.message,
                    );
                }
            }
            if let Some(e) = &pipeline.effects {
                for err in &e.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&err.span),
                        err.span.line,
                        err.span.column,
                        err.message,
                    );
                }
            }
            if let Some(o) = &pipeline.ownership {
                for err in &o.errors {
                    let _ = write!(
                        &mut out,
                        "\n  {}{}:{}: {}",
                        prefix(&err.span),
                        err.span.line,
                        err.span.column,
                        err.message,
                    );
                }
            }
        }
        _ => {}
    }
    out
}

fn print_parse_errors_text(parse_errors: &[ModuleParseErrors]) {
    for pe in parse_errors {
        for err in &pe.errors {
            eprintln!(
                "error[parse]: {}:{}:{}: {}",
                pe.file.display(),
                err.span.line,
                err.span.column,
                err.message,
            );
        }
    }
}

/// Resolver errors collected for one specific module, with the source file
/// retained so diagnostics can be printed with their original location.
struct ModuleResolveErrors {
    file: PathBuf,
    errors: Vec<ResolveError>,
}

/// Run the resolver per module with the full `ProgramTree` attached so
/// cross-module imports can be validated. Returns only modules that produced
/// errors — a module with a clean resolve contributes nothing.
fn resolve_modules(tree: &ProgramTree) -> Vec<ModuleResolveErrors> {
    let mut out = Vec::new();
    for (id, m) in tree.modules.iter().enumerate() {
        // Compiler-injected modules (CR-24 slice 8: `std.prelude` placeholder)
        // skip per-module passes — their stub items only exist to surface the
        // module path to cross-module resolution.
        if m.is_synthetic {
            continue;
        }
        // Resolver still takes a `&Program`, so wrap the module's items
        // in a freshly-owned `Program` view. Clone cost is negligible next
        // to the resolver pass itself.
        let program = Program {
            items: m.items.clone(),
            ..Program::default()
        };
        let result = Resolver::new(&program)
            .with_tree(tree, id as ModuleId)
            .resolve();
        if !result.errors.is_empty() {
            out.push(ModuleResolveErrors {
                file: m.file.clone(),
                errors: result.errors,
            });
        }
    }
    out
}

fn resolve_error_code(kind: &ResolveErrorKind) -> &'static str {
    match kind {
        ResolveErrorKind::UnknownModule => "E0224",
        ResolveErrorKind::UnknownItemInModule => "E0225",
        ResolveErrorKind::PrivateItemAccess => "E0222",
        ResolveErrorKind::UndefinedName => "E0100",
        ResolveErrorKind::DuplicateDefinition => "E0101",
        ResolveErrorKind::ReservedIdentifier => "E0102",
        ResolveErrorKind::PrivateAccess => "E0103",
        ResolveErrorKind::UndefinedType => "E0104",
        ResolveErrorKind::UndefinedVariant => "E0105",
        ResolveErrorKind::UndefinedField => "E0106",
        ResolveErrorKind::UndefinedLabel => "E0107",
        ResolveErrorKind::OperatorTraitImplRestricted => "E0108",
        ResolveErrorKind::IntoTraitImplNotAllowed => "E0109",
        ResolveErrorKind::ImplLevelEffectVarNotAllowed => "E0110",
        ResolveErrorKind::ReservedEffectResource => "E0228",
        ResolveErrorKind::CompilerBuiltinReserved => "E0237",
        ResolveErrorKind::ContinueOnBlockLabel => "E0238",
        ResolveErrorKind::NonExhaustiveInvalidTarget => "E0239",
        ResolveErrorKind::TrackCallerInvalidTarget => "E0240",
        ResolveErrorKind::DeprecatedOnImpl => "E0241",
        ResolveErrorKind::DeprecatedOnField => "E0242",
    }
}

fn print_resolve_errors_text(per_module: &[ModuleResolveErrors]) {
    for re in per_module {
        for err in &re.errors {
            let code = resolve_error_code(&err.kind);
            eprintln!(
                "error[{code}]: {}:{}:{}: {}",
                re.file.display(),
                err.span.line,
                err.span.column,
                err.message,
            );
            if let Some(ref s) = err.suggestion {
                eprintln!("  help: did you mean `{s}`?");
            }
        }
    }
}

/// Render `err.replacement` as `,"replacement":{...}` JSON tail (or empty
/// string when no replacement is attached). Mirrors the single-file
/// `print_diagnostics_json` path at the top of this file so IDE quick-fix
/// consumers see the same payload regardless of how `karac check` was
/// invoked. Multi-file-only diagnostics (E0223 / E0225) reach IDEs only
/// through this path.
fn replacement_json_tail(err: &crate::resolver::ResolveError) -> String {
    match err.replacement.as_deref() {
        Some(r) => format!(
            ",\"replacement\":{{\"offset\":{},\"length\":{},\"text\":{}}}",
            r.offset,
            r.length,
            json_string(&r.replacement),
        ),
        None => String::new(),
    }
}

fn resolve_errors_json(per_module: &[ModuleResolveErrors]) -> Vec<String> {
    let mut out = Vec::new();
    for re in per_module {
        let file = re.file.display().to_string();
        for err in &re.errors {
            let code = resolve_error_code(&err.kind);
            let suggestion = match err.suggestion.as_deref() {
                Some(s) => format!(",\"suggestion\":{}", json_string(s)),
                None => String::new(),
            };
            let replacement = replacement_json_tail(err);
            out.push(format!(
                "{{\"severity\":\"error\",\"phase\":\"resolve\",\"code\":{},\"file\":{},\"line\":{},\"column\":{},\"message\":{}{}{}}}",
                json_string(code),
                json_string(&file),
                err.span.line,
                err.span.column,
                json_string(&err.message),
                suggestion,
                replacement,
            ));
        }
    }
    out
}

fn resolve_errors_jsonl(per_module: &[ModuleResolveErrors]) -> Vec<String> {
    let mut out = Vec::new();
    for re in per_module {
        let file = re.file.display().to_string();
        for err in &re.errors {
            let code = resolve_error_code(&err.kind);
            let suggestion = match err.suggestion.as_deref() {
                Some(s) => format!(",\"suggestion\":{}", json_string(s)),
                None => String::new(),
            };
            let replacement = replacement_json_tail(err);
            out.push(format!(
                "{{\"type\":\"resolve_error\",\"code\":{},\"file\":{},\"line\":{},\"column\":{},\"message\":{}{}{}}}",
                json_string(code),
                json_string(&file),
                err.span.line,
                err.span.column,
                json_string(&err.message),
                suggestion,
                replacement,
            ));
        }
    }
    out
}

/// Typechecker errors collected for one specific module.
struct ModuleTypeErrors {
    file: PathBuf,
    errors: Vec<crate::typechecker::TypeError>,
}

/// Run the typechecker per module with the full `ProgramTree` attached so
/// the CR-24 slice-6 cross-module `E0221` + field-access rules can fire.
/// A fresh resolver pass per module provides the `ResolveResult` the
/// typechecker still consumes internally.
fn typecheck_modules(tree: &ProgramTree) -> Vec<ModuleTypeErrors> {
    let mut out = Vec::new();
    for (id, m) in tree.modules.iter().enumerate() {
        // Skip the compiler-injected `std.prelude` placeholder — its stubs
        // would clash with `register_builtin_types` if pushed through the
        // typechecker's normal item-collection.
        if m.is_synthetic {
            continue;
        }
        let program = Program {
            items: m.items.clone(),
            ..Program::default()
        };
        let resolved = Resolver::new(&program)
            .with_tree(tree, id as ModuleId)
            .resolve();
        let result = crate::typechecker::TypeChecker::new(&program, &resolved)
            .with_tree(tree, id as ModuleId)
            .check();
        if !result.errors.is_empty() {
            out.push(ModuleTypeErrors {
                file: m.file.clone(),
                errors: result.errors,
            });
        }
    }
    out
}

fn type_error_code(kind: &crate::typechecker::TypeErrorKind) -> &'static str {
    use crate::typechecker::TypeErrorKind as K;
    match kind {
        K::PrivateTypeInPublicSignature => "E0221",
        K::TypeMismatch => "E0200",
        K::UndefinedField => "E0201",
        K::WrongNumberOfArgs => "E0202",
        K::MissingField => "E0203",
        K::ExtraField => "E0204",
        K::NonExhaustiveMatch => "E0205",
        K::NotCallable => "E0206",
        K::NotAStruct => "E0207",
        K::InvalidBinaryOp => "E0208",
        K::InvalidUnaryOp => "E0209",
        K::InvalidCast => "E0210",
        K::ConditionNotBool => "E0211",
        K::BranchTypeMismatch => "E0212",
        K::ReturnTypeMismatch => "E0213",
        _ => "E0200",
    }
}

fn print_type_errors_text(per_module: &[ModuleTypeErrors]) {
    for te in per_module {
        for err in &te.errors {
            let code = type_error_code(&err.kind);
            eprintln!(
                "error[{code}]: {}:{}:{}: {}",
                te.file.display(),
                err.span.line,
                err.span.column,
                err.message,
            );
        }
    }
}

fn type_errors_json(per_module: &[ModuleTypeErrors]) -> Vec<String> {
    let mut out = Vec::new();
    for te in per_module {
        let file = te.file.display().to_string();
        for err in &te.errors {
            let code = type_error_code(&err.kind);
            out.push(format!(
                "{{\"severity\":\"error\",\"phase\":\"typecheck\",\"code\":{},\"file\":{},\"line\":{},\"column\":{},\"message\":{}}}",
                json_string(code),
                json_string(&file),
                err.span.line,
                err.span.column,
                json_string(&err.message),
            ));
        }
    }
    out
}

fn type_errors_jsonl(per_module: &[ModuleTypeErrors]) -> Vec<String> {
    let mut out = Vec::new();
    for te in per_module {
        let file = te.file.display().to_string();
        for err in &te.errors {
            let code = type_error_code(&err.kind);
            out.push(format!(
                "{{\"type\":\"type_error\",\"code\":{},\"file\":{},\"line\":{},\"column\":{},\"message\":{}}}",
                json_string(code),
                json_string(&file),
                err.span.line,
                err.span.column,
                json_string(&err.message),
            ));
        }
    }
    out
}

fn print_cycles_text(cycles: &[Cycle], tree: &ProgramTree) {
    for c in cycles {
        eprintln!("error[E0223]: circular module dependency");
        eprintln!("  cycle: {}", c.format(tree));
        eprintln!(
            "  suggestion: extract the shared items into a lower-layer module that both ends of the cycle can depend on."
        );
    }
}

fn parse_errors_json(parse_errors: &[ModuleParseErrors]) -> Vec<String> {
    let mut out = Vec::new();
    for pe in parse_errors {
        let file = pe.file.display().to_string();
        for err in &pe.errors {
            out.push(format!(
                "{{\"severity\":\"error\",\"phase\":\"parse\",\"code\":\"E0001\",\"file\":{},\"line\":{},\"column\":{},\"message\":{}}}",
                json_string(&file),
                err.span.line,
                err.span.column,
                json_string(&err.message),
            ));
        }
    }
    out
}

fn cycles_json(cycles: &[Cycle], tree: &ProgramTree) -> Vec<String> {
    cycles
        .iter()
        .map(|c| {
            let paths: Vec<String> = c
                .nodes
                .iter()
                .map(|id| {
                    let p = &tree.modules[*id].path;
                    if p.is_empty() {
                        String::new()
                    } else {
                        p.join(".")
                    }
                })
                .collect();
            let paths_json: Vec<String> = paths.iter().map(|s| json_string(s)).collect();
            let files: Vec<String> = c
                .nodes
                .iter()
                .map(|id| json_string(&tree.modules[*id].file.display().to_string()))
                .collect();
            format!(
                "{{\"severity\":\"error\",\"phase\":\"module_graph\",\"code\":\"E0223\",\"message\":{},\"cycle_paths\":[{}],\"cycle_files\":[{}]}}",
                json_string(&c.format(tree)),
                paths_json.join(","),
                files.join(","),
            )
        })
        .collect()
}

fn parse_errors_jsonl(parse_errors: &[ModuleParseErrors]) -> Vec<String> {
    let mut out = Vec::new();
    for pe in parse_errors {
        let file = pe.file.display().to_string();
        for err in &pe.errors {
            out.push(format!(
                "{{\"type\":\"parse_error\",\"file\":{},\"line\":{},\"column\":{},\"message\":{}}}",
                json_string(&file),
                err.span.line,
                err.span.column,
                json_string(&err.message),
            ));
        }
    }
    out
}

fn cycles_jsonl(cycles: &[Cycle], tree: &ProgramTree) -> Vec<String> {
    cycles
        .iter()
        .map(|c| {
            let paths: Vec<String> = c
                .nodes
                .iter()
                .map(|id| {
                    let p = &tree.modules[*id].path;
                    if p.is_empty() {
                        String::new()
                    } else {
                        p.join(".")
                    }
                })
                .collect();
            let paths_json: Vec<String> = paths.iter().map(|s| json_string(s)).collect();
            format!(
                "{{\"type\":\"module_cycle\",\"code\":\"E0223\",\"message\":{},\"cycle_paths\":[{}]}}",
                json_string(&c.format(tree)),
                paths_json.join(","),
            )
        })
        .collect()
}

fn emit_build_tree_error(e: &BuildTreeError, output: OutputMode) {
    let code = e.code().unwrap_or("module");
    match output {
        OutputMode::Text => {
            eprintln!("error[{code}]: {e}");
        }
        OutputMode::Json => {
            println!(
                "{{\"status\":\"error\",\"diagnostics\":[{{\"severity\":\"error\",\"phase\":\"module_graph\",\"code\":{},\"message\":{}}}]}}",
                json_string(code),
                json_string(&e.to_string()),
            );
        }
        OutputMode::Jsonl => {
            emit_jsonl_event(
                "build_tree_error",
                &format!(
                    "\"code\":{},\"message\":{}",
                    json_string(code),
                    json_string(&e.to_string()),
                ),
            );
        }
    }
}

fn entry_label(entry: EntryKind) -> &'static str {
    match entry {
        EntryKind::Bin => "bin",
        EntryKind::Lib => "lib",
        EntryKind::None => "none",
    }
}

fn render_walked_modules_json(walked: &WalkResult) -> String {
    walked
        .modules
        .iter()
        .map(|m| {
            let path = if m.path.is_empty() {
                String::new()
            } else {
                m.path.join(".")
            };
            let role = match m.role {
                walker::ModuleRole::Ordinary => "ordinary",
                walker::ModuleRole::Entry => "entry",
                walker::ModuleRole::Test => "test",
            };
            let platform = match m.platform {
                Some(p) => json_string(p.as_suffix()),
                None => "null".to_string(),
            };
            format!(
                "{{\"path\":{},\"role\":{},\"platform\":{},\"file\":{}}}",
                json_string(&path),
                json_string(role),
                platform,
                json_string(&m.file.display().to_string()),
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn emit_manifest_error(e: &manifest::ManifestError, output: OutputMode) {
    let code = e.code().unwrap_or("manifest");
    match output {
        OutputMode::Text => {
            eprintln!("error[{code}]: {e}");
        }
        OutputMode::Json => {
            println!(
                "{{\"status\":\"error\",\"diagnostics\":[{{\"severity\":\"error\",\"phase\":\"manifest\",\"code\":{},\"message\":{}}}]}}",
                json_string(code),
                json_string(&e.to_string()),
            );
        }
        OutputMode::Jsonl => {
            emit_jsonl_event(
                "manifest_error",
                &format!(
                    "\"code\":{},\"message\":{}",
                    json_string(code),
                    json_string(&e.to_string()),
                ),
            );
        }
    }
}

fn emit_walker_error(e: &walker::WalkerError, output: OutputMode) {
    let code = e.code().unwrap_or("walker");
    match output {
        OutputMode::Text => {
            eprintln!("error[{code}]: {e}");
        }
        OutputMode::Json => {
            println!(
                "{{\"status\":\"error\",\"diagnostics\":[{{\"severity\":\"error\",\"phase\":\"walker\",\"code\":{},\"message\":{}}}]}}",
                json_string(code),
                json_string(&e.to_string()),
            );
        }
        OutputMode::Jsonl => {
            emit_jsonl_event(
                "walker_error",
                &format!(
                    "\"code\":{},\"message\":{}",
                    json_string(code),
                    json_string(&e.to_string()),
                ),
            );
        }
    }
}

fn cmd_query(kind: QueryKind, filename: &str, function: &str) {
    let source = read_source(filename);
    let mut pipeline = Pipeline::new(filename, &source);
    pipeline.resolve();

    if pipeline.has_fatal_errors() {
        print_text_diagnostics(&pipeline);
        process::exit(1);
    }

    match kind {
        QueryKind::Effects => {
            pipeline.effectcheck();
            query_effects(&pipeline, function);
        }
        QueryKind::Ownership => {
            pipeline.typecheck();
            pipeline.lower();
            pipeline.ownershipcheck();
            query_ownership(&pipeline, function);
        }
        QueryKind::Concurrency => {
            pipeline.effectcheck();
            pipeline.concurrencycheck();
            query_concurrency(&pipeline, function);
        }
        QueryKind::CostSummary => {
            // cost-summary draws from the ownership pass for `rc_ops` and
            // walks the AST directly for `arc_provider_wraps` and
            // `borrow_flag_fields`. It needs typecheck + lower (so the
            // ownership pass sees the same AST every other phase does).
            pipeline.typecheck();
            pipeline.lower();
            pipeline.ownershipcheck();
            query_cost_summary(&pipeline);
        }
    }
}

fn query_cost_summary(pipeline: &Pipeline) {
    let Some(ownership) = pipeline.ownership.as_ref() else {
        eprintln!("error: ownership pass did not run (earlier phase failed)");
        process::exit(1);
    };
    let summary =
        crate::cost_summary::build(&pipeline.filename, &pipeline.parsed.program, ownership);
    println!("{}", render_cost_summary_json(&summary, &pipeline.filename));
}

fn render_cost_summary_json(s: &crate::cost_summary::CostSummary, filename: &str) -> String {
    let totals = format!(
        "{{\"rc_ops\":{{\"count\":{},\"rc\":{},\"arc\":{}}},\"arc_provider_wraps\":{},\"borrow_flag_fields\":{},\"partition_guard_sites\":{},\"auto_clone_insertions\":{}}}",
        s.totals.rc_ops.count,
        s.totals.rc_ops.rc,
        s.totals.rc_ops.arc,
        s.totals.arc_provider_wraps,
        s.totals.borrow_flag_fields,
        s.totals.partition_guard_sites,
        s.totals.auto_clone_insertions,
    );
    let by_function: Vec<String> = s
        .by_function
        .iter()
        .map(|row| {
            let derivation: Vec<String> = row
                .derivation
                .iter()
                .map(|d| {
                    let site = span_to_json(&d.site, filename);
                    format!(
                        "{{\"reason\":{},\"site\":{{{}}}}}",
                        json_string(&d.reason),
                        site,
                    )
                })
                .collect();
            format!(
                "{{\"function\":{},\"rc_ops\":{},\"arc_provider_wraps\":{},\"derivation\":[{}]}}",
                json_string(&row.function),
                row.rc_ops,
                row.arc_provider_wraps,
                derivation.join(","),
            )
        })
        .collect();
    let perf_notes: Vec<String> = s
        .perf_notes
        .iter()
        .map(|n| {
            let site = span_to_json(&n.site, filename);
            format!(
                "{{\"code\":{},\"message\":{},\"site\":{{{}}}}}",
                json_string(&n.code),
                json_string(&n.message),
                site,
            )
        })
        .collect();
    format!(
        "{{\"scope\":{},\"totals\":{},\"by_function\":[{}],\"perf_notes\":[{}]}}",
        json_string(&s.scope),
        totals,
        by_function.join(","),
        perf_notes.join(","),
    )
}

fn query_effects(pipeline: &Pipeline, function: &str) {
    let effects = pipeline.effects.as_ref().unwrap();

    let inferred = effects.inferred_effects.get(function);
    let declared = effects.declared_effects.get(function);

    if inferred.is_none() && declared.is_none() {
        eprintln!("error: function '{function}' not found");
        process::exit(1);
    }

    let mut inferred_list: Vec<String> = Vec::new();
    if let Some(set) = inferred {
        for te in &set.effects {
            inferred_list.push(format!(
                "{{\"verb\":{},\"resource\":{}}}",
                json_string(effect_verb_str(&te.effect.verb)),
                json_string(&te.effect.resource),
            ));
        }
    }

    let declared_str = match declared {
        Some(DeclaredEffects::Explicit(set)) => {
            let mut list: Vec<String> = Vec::new();
            for te in &set.effects {
                list.push(format!(
                    "{{\"verb\":{},\"resource\":{}}}",
                    json_string(effect_verb_str(&te.effect.verb)),
                    json_string(&te.effect.resource),
                ));
            }
            format!("[{}]", list.join(","))
        }
        Some(DeclaredEffects::Polymorphic) => "\"polymorphic\"".to_string(),
        Some(DeclaredEffects::PolymorphicWithFixed(set)) => {
            let mut list: Vec<String> = Vec::new();
            for te in &set.effects {
                list.push(format!(
                    "{{\"verb\":{},\"resource\":{}}}",
                    json_string(effect_verb_str(&te.effect.verb)),
                    json_string(&te.effect.resource),
                ));
            }
            format!("{{\"polymorphic\":true,\"fixed\":[{}]}}", list.join(","))
        }
        Some(DeclaredEffects::None) | None => "null".to_string(),
    };

    println!(
        "{{\"function\":{},\"inferred_effects\":[{}],\"declared_effects\":{}}}",
        json_string(function),
        inferred_list.join(","),
        declared_str,
    );
}

fn query_ownership(pipeline: &Pipeline, function: &str) {
    let ownership = pipeline.ownership.as_ref().unwrap();

    match ownership.param_modes.get(function) {
        Some(params) => {
            let param_entries: Vec<String> = params
                .iter()
                .map(|(name, mode)| {
                    let repr = ownership
                        .representations
                        .get(&format!("{}.{}", function, name))
                        .cloned()
                        .unwrap_or_else(|| match mode {
                            crate::ownership::OwnershipMode::Own => "owned (stack)".to_string(),
                            _ => "ref (borrow)".to_string(),
                        });
                    format!(
                        "{{\"name\":{},\"mode\":{},\"representation\":{}}}",
                        json_string(name),
                        json_string(ownership_mode_str(mode)),
                        json_string(&repr),
                    )
                })
                .collect();
            let rc_entries: Vec<String> = ownership
                .rc_values
                .get(function)
                .map(|m| {
                    let mut v: Vec<&crate::ownership::RcEntry> = m.values().collect();
                    v.sort_by(|a, b| a.binding.cmp(&b.binding));
                    v.into_iter()
                        .map(|e| {
                            let arc = ownership
                                .arc_values
                                .get(function)
                                .is_some_and(|s| s.contains(&e.binding));
                            let kind = if arc { "Arc" } else { "Rc" };
                            format!(
                                "{{\"binding\":{},\"kind\":{},\"trigger\":{},\"consume_line\":{},\"other_use_line\":{}}}",
                                json_string(&e.binding),
                                json_string(kind),
                                json_string(rc_trigger_str(&e.trigger)),
                                e.consume_span.line,
                                e.other_use_span.line,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Round 12.25: closures created inside `function` are
            // surfaced as a `"closures"` array. Each entry carries
            // the closure expression's source location plus the
            // round-12.23 inferred parameter modes and round-12.24
            // captures. Sorted by (line, column) for deterministic
            // output.
            let mut closures_to_emit: Vec<(&crate::resolver::SpanKey, &crate::token::Span)> =
                ownership
                    .closure_function
                    .iter()
                    .filter(|(_, fn_key)| fn_key.as_str() == function)
                    .filter_map(|(k, _)| ownership.closure_spans.get(k).map(|sp| (k, sp)))
                    .collect();
            closures_to_emit.sort_by_key(|(_, sp)| (sp.line, sp.column));
            let closure_entries: Vec<String> = closures_to_emit
                .iter()
                .map(|(key, span)| {
                    let params_json: Vec<String> = ownership
                        .closure_param_modes
                        .get(key)
                        .map(|ms| {
                            ms.iter()
                                .map(|(name, mode)| {
                                    format!(
                                        "{{\"name\":{},\"mode\":{}}}",
                                        json_string(name),
                                        json_string(ownership_mode_str(mode)),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let captures_json: Vec<String> = ownership
                        .closure_captures
                        .get(key)
                        .map(|cs| {
                            cs.iter()
                                .map(|(name, mode)| {
                                    format!(
                                        "{{\"name\":{},\"mode\":{}}}",
                                        json_string(name),
                                        json_string(ownership_mode_str(mode)),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    format!(
                        "{{\"line\":{},\"column\":{},\"parameters\":[{}],\"captures\":[{}]}}",
                        span.line,
                        span.column,
                        params_json.join(","),
                        captures_json.join(","),
                    )
                })
                .collect();
            println!(
                "{{\"function\":{},\"parameters\":[{}],\"rc_values\":[{}],\"closures\":[{}]}}",
                json_string(function),
                param_entries.join(","),
                rc_entries.join(","),
                closure_entries.join(","),
            );
        }
        None => {
            eprintln!("error: function '{function}' not found");
            process::exit(1);
        }
    }
}

fn rc_trigger_str(t: &crate::ownership::RcTrigger) -> &'static str {
    match t {
        crate::ownership::RcTrigger::DirectReuseAfterConsume => "direct_reuse_after_consume",
        crate::ownership::RcTrigger::ClosureCaptureWithOuterUse => "closure_capture_with_outer_use",
        crate::ownership::RcTrigger::ContainerStoreWithSubsequentUse => {
            "container_store_with_subsequent_use"
        }
    }
}

fn query_concurrency(pipeline: &Pipeline, function: &str) {
    let analysis = pipeline.concurrency.as_ref().unwrap();

    match analysis.function_decisions.get(function) {
        Some(fc) => {
            let group_entries: Vec<String> = fc
                .parallel_groups
                .iter()
                .map(|g| {
                    let indices: Vec<String> =
                        g.statement_indices.iter().map(|i| i.to_string()).collect();
                    format!(
                        "{{\"statements\":[{}],\"reason\":{}}}",
                        indices.join(","),
                        json_string(&g.reason),
                    )
                })
                .collect();
            println!(
                "{{\"function\":{},\"total_statements\":{},\"parallel_groups\":[{}]}}",
                json_string(function),
                fc.total_statements,
                group_entries.join(","),
            );
        }
        None => {
            eprintln!("error: function '{function}' not found");
            process::exit(1);
        }
    }
}

fn cmd_fmt(filename: &str) {
    let source = read_source(filename);
    let parsed = crate::parse(&source);
    if !parsed.errors.is_empty() {
        for err in &parsed.errors {
            eprintln!(
                "error[parse]: {}:{}:{}: {}",
                filename, err.span.line, err.span.column, err.message
            );
        }
        process::exit(1);
    }
    let formatted = crate::formatter::format_program(&parsed.program);
    print!("{formatted}");
}

/// Apply machine-applicable suggestions back into the source file.
///
/// Runs the full single-file pipeline (resolve → typecheck → lower →
/// effectcheck → ownership → ...), then collects every diagnostic that
/// carries a `replacement: Some(_)` payload across all phases that have
/// gained machine-applicable metadata. Edits are sorted in reverse
/// byte-offset order (so earlier edits don't invalidate later offsets)
/// and the source file is overwritten. With `dry_run = true`, prints the
/// would-be rewrites to stdout without touching disk.
///
/// Phases that contribute fixes today:
/// - Resolver: E0223 (UnknownModule, round 12.29), E0225
///   (UnknownItemInModule, round 12.28), E0228 (UndefinedName) and E0229
///   (UndefinedType) — both pre-12-era. All four are `did you mean`
///   corrections; the suggestion is a concrete identifier and the error
///   span is the misspelled token.
/// - Ownership: N0507 (UnusedMutCaptureNote, round 12.31) — closure
///   prefix `mut ref` → `ref`. Note (not error), so it does not block
///   compilation; `karac fix` applies it opportunistically.
///
/// Other diagnostic kinds carry descriptive (multi-step) suggestions
/// that are not mechanically applicable; they remain visible through
/// `karac check` and must be acted on by hand.
fn cmd_fix(filename: &str, dry_run: bool) {
    let source = read_source(filename);
    let mut pipeline = Pipeline::new(filename, &source);
    if pipeline.has_parse_errors() {
        for err in &pipeline.parsed.errors {
            eprintln!(
                "error[parse]: {}:{}:{}: {}",
                filename, err.span.line, err.span.column, err.message
            );
        }
        process::exit(1);
    }
    pipeline.run_all_checks();

    let mut edits: Vec<crate::resolver::TextEdit> = Vec::new();
    if let Some(ref r) = pipeline.resolved {
        edits.extend(
            r.errors
                .iter()
                .filter_map(|e| e.replacement.as_deref().cloned()),
        );
    }
    if let Some(ref o) = pipeline.ownership {
        edits.extend(
            o.errors
                .iter()
                .filter_map(|e| e.replacement.as_deref().cloned()),
        );
        edits.extend(
            o.notes
                .iter()
                .filter_map(|e| e.replacement.as_deref().cloned()),
        );
    }

    if edits.is_empty() {
        println!("(no fixable diagnostics in {filename})");
        return;
    }

    // Drop overlapping edits (e.g. the same token reported by multiple
    // sources). Sort by offset descending so that applying them in order
    // does not invalidate the offsets of later edits.
    edits.sort_by_key(|e| std::cmp::Reverse(e.offset));
    let mut deduped: Vec<crate::resolver::TextEdit> = Vec::with_capacity(edits.len());
    let mut last_start = usize::MAX;
    for edit in edits {
        let end = edit.offset.saturating_add(edit.length);
        if end > last_start {
            // Overlaps a later (higher-offset) edit already in the buffer
            // — skip silently. This is a defense-in-depth measure; the
            // resolver shouldn't normally emit overlapping replacements.
            continue;
        }
        last_start = edit.offset;
        deduped.push(edit);
    }

    if dry_run {
        println!("would apply {} fix(es) to {filename}:", deduped.len());
        for edit in deduped.iter().rev() {
            // Render in source order for human readability.
            let original = source
                .get(edit.offset..edit.offset.saturating_add(edit.length))
                .unwrap_or("<?>");
            let (line, col) = crate::byte_offset_to_line_col(&source, edit.offset);
            println!(
                "  {filename}:{line}:{col}: `{}` → `{}`",
                original, edit.replacement
            );
        }
        return;
    }

    let mut rewritten = source.clone();
    for edit in &deduped {
        let end = edit.offset.saturating_add(edit.length);
        if end > rewritten.len() {
            // Source shrank between read and apply — bail rather than
            // produce an out-of-bounds slice.
            eprintln!(
                "error: fix would write past end of file ({} > {}) — aborting without modifying {filename}",
                end,
                rewritten.len()
            );
            process::exit(1);
        }
        rewritten.replace_range(edit.offset..end, &edit.replacement);
    }
    if let Err(e) = std::fs::write(filename, &rewritten) {
        eprintln!("error: failed to write {filename}: {e}");
        process::exit(1);
    }
    println!("applied {} fix(es) to {filename}", deduped.len());
}

// `byte_offset_to_line_col` was promoted to `crate::byte_offset_to_line_col`
// in `src/lib.rs` so codegen's debugger-contract metadata emission can reuse
// it. The cli still calls it from `apply_fixes` below; the rename is a single
// crate-path tweak with no behavior change.

// ── Tests ────────────────────────────────────────────────────────

/// Emit one JSONL test-runner event on stdout. Schema documented in
/// `docs/design.md § Testing › Test runner output format`. The discriminator
/// key is `"type"`, matching the build pipeline's [`emit_jsonl_event`] —
/// JSONL clients consume one shape across all `karac` outputs.
fn emit_test_event(event: &str, fields: &str) {
    if fields.is_empty() {
        println!("{{\"type\":{}}}", json_string(event));
    } else {
        println!("{{\"type\":{},{}}}", json_string(event), fields);
    }
}

/// Render a module path for the qualified test ID, e.g.
/// `db.connection::test_reconnect`. The crate-root module renders as
/// `<root>` so users can distinguish a test in the entry file from any
/// other.
fn module_label(path: &[String]) -> String {
    if path.is_empty() {
        "<root>".to_string()
    } else {
        path.join(".")
    }
}

#[derive(Debug, Clone)]
struct DiscoveredTest {
    module_id: usize,
    fn_name: String,
    qualified: String,
    /// Fully-qualified resource paths (e.g. `"db.UserDB"`) the test
    /// declares via `#[test(requires = [...])]`. Empty when the test has
    /// no `requires` clause; the runner gates execution on the probe
    /// result for each entry.
    requires: Vec<String>,
    /// `#[with_provider(resource_path, constructor_expr)]` fixtures on
    /// the test, preserved in source order (outer-to-inner). The runner
    /// evaluates each constructor before the test body and pushes a
    /// matching provider frame so resource-method calls inside the test
    /// resolve against the fixture. See design.md § Testing.
    with_providers: Vec<WithProviderFixture>,
}

#[derive(Debug, Clone)]
struct WithProviderFixture {
    /// Fully-qualified resource path (e.g. `"Clock"` or `"db.UserDB"`).
    resource_path: String,
    /// Constructor expression — evaluated at test setup to produce the
    /// provider value bound into the frame. Arbitrary expression; a
    /// `panic` / runtime error / control-flow exit during evaluation
    /// produces `provider_construction_failed`.
    constructor: crate::ast::Expr,
}

fn discover_tests(tree: &ProgramTree) -> Vec<DiscoveredTest> {
    let mut out = Vec::new();
    for (mod_id, module) in tree.modules.iter().enumerate() {
        if module.is_synthetic {
            continue;
        }
        let Some(test_start) = module.test_items_start else {
            continue;
        };
        let label = module_label(&module.path);
        for item in &module.items[test_start..] {
            if let Item::Function(f) = item {
                if f.name.starts_with("test_") {
                    out.push(DiscoveredTest {
                        module_id: mod_id,
                        fn_name: f.name.clone(),
                        qualified: format!("{}::{}", label, f.name),
                        requires: extract_requires(&f.attributes),
                        with_providers: extract_with_providers(&f.attributes),
                    });
                }
            }
        }
    }
    out
}

/// Pull resource paths out of a `#[test(requires = [a.b, c.d])]` attribute.
/// Other `#[test(...)]` arg shapes are tolerated and ignored, so future
/// slices can add new keys (e.g. `cases = N`) without breaking earlier
/// runners. Non-path expressions in the array are silently dropped — the
/// parser will already have errored if the attribute body is malformed
/// (the typechecker leaves attribute values alone, so what reaches us is
/// well-formed but possibly not a path).
fn extract_requires(attributes: &[crate::ast::Attribute]) -> Vec<String> {
    let mut out = Vec::new();
    for attr in attributes {
        if attr.name != "test" {
            continue;
        }
        for arg in &attr.args {
            if arg.name.as_deref() != Some("requires") {
                continue;
            }
            let Some(value) = arg.value.as_ref() else {
                continue;
            };
            if let crate::ast::ExprKind::ArrayLiteral(elems) = &value.kind {
                for elem in elems {
                    if let Some(path) = expr_to_dotted_path(elem) {
                        out.push(path);
                    }
                }
            }
        }
    }
    out
}

/// Pull `#[with_provider(resource_path, constructor_expr)]` fixtures out
/// of a function's attribute list. Multiple attributes are preserved in
/// source order (outer-to-inner, matching design.md's stacking rule).
/// Attributes with fewer than two positional args are silently dropped —
/// the parser will already have reported a shape error if the attribute
/// body is malformed.
fn extract_with_providers(attributes: &[crate::ast::Attribute]) -> Vec<WithProviderFixture> {
    let mut out = Vec::new();
    for attr in attributes {
        if attr.name != "with_provider" {
            continue;
        }
        if attr.args.len() < 2 {
            continue;
        }
        // Expect two positional args (name is None); tolerate named-
        // attribute shape by pulling values only when present.
        let Some(resource_expr) = attr.args[0].value.as_ref() else {
            continue;
        };
        let Some(constructor_expr) = attr.args[1].value.as_ref() else {
            continue;
        };
        let Some(resource_path) = expr_to_dotted_path(resource_expr) else {
            continue;
        };
        out.push(WithProviderFixture {
            resource_path,
            constructor: constructor_expr.clone(),
        });
    }
    out
}

/// Reconstruct a dotted-path string from a parsed expression. The parser
/// breaks `db.UserDB` into `FieldAccess(Path(["db"]), "UserDB")` (and
/// deeper chains nest the same way), so walking the AST left-to-right
/// produces the original surface text. Returns `None` for anything
/// that is not a pure dotted identifier chain — such elements simply do
/// not contribute a resource entry.
fn expr_to_dotted_path(expr: &crate::ast::Expr) -> Option<String> {
    use crate::ast::ExprKind;
    match &expr.kind {
        ExprKind::Identifier(name) => Some(name.clone()),
        ExprKind::Path { segments, .. } => {
            if segments.is_empty() {
                None
            } else {
                Some(segments.join("."))
            }
        }
        ExprKind::FieldAccess { object, field } => {
            let prefix = expr_to_dotted_path(object)?;
            Some(format!("{prefix}.{field}"))
        }
        _ => None,
    }
}

/// True iff the resource is reachable. Order of precedence matches
/// `docs/design.md § Testing › Resource availability probing`:
///   1. `[test.resources]` shell command — present iff the manifest
///      lists one for this resource path; available iff exit 0.
///   2. Env var `KARA_RESOURCE_<UPPER_DOT_SLASH_>` (dots → underscores)
///      — available iff set and non-empty.
fn probe_resource(resource: &str, overrides: &std::collections::BTreeMap<String, String>) -> bool {
    if let Some(cmd) = overrides.get(resource) {
        return run_health_check(cmd);
    }
    let env_var = resource_env_var(resource);
    matches!(std::env::var(&env_var), Ok(v) if !v.is_empty())
}

/// Translate a dotted resource path into the env-var probe name. Matches
/// the design (`KARA_RESOURCE_DB_USERDB` from `db.UserDB`): the prefix is
/// fixed so the namespace is reserved, dots become underscores so the
/// shell can set the variable without quoting, and the result is upper-
/// cased so case-insensitive shells (Windows `cmd`) still hit it.
fn resource_env_var(resource: &str) -> String {
    format!(
        "KARA_RESOURCE_{}",
        resource.replace('.', "_").to_uppercase()
    )
}

/// Run a shell health-check command and report whether it succeeded.
/// Uses `sh -c` so users can write the command exactly as they would
/// in a terminal (pipes, env-var interpolation, quoting). Stdout and
/// stderr are captured (not forwarded) so a noisy probe does not
/// pollute the JSONL stream — the only signal we care about is the
/// exit code.
fn run_health_check(cmd: &str) -> bool {
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

fn cmd_test(filter: Option<String>, all: bool) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot read current directory: {e}");
            process::exit(1);
        }
    };

    let (root, mf) = match manifest::load_from_cwd(&cwd) {
        Ok(ok) => ok,
        Err(e) => {
            // Surface manifest errors as a JSONL diagnostic event so consumers
            // can recognize and act on them; then exit non-zero before any
            // run_start/summary so the schema stays clean (no half-runs).
            emit_test_event(
                "manifest_error",
                &format!("\"message\":{}", json_string(&e.to_string())),
            );
            process::exit(1);
        }
    };

    let walk_opts = WalkerOpts {
        include_tests: true,
        ..WalkerOpts::default()
    };
    let walked = match walker::walk_project(&root, walk_opts) {
        Ok(w) => w,
        Err(e) => {
            emit_test_event(
                "walker_error",
                &format!("\"message\":{}", json_string(&e.to_string())),
            );
            process::exit(1);
        }
    };

    let built = match module::build_program_tree_with(
        &walked,
        BuildTreeOpts {
            merge_test_companions: true,
        },
    ) {
        Ok(ok) => ok,
        Err(e) => {
            emit_test_event(
                "build_tree_error",
                &format!("\"message\":{}", json_string(&e.to_string())),
            );
            process::exit(1);
        }
    };

    let BuildTreeOk { tree, parse_errors } = built;
    let cycles = module::detect_cycles(&tree);

    let resolve_errors: Vec<ModuleResolveErrors> = if parse_errors.is_empty() && cycles.is_empty() {
        resolve_modules(&tree)
    } else {
        Vec::new()
    };

    let type_errors: Vec<ModuleTypeErrors> =
        if parse_errors.is_empty() && cycles.is_empty() && resolve_errors.is_empty() {
            typecheck_modules(&tree)
        } else {
            Vec::new()
        };

    let compile_failed = !parse_errors.is_empty()
        || !cycles.is_empty()
        || !resolve_errors.is_empty()
        || !type_errors.is_empty();

    if compile_failed {
        for entry in parse_errors_jsonl(&parse_errors) {
            println!("{entry}");
        }
        for entry in cycles_jsonl(&cycles, &tree) {
            println!("{entry}");
        }
        for entry in resolve_errors_jsonl(&resolve_errors) {
            println!("{entry}");
        }
        for entry in type_errors_jsonl(&type_errors) {
            println!("{entry}");
        }
        process::exit(1);
    }

    // Discover tests, apply filter, sort by (module_id, fn_name) so order is
    // stable across runs — declaration order within a module, modules in
    // walk order. LLM consumers diffing two test runs depend on this.
    let mut tests = discover_tests(&tree);
    if let Some(needle) = &filter {
        tests.retain(|t| t.qualified.contains(needle.as_str()));
    }
    tests.sort_by(|a, b| {
        a.module_id
            .cmp(&b.module_id)
            .then_with(|| a.fn_name.cmp(&b.fn_name))
    });

    let run_started = std::time::Instant::now();
    emit_test_event("run_start", &format!("\"total_tests\":{}", tests.len()));

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut current_module: Option<usize> = None;
    // Per-module state: built lazily on first test in each module.
    let mut current_program: Option<Program> = None;
    let mut current_typed: Option<TypeCheckResult> = None;

    for t in &tests {
        // `#[test(requires = [X])]` and `#[with_provider(X, ...)]` for the
        // *same* resource are contradictory: one gates on an external
        // service, the other supplies a fake. Per design.md § Testing,
        // reject at discovery time with a structured `test_fail` carrying
        // `reason = "requires_and_with_provider_conflict"`. Must precede
        // the missing-requires probe — a test shape error always beats a
        // resource-availability outcome, regardless of `--all`.
        let conflicts = conflict_resources(&t.requires, &t.with_providers);
        if !conflicts.is_empty() {
            failed += 1;
            emit_test_event("test_fail", &test_fail_conflict_fields(t, &conflicts));
            continue;
        }

        // Probe `requires` next — a skipped test must not pay the
        // per-module compile cost and must not load the interpreter.
        // Both halves of the contract (silent skip by default, hard
        // failure under `--all`) need the same `missing` list, so we
        // compute it once and branch.
        let missing = missing_resources(&t.requires, &mf.test_resources);
        if !missing.is_empty() {
            if all {
                failed += 1;
                emit_test_event(
                    "test_fail",
                    &test_fail_unsatisfied_requires_fields(t, &missing),
                );
            } else {
                skipped += 1;
                emit_test_event(
                    "test_skip",
                    &test_skip_unsatisfied_requires_fields(t, &missing),
                );
            }
            continue;
        }

        // Lazily prepare per-module Program + typecheck result so we don't
        // re-parse / re-resolve / re-typecheck for every test in the same
        // module. Tests are sorted by `module_id`, so each `current_module`
        // transition happens exactly once per module.
        if current_module != Some(t.module_id) {
            let m = &tree.modules[t.module_id];
            let program = Program {
                items: m.items.clone(),
                ..Program::default()
            };
            let resolved = Resolver::new(&program)
                .with_tree(&tree, t.module_id as ModuleId)
                .resolve();
            let typed = crate::typechecker::TypeChecker::new(&program, &resolved)
                .with_tree(&tree, t.module_id as ModuleId)
                .check();
            current_program = Some(program);
            current_typed = Some(typed);
            current_module = Some(t.module_id);
        }
        let program_ref = current_program.as_ref().unwrap();
        let typed_ref = current_typed.as_ref().unwrap();
        let module = &tree.modules[t.module_id];

        let test_file_path = module
            .test_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        let mut interp = Interpreter::new(program_ref, typed_ref);
        interp.set_source_filename(&test_file_path);
        interp.register_for_tests();

        // Evaluate `#[with_provider(R, ctor)]` fixtures in source order,
        // pushing one provider frame per successful constructor. On the
        // first constructor failure we pop whatever we already pushed,
        // emit `provider_construction_failed`, and move to the next test
        // without running its body. Reset test state once up front so
        // constructor evaluation starts from a clean slate (same as
        // `run_test_function` does when it takes over).
        interp.reset_test_state();
        let mut pushed_frames: usize = 0;
        let mut constructor_failure: Option<(String, String)> = None;
        for fx in &t.with_providers {
            match interp.test_eval_provider_constructor(&fx.constructor) {
                Ok(v) => {
                    interp.test_push_provider(fx.resource_path.clone(), v);
                    pushed_frames += 1;
                }
                Err(msg) => {
                    constructor_failure = Some((fx.resource_path.clone(), msg));
                    break;
                }
            }
        }

        if let Some((resource, message)) = constructor_failure {
            for _ in 0..pushed_frames {
                interp.test_pop_provider_frame();
            }
            failed += 1;
            emit_test_event(
                "test_fail",
                &test_fail_provider_construction_fields(t, &resource, &message),
            );
            continue;
        }

        let active_providers: Vec<String> = t
            .with_providers
            .iter()
            .map(|fx| fx.resource_path.clone())
            .collect();

        let started = std::time::Instant::now();
        let outcome = interp.run_test_function(&t.fn_name);
        let duration_ms = started.elapsed().as_millis();

        // Pop every fixture frame before emitting the event so any error
        // handling below sees a clean stack for the next test.
        for _ in 0..pushed_frames {
            interp.test_pop_provider_frame();
        }

        if outcome.passed {
            passed += 1;
            emit_test_event(
                "test_pass",
                &format!(
                    "\"test\":{},\"duration_ms\":{}",
                    json_string(&t.qualified),
                    duration_ms
                ),
            );
        } else {
            failed += 1;
            emit_test_event(
                "test_fail",
                &test_fail_fields_with_providers(
                    t,
                    &outcome,
                    &test_file_path,
                    duration_ms,
                    &active_providers,
                ),
            );
        }
    }

    let total_duration_ms = run_started.elapsed().as_millis();
    emit_test_event(
        "summary",
        &format!(
            "\"total\":{},\"passed\":{},\"failed\":{},\"skipped\":{},\"duration_ms\":{}",
            tests.len(),
            passed,
            failed,
            skipped,
            total_duration_ms,
        ),
    );

    if failed > 0 {
        process::exit(1);
    }
}

/// Subset of `requires` whose resources are NOT currently available.
/// Order is preserved from the source list so the diagnostic reads in
/// declaration order — the runner emits this slice into the
/// `resources` field of the `test_skip`/`test_fail` event.
fn missing_resources(
    requires: &[String],
    overrides: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    requires
        .iter()
        .filter(|r| !probe_resource(r, overrides))
        .cloned()
        .collect()
}

fn test_skip_unsatisfied_requires_fields(t: &DiscoveredTest, missing: &[String]) -> String {
    format!(
        "\"test\":{},\"reason\":\"unsatisfied_requires\",\"resources\":{}",
        json_string(&t.qualified),
        json_string_array(missing),
    )
}

fn test_fail_unsatisfied_requires_fields(t: &DiscoveredTest, missing: &[String]) -> String {
    // `--all` promotes the same condition to a failure. The shape mirrors a
    // normal `test_fail` (test, message) plus a `reason` + `resources` pair
    // so consumers that filter by `reason` work uniformly across skip- and
    // fail-events. `duration_ms` is 0 — the test never executed.
    let message = format!(
        "required resource{} unavailable: {}",
        if missing.len() == 1 { "" } else { "s" },
        missing.join(", "),
    );
    format!(
        "\"test\":{},\"duration_ms\":0,\"reason\":\"unsatisfied_requires\",\"resources\":{},\"message\":{}",
        json_string(&t.qualified),
        json_string_array(missing),
        json_string(&message),
    )
}

/// Render a `Vec<String>` as a JSON array literal. Each element runs
/// through [`json_string`] for proper escaping.
fn json_string_array(items: &[String]) -> String {
    let mut s = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&json_string(item));
    }
    s.push(']');
    s
}

fn test_fail_fields(
    t: &DiscoveredTest,
    outcome: &TestOutcome,
    file_path: &str,
    duration_ms: u128,
) -> String {
    let mut s = format!(
        "\"test\":{},\"duration_ms\":{}",
        json_string(&t.qualified),
        duration_ms
    );
    if let Some(span) = &outcome.span {
        s.push_str(&format!(
            ",\"location\":{{\"file\":{},\"line\":{},\"col\":{}}}",
            json_string(file_path),
            span.line,
            span.column,
        ));
    }
    let message = outcome.message.as_deref().unwrap_or("test failed");
    s.push_str(&format!(",\"message\":{}", json_string(message)));
    if let Some(left) = &outcome.left {
        s.push_str(&format!(",\"left\":{}", json_string(left)));
    }
    if let Some(right) = &outcome.right {
        s.push_str(&format!(",\"right\":{}", json_string(right)));
    }
    s
}

/// Like `test_fail_fields` but also emits a `providers` array listing
/// the fully-qualified resource paths active for the test. Per design.md
/// § Testing, pass events stay lean; only failure events grow this
/// field so consumers reading pass/fail diffs can attribute the failure
/// to the fixture stack. Empty provider lists still emit the field for
/// shape consistency — it's `"providers":[]` in that case.
fn test_fail_fields_with_providers(
    t: &DiscoveredTest,
    outcome: &TestOutcome,
    file_path: &str,
    duration_ms: u128,
    providers: &[String],
) -> String {
    let mut s = test_fail_fields(t, outcome, file_path, duration_ms);
    s.push_str(&format!(",\"providers\":{}", json_string_array(providers)));
    s
}

/// Intersection of `#[test(requires = [...])]` resources and
/// `#[with_provider(...)]` resource paths. Preserves `requires` order so
/// the conflict list reads in source declaration order.
fn conflict_resources(requires: &[String], with_providers: &[WithProviderFixture]) -> Vec<String> {
    let with_set: std::collections::BTreeSet<&str> = with_providers
        .iter()
        .map(|f| f.resource_path.as_str())
        .collect();
    requires
        .iter()
        .filter(|r| with_set.contains(r.as_str()))
        .cloned()
        .collect()
}

fn test_fail_conflict_fields(t: &DiscoveredTest, conflicts: &[String]) -> String {
    let message = format!(
        "resource{} cannot appear in both `requires` and `with_provider`: {}",
        if conflicts.len() == 1 { "" } else { "s" },
        conflicts.join(", "),
    );
    format!(
        "\"test\":{},\"duration_ms\":0,\"reason\":\"requires_and_with_provider_conflict\",\"resources\":{},\"message\":{}",
        json_string(&t.qualified),
        json_string_array(conflicts),
        json_string(&message),
    )
}

/// `test_fail` event for `provider_construction_failed` — constructor
/// expression panicked, returned `Err`, or otherwise did not complete
/// normally. `duration_ms` is 0 — the test body never ran. Includes the
/// resource path whose constructor failed and the diagnostic message so
/// CI / LLM consumers can distinguish construction failures from test-
/// body failures.
fn test_fail_provider_construction_fields(
    t: &DiscoveredTest,
    resource: &str,
    message: &str,
) -> String {
    let wrapped = format!(
        "provider for `{}` failed to construct: {}",
        resource, message
    );
    format!(
        "\"test\":{},\"duration_ms\":0,\"reason\":\"provider_construction_failed\",\"resource\":{},\"message\":{}",
        json_string(&t.qualified),
        json_string(resource),
        json_string(&wrapped),
    )
}

/// Scaffold a new Kāra project (CR-36). Validates the package name, prepares
/// the target directory (creating `./<name>/` for the positional form), then
/// writes the template files via `scaffold::scaffold_project`. Every failure
/// aborts before any file is written — name validation and target-dir checks
/// run up front so a broken invocation never leaves partial state behind.
fn cmd_init(directory: Option<String>, template: Template, force: bool) {
    let (target_dir, package_name) = match directory {
        Some(name) => {
            if let Err(e) = scaffold::validate_package_name(&name) {
                eprintln!("error[scaffold/{}]: {e}", e.tag());
                process::exit(1);
            }
            let target = PathBuf::from(&name);
            if let Err(e) = scaffold::prepare_new_target_dir(&target) {
                eprintln!("error[scaffold/{}]: {e}", e.tag());
                process::exit(1);
            }
            (target, name)
        }
        None => {
            let cwd = match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("error: cannot read current directory: {e}");
                    process::exit(1);
                }
            };
            let basename = cwd
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if let Err(e) = scaffold::validate_package_name(&basename) {
                eprintln!("error[scaffold/{}]: {e}", e.tag());
                eprintln!(
                    "  note: deriving package name from the current directory basename `{}`",
                    cwd.display(),
                );
                process::exit(1);
            }
            (cwd, basename)
        }
    };

    let opts = ScaffoldOpts { template, force };
    match scaffold::scaffold_project(&target_dir, &package_name, opts) {
        Ok(()) => {
            let kind = match template {
                Template::Bin => "binary",
                Template::Lib => "library",
            };
            println!(
                "Scaffolded {kind} project `{package_name}` in {}",
                target_dir.display(),
            );
        }
        Err(e) => {
            eprintln!("error[scaffold/{}]: {e}", e.tag());
            process::exit(1);
        }
    }
}

// ── karac clean ──────────────────────────────────────────────────
//
// Remove a build-artifact cache. Bare form targets the project-local
// `dist/`; `--global` targets the user-wide `~/.kara/cache/` per
// `design.md § Package System > Build artifact cache`. Both forms are
// idempotent — a missing directory is logged and treated as success.

fn cmd_clean(global: bool) {
    let target: PathBuf = if global {
        match dirs_kara_cache_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: cannot resolve global cache path: {e}");
                process::exit(1);
            }
        }
    } else {
        let cwd = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: cannot read current directory: {e}");
                process::exit(1);
            }
        };
        cwd.join("dist")
    };

    let scope_label = if global {
        "global cache"
    } else {
        "project dist/"
    };
    match fs::metadata(&target) {
        Ok(_) => match fs::remove_dir_all(&target) {
            Ok(()) => {
                println!("removed {} ({})", target.display(), scope_label);
            }
            Err(e) => {
                eprintln!("error: failed to remove {}: {e}", target.display());
                process::exit(1);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "{} already absent ({}); nothing to do",
                target.display(),
                scope_label
            );
        }
        Err(e) => {
            eprintln!("error: cannot stat {}: {e}", target.display());
            process::exit(1);
        }
    }
}

// Resolve `~/.kara/cache/`. Honors `$HOME` first (matches the canonical
// behavior on Unix); on Windows-like setups where `$HOME` is unset,
// falls back to `$USERPROFILE`. No external crate dependency because
// the lookup is two env vars; an unset both-of-these case is the rare
// CI image with no home directory and is treated as a hard error.
fn dirs_kara_cache_path() -> Result<PathBuf, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "$HOME (and $USERPROFILE) unset".to_string())?;
    Ok(PathBuf::from(home).join(".kara").join("cache"))
}

// ── karac install ────────────────────────────────────────────────
//
// Build a binary package from a `<bin-spec>` and install the resulting
// executable into `~/.kara/bin/`. The spec accepts the same shapes as
// the manifest dependency entry: `path = "./local"`, `git = "https://..."`,
// or a bare registry-proxy reference like `my-tool` or `my-tool@1.2.3`.
//
// v1 surface (this slice): the subcommand parses cleanly and emits a
// "not yet wired" diagnostic that names the spec back. Full resolver +
// build + symlink machinery lands alongside the dependency-resolution
// slice (same gating as `--offline`).

fn cmd_install(spec: &str) {
    eprintln!(
        "karac install: not yet wired (received spec `{spec}`).\n\
         Build + ~/.kara/bin/ install machinery lands alongside the\n\
         dependency-resolution slice. Tracking: docs/implementation_checklist/phase-5-diagnostics.md."
    );
    process::exit(2);
}

// ── karac vendor ─────────────────────────────────────────────────
//
// Copy all resolved dependencies into a `vendor/` directory at the
// project root. Subsequent `karac build --offline` reads from
// `vendor/` and refuses network access. v1 surface — the resolver
// wiring lands alongside the dependency-resolution slice. v1 emits a
// "not yet wired" diagnostic that points operators at the canonical
// flag pairing (`vendor` + `build --offline`) so air-gap CI scripts
// can be scaffolded against the final surface today.

fn cmd_vendor() {
    eprintln!(
        "karac vendor: not yet wired.\n\
         Dependency copy into ./vendor/ lands alongside the\n\
         dependency-resolution slice. Pairs with `karac build --offline`.\n\
         Tracking: docs/implementation_checklist/phase-5-diagnostics.md."
    );
    process::exit(2);
}
