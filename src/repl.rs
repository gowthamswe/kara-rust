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

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

/// Run the interactive REPL until EOF or `:quit`. Returns once the user
/// exits; never reaches the rest of the CLI dispatch.
pub fn run() {
    let banner = "Kāra REPL  (type :help for commands, :quit to exit)";
    println!("{banner}");

    let mut session = Session::new();
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
            pending_deps: BTreeMap::new(),
        }
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
            Ok(_) => self.capture_new_lets(cell_src),
            Err(msgs) => {
                for m in msgs {
                    eprintln!("{m}");
                }
            }
        }
    }

    /// Run a wrapper cell, optionally capturing the interpreter's stdout
    /// output into a returned `Vec<String>`. `Err` carries diagnostic
    /// messages for parse/resolve/type errors so callers (tests) can
    /// surface or assert on them.
    fn run_with_wrapper_inner(
        &mut self,
        cell_src: &str,
        capture: bool,
    ) -> Result<Vec<String>, Vec<String>> {
        // Shadow-prune: drop any prior persistent let whose name(s) the new
        // cell re-binds. Kāra rejects same-scope re-declaration, so without
        // this prune `cell 1: let x = 1;` followed by `cell 2: let x = 99;`
        // would fail at the resolver inside cell 2's synthetic main. Per
        // design.md § Cell Scope, the later cell shadows the earlier
        // binding — source-replay approximates that by pruning.
        self.prune_shadowed_lets(cell_src);
        let synthetic = self.build_synthetic_cell(cell_src);
        let mut parsed = crate::parse(&synthetic);
        if !parsed.errors.is_empty() {
            return Err(parsed
                .errors
                .iter()
                .map(|e| format!("parse error: {}", e.message))
                .collect());
        }
        let resolved = crate::resolve(&parsed.program);
        if !resolved.errors.is_empty() {
            return Err(resolved
                .errors
                .iter()
                .map(|e| format!("resolve error: {}", e.message))
                .collect());
        }
        let typed = crate::typecheck(&parsed.program, &resolved);
        if !typed.errors.is_empty() {
            return Err(typed
                .errors
                .iter()
                .map(|e| format!("type error: {}", e.message))
                .collect());
        }
        crate::lower(&mut parsed.program, &typed);

        let mut interp = crate::interpreter::Interpreter::new(&parsed.program, &typed);
        if capture {
            interp.captured_output = Some(Vec::new());
        }
        interp.run();
        Ok(interp.captured_output.take().unwrap_or_default())
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
            };
        }

        // Statement / expression / mixed cell.
        match self.run_with_wrapper_inner(trimmed, /* capture */ true) {
            Ok(out) => {
                self.capture_new_lets(trimmed);
                EvaluatedCell {
                    stdout: out.join(""),
                    errors: Vec::new(),
                }
            }
            Err(msgs) => {
                // Roll back history on diagnostic-side failure.
                self.cell_history.pop();
                EvaluatedCell {
                    stdout: String::new(),
                    errors: msgs,
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
    fn build_synthetic_cell(&self, cell_src: &str) -> String {
        let mut s = String::new();
        s.push_str(&strip_main(&self.items_source));
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("fn main() {\n");
        for prior_let in &self.persistent_lets {
            s.push_str(prior_let);
            if !prior_let.ends_with('\n') {
                s.push('\n');
            }
        }
        s.push_str(cell_src);
        if !cell_src.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("}\n");
        s
    }

    /// Extract every top-level `let` / `let mut` / `let ... else` statement
    /// from the cell body and append its raw source to `persistent_lets`.
    /// Called only after the cell evaluated cleanly so failed cells never
    /// leak partial bindings forward. Shadow-pruning happened earlier in
    /// `prune_shadowed_lets` so the synthetic main could parse cleanly;
    /// at this point the new entries can simply append.
    fn capture_new_lets(&mut self, cell_src: &str) {
        for entry in scan_top_level_lets(cell_src) {
            self.persistent_lets.push(entry.slice);
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
        self.persistent_lets.retain(|prior| {
            let prior_names = parse_let_binding_names(prior);
            !prior_names.iter().any(|n| new_names.contains(n))
        });
    }

    /// Drop every persistent `let` binding accumulated so far. Items
    /// (`fn` / `struct` / etc.) and cell history are left intact.
    pub fn reset_persistent_lets(&mut self) {
        self.persistent_lets.clear();
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
        Item::ImplBlock(_)
        | Item::UseDecl(_)
        | Item::Import(_)
        | Item::IndependentDecl(_)
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
