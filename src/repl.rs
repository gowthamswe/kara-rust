//! Line-based REPL over the tree-walk interpreter.
//!
//! Implements `karac repl` (P0 delivery item per `roadmap.md § Interactive
//! Development`). Each cell is wrapped as the body of an implicit
//! `fn main()` per `design.md § Interactive Evaluation Model > Cell Scope`,
//! with top-level items (fn / struct / enum / trait / impl / const)
//! accumulating across the session.
//!
//! Surface (v1 MVP):
//! - Read-eval-print loop with multi-line continuation (brace-balance probe).
//! - Persistent top-level items across cells.
//! - Meta-commands: `:help`, `:quit`, `:type <expr>`, `:effects`, `:save <file>`.
//! - Last expression in a cell auto-prints if it's not `Unit`.
//!
//! Persistent `let` bindings (v1 source-replay model):
//! - Top-level `let` / `let mut` statements from a successfully-evaluated
//!   statement cell are extracted (verbatim source) and prepended to every
//!   subsequent synthetic `fn main()` body. A later cell that references
//!   `x` resolves it against the replayed `let x = ...;` from cell N.
//! - Caveat: side effects on the RHS re-run on every cell (the value is
//!   recomputed, not snapshotted). Pure literal / arithmetic bindings
//!   behave like a notebook; `let x = read_file(...)` re-reads the file.
//!   Full value-snapshot semantics ship with the Jupyter kernel CR
//!   (interpreter surgery scheduled separately).
//! - `:reset` clears the persistent-let buffer for users who want to
//!   recover from an expensive replay.
//!
//! Other v1 limitations (called out in `:help`):
//! - Mutation in a statement cell (`x += 1;`) does not survive to the
//!   next cell — only `let` source replays. Re-binding (`let x = x + 1;`)
//!   is the supported idiom for now.
//! - `--auto-clone`, notebook-aware use-after-move hints, and rich Jupyter
//!   display all ship as separate items in the Interactive Development
//!   chapter of `roadmap.md`.

use std::collections::BTreeMap;
use std::ops::Range;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

/// Caller-supplied options for `run_with_options` — mirrors the flags
/// surfaced on the `karac repl` subcommand. `Default` matches the
/// historical bare-`karac repl` behavior so existing callers keep working.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplOptions {
    /// `--auto-clone`: when on, the REPL auto-inserts `.clone()` at
    /// cross-cell consume sites flagged by `UseAfterMove`, rather than
    /// surfacing the diagnostic. Each insertion emits a
    /// `perf[auto-clone-in-repl]` note (never silent — design.md §
    /// Interactive Evaluation Model > Ownership Across Cells).
    pub auto_clone: bool,
}

/// Run the interactive REPL until EOF or `:quit`. Returns once the user
/// exits; never reaches the rest of the CLI dispatch.
pub fn run() {
    run_with_options(ReplOptions::default());
}

/// Surface for the binary entry point: launch the REPL with caller-supplied
/// options. Used by `karac repl` flag wiring (`--auto-clone`).
pub fn run_with_options(opts: ReplOptions) {
    let banner = if opts.auto_clone {
        "Kāra REPL  (auto-clone on; type :help for commands, :quit to exit)"
    } else {
        "Kāra REPL  (type :help for commands, :quit to exit)"
    };
    println!("{banner}");

    let mut session = Session::with_options(opts);
    let mut editor = match DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: failed to start line editor: {e}");
            return;
        }
    };

    loop {
        let primary = "> ";
        let cont = ".. ";
        let mut buffer = String::new();
        let mut prompt = primary;

        let line = match editor.readline(prompt) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => {
                eprintln!("error: {e}");
                continue;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        // Meta-command short-circuit (only on the first line of a cell).
        if line.starts_with(':') {
            let _ = editor.add_history_entry(&line);
            if !session.dispatch_meta(line.trim()) {
                break;
            }
            continue;
        }

        buffer.push_str(&line);
        buffer.push('\n');
        prompt = cont;

        // Brace/paren balance probe — fall into multi-line mode while the
        // input is structurally incomplete. Strings and comments are coarse-
        // tracked so braces inside them don't trip the probe.
        while !is_balanced(&buffer) {
            let next = match editor.readline(prompt) {
                Ok(l) => l,
                Err(ReadlineError::Interrupted) => {
                    buffer.clear();
                    break;
                }
                Err(ReadlineError::Eof) => break,
                Err(e) => {
                    eprintln!("error: {e}");
                    buffer.clear();
                    break;
                }
            };
            buffer.push_str(&next);
            buffer.push('\n');
        }

        if buffer.trim().is_empty() {
            continue;
        }

        let _ = editor.add_history_entry(buffer.trim_end());
        session.evaluate_cell(&buffer);
    }
}

/// Result of `Session::evaluate_cell_captured` — captured stdout plus any
/// parse / resolve / type errors. Used by integration tests; the
/// production `evaluate_cell` writes directly to the process's stdout/
/// stderr instead.
#[derive(Debug, Default)]
pub struct EvaluatedCell {
    pub stdout: String,
    pub errors: Vec<String>,
    /// Compiler-surface notes that aren't fatal — today this carries the
    /// `perf[auto-clone-in-repl]` lines emitted when `--auto-clone` rewrites
    /// a consume site. Always non-empty when an insertion fired (the spec
    /// requires the channel be visible — never silent). Production
    /// `evaluate_cell` mirrors each entry to stderr so users see them.
    pub notes: Vec<String>,
}

/// Internal result type for `run_with_wrapper_inner` — bundles captured
/// stdout, diagnostic strings, and auto-clone perf notes so the caller
/// can route each to the right surface (stderr / stdout / `EvaluatedCell`
/// fields). Unlike `EvaluatedCell`, both `Ok` and `Err` arms can carry
/// notes — `--auto-clone` emits perf notes on every insertion regardless
/// of whether the cell ultimately compiled cleanly.
struct WrapperOutcome {
    stdout: Vec<String>,
    errors: Vec<String>,
    notes: Vec<String>,
}

impl WrapperOutcome {
    fn errors(errors: Vec<String>, notes: Vec<String>) -> Self {
        Self {
            stdout: Vec::new(),
            errors,
            notes,
        }
    }
}

/// Per-session state: accumulated source for top-level items + cell history
/// captured for `:save`. Public so integration tests can exercise the cell
/// pipeline without driving rustyline through a TTY.
pub struct Session {
    /// Concatenated source text of every top-level item (fn / struct / enum /
    /// trait / impl / const / type / distinct / extern / use / import) seen
    /// in any prior cell, in submission order. Replayed at the head of each
    /// new synthetic program so later cells resolve names from earlier ones.
    items_source: String,
    /// Verbatim cell inputs in submission order, used by `:save` to write a
    /// `.kara` file that reconstructs the session. Excludes meta-commands.
    cell_history: Vec<String>,
    /// Source slices of every top-level `let` / `let mut` / `let ... else`
    /// statement extracted from successfully-evaluated statement cells, in
    /// submission order. Re-emitted at the head of every subsequent
    /// synthetic `fn main()` body so cell N's `let x = 5;` is in scope when
    /// cell N+1 evaluates `println(x);`. v1 source-replay model — values
    /// are recomputed each cell rather than snapshotted; see the module
    /// docs for the caveat.
    persistent_lets: Vec<String>,
    /// 1-based origin cell index for each entry in `persistent_lets`. Length
    /// is invariant-equal to `persistent_lets.len()`. Recorded so the
    /// REPL-aware diagnostic-rendering path can map a span landing inside
    /// a replayed `let` slice back to the cell that originally introduced
    /// the binding (drives the "consumed by cell N" tail on cross-cell
    /// `UseAfterMove` errors). The 1-based convention matches what users
    /// see in cell labels (cell 1, cell 2, …); cell 0 is reserved for
    /// synthetic / wrapper-introduced source that does not correspond to
    /// any user cell.
    persistent_let_origin: Vec<usize>,
    /// Byte ranges, in the most recently built synthetic-program source,
    /// that map back to a user cell. Each entry is `(cell_idx_1_based,
    /// byte_range)`. Re-populated at the top of every `run_with_wrapper_inner`
    /// / `evaluate_cell_captured` call, before the parser runs, so the
    /// downstream diagnostic-rendering layer can call `cell_for_span`
    /// against fresh data. Ranges are non-overlapping and source-ordered
    /// so a binary search over them recovers the cell for any span. The
    /// next slice (`--auto-clone` opt-in mode) consumes the same data to
    /// decide which cell's source to rewrite when inserting `.clone()`;
    /// the shape here is deliberately rich enough to support that without
    /// surfacing helper API the auto-clone slice doesn't need yet.
    cell_byte_ranges: Vec<(usize, Range<usize>)>,
    /// `--auto-clone` opt-in mode. When `true`, the REPL pipeline detects
    /// cross-cell `UseAfterMove` errors (using the same `cell_for_span`
    /// machinery that drives the notebook-aware diagnostic), rewrites the
    /// consume-site cell to insert `.clone()` after the consumed binding,
    /// and re-runs the cell pipeline. The rewrite is recorded in
    /// `cell_history[M-1]` AND the matching `persistent_lets[i]` slot so
    /// the inserted `.clone()` survives both `:save` export and subsequent
    /// cell compilations. Each insertion emits a `perf[auto-clone-in-repl]`
    /// note onto the cell's `notes` channel — never silent (design.md §
    /// Interactive Evaluation Model > Ownership Across Cells).
    auto_clone: bool,
    /// In-memory `[dependencies]` table built up via the `:dep` meta-command.
    /// Each entry is `name → normalized TOML value` (e.g. `"\"1.2\""` for a
    /// bare semver, `"{ git = \"...\" }"` for an inline-table form). v1
    /// stores the request only — package resolution / download / symbol
    /// registration is the package manager's responsibility (design.md
    /// tags `:dep` as v1.1; the resolver, registry proxy, and lock-file
    /// machinery have not landed yet). The buffer here is the integration
    /// point: when the package manager arrives, it consumes this map at
    /// the head of each subsequent cell to register surface symbols.
    pending_deps: BTreeMap<String, String>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Session {
            items_source: String::new(),
            cell_history: Vec::new(),
            persistent_lets: Vec::new(),
            persistent_let_origin: Vec::new(),
            cell_byte_ranges: Vec::new(),
            auto_clone: false,
            pending_deps: BTreeMap::new(),
        }
    }

    /// Construct a session pre-configured from caller-supplied
    /// `ReplOptions` (the CLI flag bag). Used by `run_with_options` and
    /// integration tests that exercise `--auto-clone` without driving
    /// rustyline through a TTY.
    pub fn with_options(opts: ReplOptions) -> Self {
        let mut s = Self::new();
        s.auto_clone = opts.auto_clone;
        s
    }

    /// Inspector for the `--auto-clone` flag. Used by integration tests.
    pub fn auto_clone(&self) -> bool {
        self.auto_clone
    }

    /// Inspector for the accumulated `let`-statement replay buffer. Used by
    /// integration tests and by a future `:show lets` meta-command.
    pub fn persistent_lets(&self) -> &[String] {
        &self.persistent_lets
    }

    /// Inspector for the in-memory `:dep` registry. Returns `name → spec`
    /// where `spec` is the normalized TOML value (a quoted semver string
    /// or an inline-table source). Used by integration tests; consumed
    /// by the package manager once it lands.
    pub fn pending_deps(&self) -> &BTreeMap<String, String> {
        &self.pending_deps
    }

    /// Inspector for accumulated items source. Used by integration tests
    /// and could surface inside a `:show items` meta-command later.
    pub fn items_source(&self) -> &str {
        &self.items_source
    }

    /// Inspector for verbatim cell history. Used by `:save` and integration
    /// tests; could back a future `:history` meta-command.
    pub fn cell_history(&self) -> &[String] {
        &self.cell_history
    }

    /// Inspector for the current cell-byte-range map (the synthetic-source
    /// → cell mapping built by the most recent compilation). Used by
    /// integration tests and consumed in the next slice (`--auto-clone`
    /// opt-in mode) to drive consume-site rewrites.
    pub fn cell_byte_ranges(&self) -> &[(usize, Range<usize>)] {
        &self.cell_byte_ranges
    }

    /// Map a span (recorded against the most recently built synthetic-
    /// program source) to the 1-based cell index that produced the
    /// corresponding source bytes. Returns `None` for spans that fall
    /// outside any tracked range — synthetic wrapper bytes (`fn main() {`,
    /// closing `}`), `items_source` content, or stale spans whose offsets
    /// no longer line up after a re-compile. The `cell_byte_ranges` table
    /// is non-overlapping and source-ordered, so the linear scan here is
    /// O(n) in the number of cells contributing to the current synthetic
    /// source — fine for v1 cell counts.
    pub fn cell_for_span(&self, span: &crate::token::Span) -> Option<usize> {
        let start = span.offset;
        for (cell_idx, range) in &self.cell_byte_ranges {
            if range.contains(&start) {
                return Some(*cell_idx);
            }
        }
        None
    }

    /// Handle a `:meta` command. Returns `false` when the user requested
    /// quit, `true` to continue the loop.
    pub fn dispatch_meta(&mut self, line: &str) -> bool {
        // Split on first whitespace.
        let (cmd, rest) = match line.split_once(char::is_whitespace) {
            Some((c, r)) => (c.trim(), r.trim()),
            None => (line.trim(), ""),
        };
        match cmd {
            ":help" => {
                print_help();
            }
            ":quit" | ":q" | ":exit" => {
                return false;
            }
            ":type" => {
                if rest.is_empty() {
                    eprintln!("usage: :type <expr>");
                } else {
                    self.show_type(rest);
                }
            }
            ":effects" => {
                self.show_effects();
            }
            ":save" => {
                if rest.is_empty() {
                    eprintln!("usage: :save <path.kara>");
                } else {
                    self.save(rest);
                }
            }
            ":reset" => {
                let cleared = self.persistent_lets.len();
                self.reset_persistent_lets();
                println!("(cleared {cleared} persistent let binding(s); items + history kept)");
            }
            ":dep" => {
                if rest.is_empty() {
                    eprintln!(
                        "usage: :dep <name> = \"<version>\"  or  :dep <name> = {{ git = \"...\" }}"
                    );
                } else {
                    self.add_dep(rest);
                }
            }
            other => {
                eprintln!("unknown meta-command '{other}'. Try :help for the supported list.");
            }
        }
        true
    }

    /// Evaluate one cell. Classification is best-effort: the cell is
    /// classified as "pure items" iff a standalone parse of the input
    /// produces at least one top-level item AND no top-level statements
    /// are present. Otherwise the cell is treated as statements and run
    /// inside the synthetic `fn main()` wrapper.
    pub fn evaluate_cell(&mut self, src: &str) {
        let trimmed = src.trim_end();
        self.cell_history.push(trimmed.to_string());

        if classify_input(trimmed) == CellShape::PureItems {
            // Pure-item cell: append to the accumulated source and
            // re-validate the whole thing so type-checker errors surface
            // immediately rather than waiting for the next statement cell.
            self.append_items(trimmed);
            self.validate_accumulated();
            return;
        }

        // Statement / expression / mixed cell — wrap in a synthetic main.
        self.run_with_wrapper(trimmed);
    }

    fn append_items(&mut self, src: &str) {
        // Per design.md § Cell Scope, a later cell's `fn f` / `struct Point`
        // / `const X` shadows the earlier one. The accumulated buffer holds
        // both, so the resolver would surface a duplicate-name error if we
        // just appended. Strip the prior definition(s) of any name the new
        // cell re-declares before appending — anonymous items (`impl`
        // blocks, `use` / `import`, `independent`) are never pruned because
        // they have no shadowable name.
        self.prune_shadowed_items(src);
        if !self.items_source.is_empty() && !self.items_source.ends_with('\n') {
            self.items_source.push('\n');
        }
        self.items_source.push_str(src);
        if !self.items_source.ends_with('\n') {
            self.items_source.push('\n');
        }
    }

    /// Remove every prior top-level definition from `items_source` whose
    /// name appears in `new_item_src`. Re-parses the buffer, identifies
    /// each surviving item's source range (extended to the end of the
    /// previous item so doc comments and inter-item whitespace ride along
    /// with the *next* item), and concatenates the kept ranges back into
    /// the buffer. Trailing content after the last item rides with the
    /// last kept item so it doesn't get stranded.
    fn prune_shadowed_items(&mut self, new_item_src: &str) {
        let new_names = collect_top_level_item_names(new_item_src);
        if new_names.is_empty() || self.items_source.trim().is_empty() {
            return;
        }
        let parsed = crate::parse(&self.items_source);
        if !parsed.errors.is_empty() {
            // The buffer is already broken — leave it alone so the user
            // sees the original parse error rather than a corrupted view
            // produced by our prune.
            return;
        }
        let total_len = self.items_source.len();
        let mut keep_ranges: Vec<(usize, usize)> = Vec::new();
        let mut cursor = 0usize;
        for item in &parsed.program.items {
            let span = item_span(item);
            let item_end = span.offset.saturating_add(span.length);
            // Everything from `cursor` (the end of the previous item, or 0)
            // up to `item_end` belongs to this item — including any leading
            // doc comments / attributes / `pub` keyword that sit before the
            // recorded span.
            let extended_start = cursor;
            cursor = item_end;
            let keep = match item_name(item) {
                Some(n) => !new_names.contains(n),
                None => true,
            };
            if keep {
                keep_ranges.push((extended_start, item_end));
            }
        }
        // Attach any trailing content (final newline, dangling comments) to
        // the last kept item so it doesn't get dropped.
        if let Some(last) = keep_ranges.last_mut() {
            if cursor < total_len {
                last.1 = total_len;
            }
        }
        let mut rebuilt = String::with_capacity(total_len);
        for (start, end) in keep_ranges {
            rebuilt.push_str(&self.items_source[start..end]);
        }
        self.items_source = rebuilt;
    }

    /// Run the resolver + typechecker over the accumulated items so that
    /// item-only cells (struct/fn definitions) report errors immediately
    /// rather than waiting for a future evaluation cell.
    fn validate_accumulated(&mut self) {
        if self.items_source.trim().is_empty() {
            return;
        }
        let parsed = crate::parse(&self.items_source);
        if !parsed.errors.is_empty() {
            for e in &parsed.errors {
                eprintln!("parse error: {}", e.message);
            }
            return;
        }
        let resolved = crate::resolve(&parsed.program);
        if !resolved.errors.is_empty() {
            for e in &resolved.errors {
                eprintln!("resolve error: {}", e.message);
            }
            return;
        }
        let typed = crate::typecheck(&parsed.program, &resolved);
        for e in &typed.errors {
            eprintln!("type error: {}", e.message);
        }
    }

    fn run_with_wrapper(&mut self, cell_src: &str) {
        match self.run_with_wrapper_inner(cell_src, /* capture */ false) {
            Ok(out) => {
                for note in &out.notes {
                    eprintln!("{note}");
                }
                self.capture_new_lets(cell_src);
            }
            Err(out) => {
                for note in &out.notes {
                    eprintln!("{note}");
                }
                for m in &out.errors {
                    eprintln!("{m}");
                }
            }
        }
    }

    /// Run a wrapper cell, optionally capturing the interpreter's stdout
    /// output. The returned `WrapperOutcome` carries either captured
    /// stdout lines (`Ok` shape) or diagnostic strings (`Err` shape), plus
    /// any auto-clone perf notes emitted along the way (always surfaced —
    /// `--auto-clone` is never silent).
    fn run_with_wrapper_inner(
        &mut self,
        cell_src: &str,
        capture: bool,
    ) -> Result<WrapperOutcome, WrapperOutcome> {
        // Shadow-prune: drop any prior persistent let whose name(s) the new
        // cell re-binds. Kāra rejects same-scope re-declaration, so without
        // this prune `cell 1: let x = 1;` followed by `cell 2: let x = 99;`
        // would fail at the resolver inside cell 2's synthetic main. Per
        // design.md § Cell Scope, the later cell shadows the earlier
        // binding — source-replay approximates that by pruning.
        self.prune_shadowed_lets(cell_src);

        // Auto-clone iteration loop: when `auto_clone` is on, cross-cell
        // UAM errors found by the ownership pass drive a source rewrite of
        // the consuming cell's stored slice. The rewritten slice replaces
        // the entry in `persistent_lets` AND in `cell_history` (so `:save`
        // exports the clone'd form). After each successful rewrite the
        // pipeline restarts from synthetic-source assembly. The cap is
        // generous — `persistent_lets.len() + 8` covers every realistic
        // multi-binding consume cell while still bounding pathological
        // input. With the flag off this loop runs at most once and falls
        // through the existing rendering paths unchanged.
        let max_iters = self.persistent_lets.len() + 8;
        let mut notes: Vec<String> = Vec::new();
        for _ in 0..=max_iters {
            let synthetic = self.build_synthetic_cell(cell_src);
            let mut parsed = crate::parse(&synthetic);
            if !parsed.errors.is_empty() {
                return Err(WrapperOutcome::errors(
                    parsed
                        .errors
                        .iter()
                        .map(|e| format!("parse error: {}", e.message))
                        .collect(),
                    notes,
                ));
            }
            let resolved = crate::resolve(&parsed.program);
            if !resolved.errors.is_empty() {
                return Err(WrapperOutcome::errors(
                    resolved
                        .errors
                        .iter()
                        .map(|e| format!("resolve error: {}", e.message))
                        .collect(),
                    notes,
                ));
            }
            let typed = crate::typecheck(&parsed.program, &resolved);
            if !typed.errors.is_empty() {
                return Err(WrapperOutcome::errors(
                    typed
                        .errors
                        .iter()
                        .map(|e| format!("type error: {}", e.message))
                        .collect(),
                    notes,
                ));
            }

            // Round-trip the program through the ownership checker so cross-
            // cell `UseAfterMove` errors fire in REPL context the same way
            // they fire on `.kara` files. Strictness is identical to the
            // .kara surface; only diagnostic *presentation* is enriched.
            // Rendering happens via `render_ownership_errors_repl`, which
            // appends a notebook-aware tail to UAM errors whose consume site
            // and use site land in different cells.
            let owned = crate::ownershipcheck(&parsed.program, &typed);
            if !owned.errors.is_empty() {
                if self.auto_clone {
                    // Try to rewrite consume sites for the cross-cell UAM
                    // arms. `apply_auto_clone_rewrites` mutates
                    // `persistent_lets` / `cell_history` in place and
                    // returns the perf-note lines for the insertions it
                    // performed. If at least one rewrite happened, restart
                    // the compile pipeline; otherwise fall through to the
                    // baseline rendering path. The rewrites can't restore
                    // a binding the user has already moved within the
                    // same cell — same-cell UAM still surfaces, matching
                    // the slice's "cross-cell only" rule.
                    let new_notes = self.apply_auto_clone_rewrites(&owned.errors);
                    if !new_notes.is_empty() {
                        notes.extend(new_notes);
                        continue;
                    }
                }
                return Err(WrapperOutcome::errors(
                    self.render_ownership_errors_repl(&owned.errors),
                    notes,
                ));
            }

            crate::lower(&mut parsed.program, &typed);

            let mut interp = crate::interpreter::Interpreter::new(&parsed.program, &typed);
            if capture {
                interp.captured_output = Some(Vec::new());
            }
            interp.run();
            return Ok(WrapperOutcome {
                stdout: interp.captured_output.take().unwrap_or_default(),
                errors: Vec::new(),
                notes,
            });
        }
        // The loop bound is a defensive cap — every realistic auto-clone
        // sequence converges in `persistent_lets.len() + 1` iterations
        // (one rewrite per consume site, plus a final clean compile).
        // Reaching this branch indicates a pathological repeat-rewrite
        // scenario; surface as a regular ownership error so the user
        // isn't left with a silent failure.
        Err(WrapperOutcome::errors(
            vec!["ownership error: --auto-clone exceeded its rewrite budget; aborting".to_string()],
            notes,
        ))
    }

    /// Apply auto-clone rewrites for the cross-cell `UseAfterMove` arms
    /// in `errors`. Returns one `perf[auto-clone-in-repl]` note per
    /// successful insertion (always non-empty when at least one rewrite
    /// was applied; the caller restarts the compile when the slice is
    /// non-empty). The rewrite locates the consume-site span in the
    /// synthetic source we just built, identifies which `persistent_lets`
    /// slot owns the corresponding bytes, and splices `.clone()` after
    /// the consumed identifier. The matching `cell_history[M-1]` entry
    /// is rewritten in lockstep so `:save` exports the clone'd form.
    fn apply_auto_clone_rewrites(
        &mut self,
        errors: &[crate::ownership::OwnershipError],
    ) -> Vec<String> {
        let mut notes: Vec<String> = Vec::new();
        // Collect rewrite intents first so we don't mutate persistent_lets
        // while iterating over its indices. Each intent records the
        // persistent_lets slot, the local byte offset of the identifier
        // end, the binding name, and the consuming cell index.
        struct Intent {
            slot: usize,
            local_end: usize,
            binding: String,
            consume_cell: usize,
            use_cell: usize,
        }
        let mut intents: Vec<Intent> = Vec::new();
        for err in errors {
            if err.kind != crate::ownership::OwnershipErrorKind::UseAfterMove {
                continue;
            }
            let Some(cs) = err.consume_span.as_ref() else {
                continue;
            };
            let consume_cell = match self.cell_for_span(cs) {
                Some(c) => c,
                None => continue,
            };
            let use_cell = match self.cell_for_span(&err.span) {
                Some(c) => c,
                None => continue,
            };
            // Same-cell UAM is out of scope for auto-clone — the existing
            // diagnostic still fires for it.
            if consume_cell == use_cell {
                continue;
            }
            // Locate the persistent_lets slot whose byte range contains
            // the consume span. Falls through silently if the consume
            // site is in the current cell body (use_cell == current and
            // consume_cell < use_cell means the consume sits in a
            // replayed `let`; that's the case the inheritance caveat
            // from slice 5 narrows us to).
            let Some(slot) = self.persistent_let_slot_for_offset(cs.offset) else {
                continue;
            };
            let slot_range = match self.cell_byte_ranges.iter().find_map(|(_, r)| {
                if r.start <= cs.offset && cs.offset < r.end {
                    Some(r.clone())
                } else {
                    None
                }
            }) {
                Some(r) => r,
                None => continue,
            };
            let local_end = cs
                .offset
                .saturating_sub(slot_range.start)
                .saturating_add(cs.length);
            // Guard: `local_end` must fall inside the persistent_lets
            // slice (the trailing `\n` we appended in build_synthetic_cell
            // sits past the stored slice's end, so subtract that off).
            let stored_len = self.persistent_lets[slot].len();
            if local_end > stored_len {
                continue;
            }
            // Extract the binding name from the consume span bytes — it's
            // a bare identifier, so we can read it straight off
            // persistent_lets[slot].
            let local_start = cs.offset.saturating_sub(slot_range.start);
            if local_start >= stored_len {
                continue;
            }
            let binding = self.persistent_lets[slot][local_start..local_end].to_string();
            intents.push(Intent {
                slot,
                local_end,
                binding,
                consume_cell,
                use_cell,
            });
        }
        if intents.is_empty() {
            return notes;
        }
        // Apply intents grouped by slot, descending by local_end so
        // earlier rewrites inside the same slot don't shift later
        // offsets. Multiple rewrites in one slot is rare (one consumed
        // binding per `let` in v1 source-replay) but the descending
        // sort is cheap insurance.
        intents.sort_by(|a, b| {
            a.slot
                .cmp(&b.slot)
                .then_with(|| b.local_end.cmp(&a.local_end))
        });
        // Track which (slot, local_end) we already rewrote so duplicate
        // diagnostics on the same site don't double-insert.
        let mut applied: std::collections::HashSet<(usize, usize)> =
            std::collections::HashSet::new();
        for intent in &intents {
            if !applied.insert((intent.slot, intent.local_end)) {
                continue;
            }
            // Splice `.clone()` into the persistent_lets entry.
            let entry = &mut self.persistent_lets[intent.slot];
            entry.insert_str(intent.local_end, ".clone()");
            // Re-emit the rewritten slice into the matching
            // cell_history entry. Because cell_history records the cell
            // verbatim (a multi-line block possibly containing many
            // statements), we rewrite by recomposing: rebuild the
            // history string from the persistent_lets slots that share
            // this consume_cell origin, joined onto whatever non-`let`
            // remainder the original cell carried. v1 simplification:
            // overwrite the cell_history entry by replacing the bare
            // `<binding>` token's *first* occurrence inside the
            // statement that targeted this consume — same effect as the
            // local splice when the original cell was the `let _t =
            // consume(s);` shape the slice's caveat narrows us to.
            let cell_idx0 = intent.consume_cell.saturating_sub(1);
            if let Some(cell_src) = self.cell_history.get_mut(cell_idx0) {
                rewrite_cell_history_consume(cell_src, &intent.binding);
            }
            notes.push(format!(
                "perf[auto-clone-in-repl]: inserted `.clone()` on `{}` at consume site (cell {}, used in cell {}); cross-cell binding kept alive — disable with `karac repl` (no `--auto-clone`) to surface the underlying use-after-move diagnostic instead",
                intent.binding, intent.consume_cell, intent.use_cell,
            ));
        }
        notes
    }

    /// Locate the index in `persistent_lets` whose backing byte range
    /// (inside the most recently built synthetic source) contains
    /// `offset`. Returns `None` when the offset falls outside every
    /// stored slot — including when it lands inside the current cell's
    /// body block (the trailing `cell_byte_ranges` entry tagged with the
    /// current 1-based cell index, which is *not* a persistent_lets slot
    /// yet). Callers use this to decide whether the consume-site rewrite
    /// targets a replayed `let` or the freshly-submitted cell body. The
    /// slot index found is returned in `persistent_lets` order — the
    /// same order ranges are emitted in `build_synthetic_cell`.
    fn persistent_let_slot_for_offset(&self, offset: usize) -> Option<usize> {
        // The first `persistent_lets.len()` entries in `cell_byte_ranges`
        // map onto the persistent_lets slots in order (skipping any
        // entries whose origin was the sentinel 0 — which `capture_new_lets`
        // never produces in practice but the legacy fallback in
        // `build_synthetic_cell` still defends). The trailing entry is
        // the current cell's body block; never a persistent_lets slot.
        // We mirror that iteration order here.
        let n = self.persistent_lets.len();
        for (i, range) in self.cell_byte_ranges.iter().take(n).enumerate() {
            if range.1.start <= offset && offset < range.1.end {
                return Some(i);
            }
        }
        None
    }

    /// Render ownership errors with REPL-aware enrichment for
    /// `UseAfterMove` diagnostics. When the consume site and the use site
    /// resolve to different cells (via `cell_for_span` over the just-built
    /// `cell_byte_ranges`), append a notebook-aware tail naming the
    /// consuming cell and pointing the user at `.clone()` at the consume
    /// site. Same-cell UAM and every non-UAM kind use the existing
    /// rendering verbatim — strictness is identical to `.kara` files; only
    /// presentation differs in REPL context.
    fn render_ownership_errors_repl(
        &self,
        errors: &[crate::ownership::OwnershipError],
    ) -> Vec<String> {
        let mut rendered: Vec<String> = Vec::with_capacity(errors.len());
        for err in errors {
            let mut line = format!("ownership error: {}", err.message);
            if let Some(s) = err.suggestion.as_deref() {
                line.push_str("\n  help: ");
                line.push_str(s);
            }
            // Notebook-aware tail: only fires for UseAfterMove with a
            // consume-site span that resolves to a different cell than
            // the use-site span. Same-cell UAM and any kind without a
            // recorded `consume_span` fall through unchanged.
            if err.kind == crate::ownership::OwnershipErrorKind::UseAfterMove {
                if let Some(consume_span) = err.consume_span.as_ref() {
                    let consume_cell = self.cell_for_span(consume_span);
                    let use_cell = self.cell_for_span(&err.span);
                    if let (Some(c), Some(u)) = (consume_cell, use_cell) {
                        if c != u {
                            line.push_str(&format!(
                                "\n  note: consumed by cell {c} (`{}`); add `.clone()` at the consume site to keep the original binding usable in later cells",
                                cell_preview(self.cell_history.get(c.saturating_sub(1)).map(String::as_str).unwrap_or("")),
                            ));
                        }
                    }
                }
            }
            rendered.push(line);
        }
        rendered
    }

    /// Test helper: evaluate a cell and return any captured output and
    /// surfaced errors. Mirrors `evaluate_cell` but routes the
    /// interpreter's stdout into an in-memory buffer instead of the
    /// process's stdout.
    pub fn evaluate_cell_captured(&mut self, src: &str) -> EvaluatedCell {
        let trimmed = src.trim_end();
        self.cell_history.push(trimmed.to_string());

        if classify_input(trimmed) == CellShape::PureItems {
            self.append_items(trimmed);
            // Mirror validate_accumulated but collect errors for return.
            let mut errs = Vec::new();
            if !self.items_source.trim().is_empty() {
                let parsed = crate::parse(&self.items_source);
                for e in &parsed.errors {
                    errs.push(format!("parse error: {}", e.message));
                }
                if parsed.errors.is_empty() {
                    let resolved = crate::resolve(&parsed.program);
                    for e in &resolved.errors {
                        errs.push(format!("resolve error: {}", e.message));
                    }
                    let typed = crate::typecheck(&parsed.program, &resolved);
                    for e in &typed.errors {
                        errs.push(format!("type error: {}", e.message));
                    }
                }
            }
            // Roll back history if pure-item parse trips an error so :save
            // never bakes in a broken cell.
            if !errs.is_empty() {
                // Drop the failing cell from history AND from items_source —
                // the failing definition would block all subsequent cells.
                self.cell_history.pop();
                let trimmed_added = trimmed.to_string();
                if let Some(idx) = self.items_source.rfind(&trimmed_added) {
                    self.items_source.truncate(idx);
                }
            }
            return EvaluatedCell {
                stdout: String::new(),
                errors: errs,
                notes: Vec::new(),
            };
        }

        // Statement / expression / mixed cell.
        match self.run_with_wrapper_inner(trimmed, /* capture */ true) {
            Ok(out) => {
                self.capture_new_lets(trimmed);
                EvaluatedCell {
                    stdout: out.stdout.join(""),
                    errors: Vec::new(),
                    notes: out.notes,
                }
            }
            Err(out) => {
                // Roll back history on diagnostic-side failure.
                self.cell_history.pop();
                EvaluatedCell {
                    stdout: String::new(),
                    errors: out.errors,
                    notes: out.notes,
                }
            }
        }
    }

    /// Synthesize the source text fed to the parser for a non-pure-item cell:
    /// `<items_source>\nfn main() { <persistent_lets>\n<cell_body> }`.
    /// Using `main` (not `__cell_N`) ensures `Interpreter::run` finds the
    /// entry point automatically. If the user already declared `main` in
    /// `items_source`, the new one shadows it (Cell Scope re-declaration
    /// rule from design.md). The persistent-let replay block sits at the
    /// top of the wrapper body so each cell sees the same flat scope as
    /// design.md § Cell Scope describes.
    ///
    /// Side effect: rebuilds `Session.cell_byte_ranges` so it reflects the
    /// just-assembled source. Each replayed `let` slice is tagged with
    /// the cell index that originally introduced it (from
    /// `persistent_let_origin`); the trailing `cell_src` block is tagged
    /// with the *current* cell's 1-based index (the one being submitted).
    fn build_synthetic_cell(&mut self, cell_src: &str) -> String {
        let mut s = String::new();
        s.push_str(&strip_main(&self.items_source));
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("fn main() {\n");
        let mut ranges: Vec<(usize, Range<usize>)> = Vec::new();
        for (i, prior_let) in self.persistent_lets.iter().enumerate() {
            let start = s.len();
            s.push_str(prior_let);
            if !prior_let.ends_with('\n') {
                s.push('\n');
            }
            let end = s.len();
            // Origin index 0 (a sentinel) means "unknown cell" — should
            // not occur with correct upkeep but if it does, skip the
            // mapping rather than synthesize a misleading number.
            if let Some(&cell_idx) = self.persistent_let_origin.get(i) {
                if cell_idx > 0 {
                    ranges.push((cell_idx, start..end));
                }
            }
        }
        // The cell currently being evaluated occupies the next contiguous
        // slice of the wrapper body. Its 1-based index is one past the
        // current `cell_history` length (the entry has already been pushed
        // by the caller before `build_synthetic_cell` runs).
        let cell_idx = self.cell_history.len();
        let body_start = s.len();
        s.push_str(cell_src);
        if !cell_src.ends_with('\n') {
            s.push('\n');
        }
        let body_end = s.len();
        if cell_idx > 0 {
            ranges.push((cell_idx, body_start..body_end));
        }
        s.push_str("}\n");
        self.cell_byte_ranges = ranges;
        s
    }

    /// Extract every top-level `let` / `let mut` / `let ... else` statement
    /// from the cell body and append its raw source to `persistent_lets`.
    /// Called only after the cell evaluated cleanly so failed cells never
    /// leak partial bindings forward. Shadow-pruning happened earlier in
    /// `prune_shadowed_lets` so the synthetic main could parse cleanly;
    /// at this point the new entries can simply append.
    ///
    /// Each new entry is tagged with the 1-based index of the current cell
    /// in `persistent_let_origin` so the diagnostic-rendering layer can map
    /// a span landing inside the replayed slice back to the originating
    /// cell.
    fn capture_new_lets(&mut self, cell_src: &str) {
        // Cell index = `cell_history.len()` because the caller pushed the
        // current cell's source before invoking the wrapper pipeline.
        let cell_idx = self.cell_history.len();
        for entry in scan_top_level_lets(cell_src) {
            self.persistent_lets.push(entry.slice);
            self.persistent_let_origin.push(cell_idx);
        }
    }

    /// Drop every persistent `let` whose pattern binds a name the new cell
    /// is about to re-bind. Runs before the synthetic main is constructed
    /// so the pre-shadow binding is gone by the time the resolver sees the
    /// concatenated body. If the cell fails to evaluate, the prune is left
    /// applied — the older binding is conceptually superseded the moment
    /// the user typed the new `let` even if the cell itself errored. This
    /// matches the design.md "later cell shadows" wording at the cost of
    /// an edge case: a typo'd cell that fails type-check still drops the
    /// prior entry. Acceptable for the v1 source-replay model; users can
    /// re-bind explicitly in the next cell.
    fn prune_shadowed_lets(&mut self, cell_src: &str) {
        let mut new_names = std::collections::HashSet::new();
        for entry in scan_top_level_lets(cell_src) {
            new_names.extend(entry.names);
        }
        if new_names.is_empty() {
            return;
        }
        // Walk persistent_lets and persistent_let_origin in lockstep so
        // the parallel cell-origin tags stay aligned with the slices.
        let mut kept_lets: Vec<String> = Vec::with_capacity(self.persistent_lets.len());
        let mut kept_origin: Vec<usize> = Vec::with_capacity(self.persistent_let_origin.len());
        for (i, prior) in self.persistent_lets.iter().enumerate() {
            let prior_names = parse_let_binding_names(prior);
            if !prior_names.iter().any(|n| new_names.contains(n)) {
                kept_lets.push(prior.clone());
                kept_origin.push(*self.persistent_let_origin.get(i).unwrap_or(&0));
            }
        }
        self.persistent_lets = kept_lets;
        self.persistent_let_origin = kept_origin;
    }

    /// Drop every persistent `let` binding accumulated so far. Items
    /// (`fn` / `struct` / etc.) and cell history are left intact.
    pub fn reset_persistent_lets(&mut self) {
        self.persistent_lets.clear();
        self.persistent_let_origin.clear();
    }

    /// Type a single expression by wrapping it as `let __t = <expr>;` inside
    /// a synthetic main and reporting the inferred type of the binding.
    fn show_type(&self, expr: &str) {
        let mut s = String::new();
        s.push_str(&strip_main(&self.items_source));
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("fn main() { let __t = ");
        s.push_str(expr.trim());
        s.push_str("; }\n");

        let parsed = crate::parse(&s);
        if !parsed.errors.is_empty() {
            for e in &parsed.errors {
                eprintln!("parse error: {}", e.message);
            }
            return;
        }
        let resolved = crate::resolve(&parsed.program);
        let typed = crate::typecheck(&parsed.program, &resolved);
        if !typed.errors.is_empty() {
            // Surface resolve-side and type-side errors uniformly.
            for e in &typed.errors {
                eprintln!("type error: {}", e.message);
            }
            return;
        }

        // Find the `__t` binding's recorded type. The typechecker stores
        // expression types keyed by span; for the MVP we just print the
        // RHS expression's type by re-walking the parsed AST.
        if let Some(ty) = lookup_let_rhs_type(&parsed.program, &typed, "__t") {
            println!("{ty}");
        } else {
            eprintln!("(could not infer type — typechecker did not record one)");
        }
    }

    /// Print the union of every cell's effect set so far. v1 MVP runs the
    /// effect checker over the accumulated items + a synthetic main built
    /// from concatenating every prior statement-style cell.
    fn show_effects(&self) {
        // For the MVP, just compute effects on items_source. A richer
        // version that tracks per-cell effects belongs with the Jupyter
        // kernel CR (post-MVP).
        if self.items_source.trim().is_empty() {
            println!("(no items defined yet)");
            return;
        }
        let parsed = crate::parse(&self.items_source);
        if !parsed.errors.is_empty() {
            eprintln!("(items_source has parse errors — fix them first)");
            return;
        }
        let effects = crate::effectcheck(&parsed.program);
        // Surface every function's inferred effect set. Empty sets are
        // skipped so the output focuses on functions that actually touch
        // resources.
        let mut printed = 0usize;
        let mut entries: Vec<(&String, &crate::effectchecker::EffectSet)> =
            effects.inferred_effects.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, eff) in entries {
            if eff.effects.is_empty() {
                continue;
            }
            let mut parts: Vec<String> = eff
                .effects
                .iter()
                .map(|e| format!("{:?}", e.effect))
                .collect();
            parts.sort();
            parts.dedup();
            println!("{name}: {}", parts.join(", "));
            printed += 1;
        }
        if printed == 0 {
            println!("(no effects inferred so far)");
        }
    }

    fn save(&self, path: &str) {
        if self.cell_history.is_empty() {
            eprintln!("(no cells to save)");
            return;
        }
        let mut out = String::new();
        out.push_str("// Saved Kāra REPL session.\n\n");
        for cell in &self.cell_history {
            out.push_str(cell);
            if !cell.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        match std::fs::write(path, out) {
            Ok(()) => println!("session saved to {path}"),
            Err(e) => eprintln!("error: failed to write {path}: {e}"),
        }
    }

    /// Handle a `:dep <name> = <spec>` line. Parses the right-hand side
    /// against the same shape `[dependencies]` accepts in `kara.toml` —
    /// either a bare semver string (`http = "1.2"`) or an inline table
    /// (`{ git = "..." }` / `{ path = "..." }` / `{ version = "..." }`).
    /// Records the request in `pending_deps` and prints a confirmation
    /// plus a v1.1 caveat: the package manager is not yet wired, so the
    /// surface symbols are NOT yet in scope. When the resolver lands the
    /// stored map is what it consumes.
    fn add_dep(&mut self, rest: &str) {
        // Wrap the user's `name = value` pair as a one-table TOML snippet
        // and lean on `toml::Table` to do the heavy lifting (string
        // escaping, inline-table parsing, etc.). The synthetic table name
        // is unreachable from user input.
        let snippet = format!("[__kara_repl_dep__]\n{rest}\n");
        let table: toml::Table = match snippet.parse() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: invalid `:dep` syntax — {}", e.message());
                eprintln!(
                    "  expected: :dep <name> = \"version\"  or  :dep <name> = {{ git = \"...\" }}"
                );
                return;
            }
        };
        let inner = match table.get("__kara_repl_dep__").and_then(|v| v.as_table()) {
            Some(t) => t,
            None => {
                eprintln!("error: `:dep` argument must be a single `name = value` pair");
                return;
            }
        };
        if inner.len() != 1 {
            eprintln!(
                "error: `:dep` accepts exactly one dependency per command (got {})",
                inner.len()
            );
            return;
        }
        let (name, value) = inner.iter().next().expect("len == 1 just checked");
        if name.is_empty() {
            eprintln!("error: `:dep` package name cannot be empty");
            return;
        }
        // For inline-table forms (`{ git = "..." }`) reuse the user's
        // original RHS slice verbatim — `toml::Value` doesn't impl Display
        // and pulling in the toml serializer just to round-trip one value
        // would be excessive. For bare strings, normalize through a tiny
        // TOML escaper so the stored form is independent of the user's
        // whitespace / quote choice.
        let normalized = match value {
            toml::Value::String(s) => format!("\"{}\"", toml_escape_string(s)),
            toml::Value::Table(_) => match rest.split_once('=') {
                Some((_, rhs)) => rhs.trim().to_string(),
                None => {
                    eprintln!("error: `:dep` syntax must include `=` between name and value");
                    return;
                }
            },
            other => {
                eprintln!(
                    "error: `:dep {name} = ...` must be a version string or an inline table (got {})",
                    other.type_str()
                );
                return;
            }
        };
        let updated = self.pending_deps.insert(name.clone(), normalized.clone());
        let verb = if updated.is_some() {
            "updated"
        } else {
            "registered"
        };
        println!("(:dep {verb}: {name} = {normalized})");
        println!(
            "  note: package resolution / download lands in v1.1 — the entry is recorded but `{name}`'s symbols are not yet in scope."
        );
    }
}

/// Rewrite the consume-site of `binding` inside a cell-history entry so
/// `:save` exports the auto-clone'd form. The rewrite is positional: find
/// the first occurrence of the bare identifier `binding` whose immediate
/// suffix is *not* already `.clone()` (idempotent w.r.t. repeat
/// rewrites — the second auto-clone for the same site is a no-op), and
/// splice `.clone()` after it. Matches identifier boundaries so that
/// substring overlap (e.g. `binding="s"` against `consumed`) doesn't trip
/// the search. Idempotent under repeat invocations: a cell that already
/// reads `consume(s.clone())` is returned unchanged.
fn rewrite_cell_history_consume(cell_src: &mut String, binding: &str) {
    if binding.is_empty() {
        return;
    }
    let bytes = cell_src.as_bytes();
    let blen = binding.len();
    let mut i = 0usize;
    while i + blen <= bytes.len() {
        // Skip past string-literal contents — a binding name embedded in
        // a string isn't a use-site. Cheap coarse-tracking matches the
        // probe in `is_balanced` (good enough for the v1 source-replay
        // model's input).
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i = i.saturating_add(1);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        // Identifier-boundary match: the byte before `binding` must not
        // be an identifier continuation; ditto the byte after.
        let prev_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
        let after = i + blen;
        let next_ok = after >= bytes.len() || !is_ident_continue(bytes[after]);
        if prev_ok && next_ok && &bytes[i..after] == binding.as_bytes() {
            // Already-cloned guard — skip if `.clone(` immediately
            // follows the identifier.
            let already_cloned = bytes[after..].starts_with(b".clone(");
            if !already_cloned {
                cell_src.insert_str(after, ".clone()");
                return;
            }
            i = after + ".clone()".len();
            continue;
        }
        i += 1;
    }
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// One-line preview of a cell's source for the notebook-aware
/// `UseAfterMove` tail. Trims whitespace, collapses interior newlines to
/// spaces, and truncates with an ellipsis past `MAX` chars so a long
/// multi-line cell doesn't blow out the diagnostic. Returned form is safe
/// to embed in backticks: any embedded backticks are replaced with single
/// quotes so the surrounding code-fence stays well-formed.
fn cell_preview(src: &str) -> String {
    const MAX: usize = 60;
    let mut out = String::with_capacity(src.len().min(MAX + 1));
    let mut last_was_space = true;
    for ch in src.chars() {
        let mapped = match ch {
            '\n' | '\r' | '\t' => ' ',
            '`' => '\'',
            c => c,
        };
        if mapped == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        if out.chars().count() >= MAX {
            out.push('…');
            break;
        }
        out.push(mapped);
    }
    let trimmed = out.trim().to_string();
    trimmed
}

/// Minimal TOML basic-string escaper. Only the characters that need
/// escaping inside a `"..."` literal are touched; everything else passes
/// through. Used to round-trip a `:dep` version string through the stored
/// representation without depending on `toml`'s serializer.
fn toml_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                write!(out, "\\u{:04X}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out
}

fn print_help() {
    println!(
        "Kāra REPL meta-commands:
  :help              show this help
  :quit / :q         exit the REPL
  :type <expr>       print the inferred type of <expr>
  :effects           print effects accumulated by session items
  :save <file.kara>  write the cell history to <file.kara>
  :reset             clear persistent `let` bindings (items + history kept)
  :dep <name> = ...  register a dependency for the current session
                     (RHS is the same shape as `[dependencies]` in
                     `kara.toml`: a quoted version string or an inline
                     table). Resolution lands in v1.1; v1 records the
                     entry but the package's symbols are NOT yet in scope.

Cell semantics (v1 MVP):
  - Top-level items (fn, struct, enum, trait, impl, const, type, distinct)
    accumulate across cells. A later definition shadows an earlier one with
    the same name.
  - Each statement-style cell runs as the body of an implicit `fn main()`,
    so `?` is legal.
  - `let` / `let mut` bindings persist across statement cells: cell N's
    `let x = 5;` is in scope when cell N+1 evaluates `println(x);`. v1
    uses source-replay — the RHS re-runs each cell — so side-effecting
    bindings (`let log = read_file(...)`) recompute every time. Use
    `:reset` to drop the replay buffer if it gets expensive.
  - Mutation in a statement cell (`x += 1;`) does NOT survive to the
    next cell. Re-bind with `let x = x + 1;` to thread updated values.
  - Re-declaring an item in a later cell (`fn f` in cell 5 after `fn f`
    in cell 2; same for struct / enum / trait / const / type alias /
    distinct type / layout / effect group) shadows the earlier
    definition — the prior decl is pruned from the items buffer before
    the new one is appended. `impl` blocks are anonymous and are never
    pruned (multiple impls for the same target type compose as expected)."
    );
}

/// Cell input shape. The parser only accepts top-level items, so any
/// cell that begins with a statement (raw expression, `let`, etc.) needs
/// the synthetic `fn main()` wrapper to parse at all.
#[derive(Debug, PartialEq, Eq)]
enum CellShape {
    /// Every non-blank, non-comment line begins with one of the item
    /// keywords (`fn`, `pub`, `private`, `struct`, `enum`, `trait`,
    /// `impl`, `const`, `type`, `distinct`, `extern`, `use`, `import`,
    /// `layout`, `effect`, `mod`, `#[...]` attribute prefix).
    PureItems,
    /// Any other shape — bare expressions, `let`s, `return`s, mixed
    /// statements + items. Routed through the wrapper.
    Statements,
}

fn classify_input(src: &str) -> CellShape {
    // Item-starter keywords. `pub` / `private` are visibility modifiers
    // that always precede an item. `#` introduces an attribute that
    // attaches to the next item.
    const ITEM_PREFIXES: &[&str] = &[
        "fn ",
        "fn(",
        "pub ",
        "private ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "impl<",
        "impl[",
        "const ",
        "type ",
        "distinct ",
        "extern ",
        "use ",
        "import ",
        "layout ",
        "effect ",
        "mod ",
        "#",
    ];
    for raw_line in src.lines() {
        let line = raw_line.trim_start();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("//") || line.starts_with("/*") {
            continue;
        }
        // Continuation of a prior item's body — `}` closing a brace block
        // is permitted in pure-items mode. Same for trailing `,` etc. We
        // approximate by treating any line that is purely closing punctuation
        // or that begins with a bracket-closer as item-continuation.
        if line.starts_with('}') || line.starts_with(')') || line.starts_with(']') {
            continue;
        }
        // Field / variant / arm bodies inside an item — punted to the
        // pessimistic side: any line we can't classify as item-prefixed
        // forces statements mode. To keep the heuristic tight enough that
        // real item bodies aren't misclassified, we additionally accept
        // lines that look like field declarations (`name: Type,`) and
        // variant declarations (`Name,` / `Name { ... },`).
        if ITEM_PREFIXES.iter().any(|p| line.starts_with(p)) {
            continue;
        }
        if looks_like_item_body_line(line) {
            continue;
        }
        return CellShape::Statements;
    }
    // All non-blank lines were item-shaped (or item-body lines).
    CellShape::PureItems
}

/// Heuristic: a line that lives inside an item body but doesn't itself
/// start an item. Recognizes struct field declarations, enum variants,
/// trait method headers, and so on. Conservative — when in doubt, returns
/// `false` and the cell falls into statements mode.
fn looks_like_item_body_line(line: &str) -> bool {
    // "name: Type," / "name: Type" — struct field.
    // "Name," / "Name { ... }," / "Name(...)," — enum variant.
    // "fn name(...) -> ...;" — trait method header (no body).
    // We accept any line that contains a `:` before any `=` (rules out
    // `let x: T = 5;`-style statements; `let` statements always start with
    // `let`, which `classify_input` already rejects).
    if line.starts_with("let ") || line.starts_with("return ") {
        return false;
    }
    // Pure punctuation / comment trailers.
    if line
        .chars()
        .all(|c| c.is_whitespace() || c == ',' || c == ';' || c == '{' || c == '}')
    {
        return true;
    }
    // Field / variant / where-clause / attribute body line — accept if
    // the line is identifier-ish followed by `:` or comma-separated.
    if line.contains(':') && !line.contains("=") {
        return true;
    }
    // Bare variant name (possibly with payload).
    let first = line
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next();
    if let Some(name) = first {
        if let Some(c) = name.chars().next() {
            // Heuristic: identifiers starting uppercase look like enum
            // variants when they appear at body-line position.
            if c.is_ascii_uppercase() {
                return true;
            }
        }
    }
    false
}

/// Extract the bare name of a named top-level item. Anonymous items
/// (`impl`, `use`, `import`, `independent`) return `None` — they have no
/// shadowable identity, so the prune loop should leave them alone.
fn item_name(item: &crate::ast::Item) -> Option<&str> {
    use crate::ast::Item;
    match item {
        Item::Function(f) => Some(&f.name),
        Item::StructDef(s) => Some(&s.name),
        Item::EnumDef(e) => Some(&e.name),
        Item::TraitDef(t) => Some(&t.name),
        Item::TraitAlias(t) => Some(&t.name),
        Item::MarkerTrait(t) => Some(&t.name),
        Item::ConstDecl(c) => Some(&c.name),
        Item::ExternFunction(f) => Some(&f.name),
        Item::TypeAlias(t) => Some(&t.name),
        Item::DistinctType(d) => Some(&d.name),
        Item::LayoutDef(l) => Some(&l.name),
        Item::EffectResource(r) => Some(&r.name),
        Item::EffectGroup(g) => Some(&g.name),
        Item::EffectVerbDecl(v) => Some(&v.verb_name),
        // `AliasDecl` is an alias *declaration* (`alias L = R`), not a
        // type alias — its identity is the LHS name pair, not a single
        // shadowable identifier. Treat as anonymous for shadow purposes.
        // `ExternBlock` groups N items under one trust-boundary header;
        // there is no single shadowable identifier — treat as anonymous.
        Item::ImplBlock(_)
        | Item::UseDecl(_)
        | Item::Import(_)
        | Item::IndependentDecl(_)
        | Item::ExternBlock(_)
        | Item::AliasDecl(_) => None,
    }
}

/// Recover the parser-recorded span for any item kind so the prune loop
/// can identify byte ranges in the source buffer. The span starts after
/// any doc comments / attributes / visibility keyword (the parser eats
/// those before recording `start = current_span()`); the prune loop
/// recovers the leading bytes by attaching them to the *next* item.
fn item_span(item: &crate::ast::Item) -> &crate::token::Span {
    use crate::ast::Item;
    match item {
        Item::Function(f) => &f.span,
        Item::StructDef(s) => &s.span,
        Item::EnumDef(e) => &e.span,
        Item::TraitDef(t) => &t.span,
        Item::TraitAlias(t) => &t.span,
        Item::MarkerTrait(t) => &t.span,
        Item::ConstDecl(c) => &c.span,
        Item::ExternFunction(f) => &f.span,
        Item::TypeAlias(t) => &t.span,
        Item::DistinctType(d) => &d.span,
        Item::LayoutDef(l) => &l.span,
        Item::EffectResource(r) => &r.span,
        Item::EffectGroup(g) => &g.span,
        Item::EffectVerbDecl(v) => &v.span,
        Item::AliasDecl(a) => &a.span,
        Item::ImplBlock(b) => &b.span,
        Item::UseDecl(u) => &u.span,
        Item::Import(i) => &i.span,
        Item::IndependentDecl(d) => &d.span,
        Item::ExternBlock(b) => &b.span,
    }
}

/// Parse a snippet of top-level item source and return the set of names
/// it declares (functions, structs, enums, traits, consts, type aliases,
/// distinct newtypes, layouts, effect groups/resources/verbs, aliases).
/// Anonymous items contribute nothing. Empty set if the snippet does not
/// parse — the production pipeline will surface that error elsewhere.
fn collect_top_level_item_names(src: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let parsed = crate::parse(src);
    if !parsed.errors.is_empty() {
        return names;
    }
    for item in &parsed.program.items {
        if let Some(n) = item_name(item) {
            names.insert(n.to_string());
        }
    }
    names
}

/// One captured `let` source slice plus the names it binds.
struct CellLet {
    slice: String,
    names: std::collections::HashSet<String>,
}

/// Parse a cell body inside a synthetic `fn __cell()` wrapper and return
/// each top-level `let` / `let ... else` statement: its raw source slice
/// plus the names its pattern binds. Returns an empty list if the cell
/// fails to parse (the production pipeline will surface that error
/// elsewhere; this helper just declines to capture).
fn scan_top_level_lets(cell_src: &str) -> Vec<CellLet> {
    use crate::ast::{Item, StmtKind};
    let mut wrapper = String::with_capacity(cell_src.len() + 24);
    wrapper.push_str("fn __cell() {\n");
    let body_offset = wrapper.len();
    wrapper.push_str(cell_src);
    if !cell_src.ends_with('\n') {
        wrapper.push('\n');
    }
    wrapper.push_str("}\n");
    let parsed = crate::parse(&wrapper);
    let mut out = Vec::new();
    if !parsed.errors.is_empty() {
        return out;
    }
    for item in &parsed.program.items {
        let Item::Function(f) = item else { continue };
        if f.name != "__cell" {
            continue;
        }
        for stmt in &f.body.stmts {
            let pattern = match &stmt.kind {
                StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => pattern,
                _ => continue,
            };
            let start = stmt.span.offset;
            let end = start.saturating_add(stmt.span.length);
            if start < body_offset || end > wrapper.len() || start >= end {
                continue;
            }
            let mut names = std::collections::HashSet::new();
            collect_pattern_bindings(&pattern.kind, &mut names);
            out.push(CellLet {
                slice: wrapper[start..end].to_string(),
                names,
            });
        }
        break;
    }
    out
}

/// Walk a pattern, collecting every value-binding name. Used to drop
/// prior persistent `let`s when a new cell shadows the same name.
fn collect_pattern_bindings(
    pat: &crate::ast::PatternKind,
    out: &mut std::collections::HashSet<String>,
) {
    use crate::ast::PatternKind;
    match pat {
        PatternKind::Binding(name) => {
            out.insert(name.clone());
        }
        PatternKind::Tuple(parts) => {
            for p in parts {
                collect_pattern_bindings(&p.kind, out);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for f in fields {
                if let Some(ref p) = f.pattern {
                    collect_pattern_bindings(&p.kind, out);
                } else {
                    // Shorthand `Foo { x }` binds `x` directly.
                    out.insert(f.name.clone());
                }
            }
        }
        PatternKind::TupleVariant { patterns, .. } => {
            for p in patterns {
                collect_pattern_bindings(&p.kind, out);
            }
        }
        PatternKind::AtBinding { name, pattern } => {
            out.insert(name.clone());
            collect_pattern_bindings(&pattern.kind, out);
        }
        PatternKind::Or(alternatives) => {
            // Every alternative binds the same set of names per the
            // language's or-pattern rule; pick the first.
            if let Some(first) = alternatives.first() {
                collect_pattern_bindings(&first.kind, out);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for p in prefix.iter().chain(suffix.iter()) {
                collect_pattern_bindings(&p.kind, out);
            }
            if let Some(crate::ast::RestPattern::Bound(name)) = rest {
                out.insert(name.clone());
            }
        }
        // Leaves: no bindings.
        PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
    }
}

/// Re-parse a stored `let` source slice to recover the names it binds.
/// Used for shadow-pruning — when a new cell binds `x`, we drop every
/// prior entry whose pattern also binds `x`.
fn parse_let_binding_names(let_src: &str) -> std::collections::HashSet<String> {
    use crate::ast::{Item, StmtKind};
    let mut wrapper = String::with_capacity(let_src.len() + 24);
    wrapper.push_str("fn __probe() {\n");
    wrapper.push_str(let_src);
    if !let_src.ends_with('\n') {
        wrapper.push('\n');
    }
    wrapper.push_str("}\n");
    let parsed = crate::parse(&wrapper);
    let mut names = std::collections::HashSet::new();
    if !parsed.errors.is_empty() {
        return names;
    }
    for item in &parsed.program.items {
        let Item::Function(f) = item else { continue };
        if f.name != "__probe" {
            continue;
        }
        for stmt in &f.body.stmts {
            let pattern = match &stmt.kind {
                StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => pattern,
                _ => continue,
            };
            collect_pattern_bindings(&pattern.kind, &mut names);
        }
    }
    names
}

fn strip_main(src: &str) -> String {
    // Remove every `fn main(...) { ... }` definition from `src` so the
    // synthetic wrapper can install its own entry point. Uses brace
    // balancing — string-literal escaping is approximate but sufficient
    // for the MVP since `items_source` only contains accepted user input.
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for the literal "fn main" preceded by start-of-input or a
        // non-identifier byte (avoid matching inside identifiers).
        let after_fn_main_starts = bytes[i..].starts_with(b"fn main")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
        if after_fn_main_starts {
            // Skip past `fn main`, then to the opening `{`.
            let mut j = i + b"fn main".len();
            while j < bytes.len() && bytes[j] != b'{' {
                j += 1;
            }
            if j == bytes.len() {
                // Malformed — fall through and copy the rest.
                out.push_str(&src[i..]);
                return out;
            }
            // Brace-balance from the opening `{`.
            let mut depth = 0i32;
            while j < bytes.len() {
                match bytes[j] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Find the static type of a `let __t = <expr>;` binding inside the
/// synthetic main. Walks the AST for the let, then looks up the RHS span
/// in `expr_types`.
fn lookup_let_rhs_type(
    program: &crate::ast::Program,
    typed: &crate::typechecker::TypeCheckResult,
    name: &str,
) -> Option<String> {
    use crate::ast::{ExprKind, Item, PatternKind, StmtKind};
    use crate::resolver::SpanKey;
    use crate::typechecker::type_display;

    for it in &program.items {
        if let Item::Function(f) = it {
            if f.name != "main" {
                continue;
            }
            for stmt in &f.body.stmts {
                if let StmtKind::Let { pattern, value, .. } = &stmt.kind {
                    let bound_name = match &pattern.kind {
                        PatternKind::Binding(n) => Some(n.as_str()),
                        _ => None,
                    };
                    if bound_name == Some(name) {
                        let key = SpanKey::from_span(&value.span);
                        if let Some(t) = typed.expr_types.get(&key) {
                            return Some(type_display(t));
                        }
                        // Fall back to the literal-int / -bool case where
                        // expr_types may not record a span for trivial RHS.
                        return match &value.kind {
                            ExprKind::Bool(_) => Some("bool".into()),
                            ExprKind::Integer(_, _) => Some("i64".into()),
                            ExprKind::Float(_, _) => Some("f64".into()),
                            ExprKind::StringLit(_) => Some("String".into()),
                            _ => None,
                        };
                    }
                }
            }
        }
    }
    None
}

/// Cheap structural-balance probe: returns `false` when the input clearly
/// has more open delimiters than close. Skips characters inside string
/// literals (`"..."`), char literals (`'.'`), and line comments (`// …`)
/// so braces inside those don't trip the multi-line probe.
fn is_balanced(src: &str) -> bool {
    let bytes = src.as_bytes();
    let mut depth_curly = 0i32;
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            b'{' => {
                depth_curly += 1;
                i += 1;
            }
            b'}' => {
                depth_curly -= 1;
                i += 1;
            }
            b'(' => {
                depth_paren += 1;
                i += 1;
            }
            b')' => {
                depth_paren -= 1;
                i += 1;
            }
            b'[' => {
                depth_brack += 1;
                i += 1;
            }
            b']' => {
                depth_brack -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    depth_curly <= 0 && depth_paren <= 0 && depth_brack <= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_simple() {
        assert!(is_balanced(""));
        assert!(is_balanced("let x = 5;"));
        assert!(is_balanced("fn f() { 1 }"));
    }

    #[test]
    fn unbalanced_open_brace() {
        assert!(!is_balanced("fn f() {"));
        assert!(!is_balanced("if true {"));
    }

    #[test]
    fn balanced_with_string_braces() {
        // Curly inside a string literal must not affect balance.
        assert!(is_balanced(r#"println("hello {world}");"#));
    }

    #[test]
    fn balanced_with_line_comment_brace() {
        assert!(is_balanced("let x = 5; // {\n"));
    }

    #[test]
    fn strip_main_removes_def() {
        let src = "struct A {}\nfn main() { let x = 1; }\nfn helper() {}\n";
        let stripped = strip_main(src);
        assert!(!stripped.contains("fn main"));
        assert!(stripped.contains("struct A"));
        assert!(stripped.contains("fn helper"));
    }

    #[test]
    fn strip_main_no_op_when_absent() {
        let src = "struct A {}\nfn helper() { 1 }\n";
        let stripped = strip_main(src);
        assert_eq!(stripped, src);
    }

    #[test]
    fn strip_main_preserves_nested_braces() {
        let src = "fn main() { if true { 1 } else { 2 } }\nfn helper() {}\n";
        let stripped = strip_main(src);
        assert!(!stripped.contains("fn main"));
        assert!(stripped.contains("fn helper"));
    }
}
