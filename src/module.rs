//! Module-system scaffolding and graph construction for the multi-file
//! pipeline.
//!
//! Slice 1 introduced the type shape (`Module`, `ModulePath`, `ModuleGraph`,
//! `ProgramTree`). Slice 4 adds the first real population of that shape:
//! [`build_program_tree`] consumes a [`walker::WalkResult`], parses every
//! discovered file into its own `Program`, indexes the modules by path, and
//! assembles a [`ProgramTree`]. Import edges feed a [`ModuleGraph`] that
//! [`detect_cycles`] runs Tarjan's SCC over — any SCC of size > 1 (or a
//! size-1 SCC with a self-loop) is a circular module dependency and surfaces
//! as `E0223 CircularModuleDependency`.
//!
//! Slice 5 flipped the parser to emit `Item::Import` so this module now sees
//! real import edges; unresolvable paths are still silently dropped here —
//! the per-module resolver pass owns `E0224 UnknownModule` / `E0225
//! UnknownItemInModule`.
//!
//! Shape mirrors `brainstorming/brainstorming_v41.md § C1` so the plan and
//! the code agree. See `docs/design.md § Module System` for the full design.
//!
//! `ModulePath` is a plain `Vec<String>` for v1; interning is a later
//! optimization once lookup / clone becomes a hotspot.

use crate::ast::{ExternItem, ImportDecl, Item, Program, Visibility};
use crate::parser::ParseError;
use crate::prelude;
use crate::walker::{self, WalkResult};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Handle into `ProgramTree::modules`. Cheap to copy; stable across a single
/// compilation.
pub type ModuleId = usize;

/// Dotted module path, e.g. `["db", "connection"]` for `src/db/connection.kara`.
pub type ModulePath = Vec<String>;

/// One Kāra source file, parsed but not yet resolved.
///
/// The `items` vector holds the top-level items as produced by the parser,
/// and `imports` is a side-table of `import` declarations extracted from those
/// items for faster dependency-graph construction. Keeping both lets each
/// phase iterate items directly while the module graph builder walks only the
/// imports.
#[derive(Debug, Clone)]
pub struct Module {
    pub id: ModuleId,
    pub path: ModulePath,
    pub file: PathBuf,
    pub items: Vec<Item>,
    pub imports: Vec<ImportDecl>,
    /// Joined `//!` doc-comment text at the top of the source file.
    /// `None` for files with no leading `//!` lines and for the
    /// synthetic prelude module.
    pub module_doc_comment: Option<String>,
    pub is_test_file: bool,
    /// `true` for the compiler-injected `std.prelude` placeholder added by
    /// CR-24 slice 8. Per-module passes (resolver, typechecker) skip
    /// synthetic modules so the placeholder's stub items never participate
    /// in inference; cross-module lookups continue to see them.
    pub is_synthetic: bool,
    /// Set by `build_program_tree_with(BuildTreeOpts { merge_test_companions:
    /// true, .. })`. When `Some(n)`, items[..n] originated from the
    /// production sibling and items[n..] originated from the merged test
    /// companion (`_test.kara`). When `None`, no test companion was merged.
    /// The `karac test` runner walks items[n..] for `test_*`-prefixed
    /// functions to discover tests.
    pub test_items_start: Option<usize>,
    /// Path to the `_test.kara` file when `test_items_start` is set. Test
    /// failures use this for the `location.file` field so diagnostics point
    /// at the test source, not the production sibling.
    pub test_file: Option<PathBuf>,
}

/// Directed importer → importee edges plus a path-to-id index.
///
/// Edges are collected during the file walker / resolver's first pass and
/// feed Tarjan's SCC for cycle detection (`E0223 CircularModuleDependency`).
#[derive(Debug, Clone, Default)]
pub struct ModuleGraph {
    pub edges: Vec<(ModuleId, ModuleId)>,
    pub by_path: HashMap<ModulePath, ModuleId>,
}

impl ModuleGraph {
    pub fn new() -> Self {
        ModuleGraph::default()
    }

    pub fn lookup(&self, path: &[String]) -> Option<ModuleId> {
        self.by_path.get(path).copied()
    }

    /// Append an import edge. Duplicate edges are permitted — Tarjan's SCC
    /// doesn't care, and filtering during collection would cost O(E) per
    /// insert without benefit.
    pub fn add_edge(&mut self, importer: ModuleId, importee: ModuleId) {
        self.edges.push((importer, importee));
    }
}

/// A whole Kāra project: every `.kara` file parsed into a `Module`, plus the
/// import graph that connects them. `root` is the entry module — the file
/// containing `main.kara` (binary) or `lib.kara` (library).
#[derive(Debug, Clone)]
pub struct ProgramTree {
    pub modules: Vec<Module>,
    pub root: ModuleId,
    pub graph: ModuleGraph,
}

impl ProgramTree {
    pub fn module(&self, id: ModuleId) -> &Module {
        &self.modules[id]
    }
}

// ── Parse-time errors ───────────────────────────────────────────

/// Parse errors surfaced for one specific module file. The CLI formats these
/// per-file so the user sees diagnostics grouped by source.
#[derive(Debug, Clone)]
pub struct ModuleParseErrors {
    pub file: PathBuf,
    pub errors: Vec<ParseError>,
}

/// Output of [`build_program_tree`]: the assembled tree plus any per-file
/// parse errors collected along the way. Parse errors are non-fatal for tree
/// construction — we continue building so the graph covers as much of the
/// project as possible and downstream phases can emit cleaner cascades.
#[derive(Debug)]
pub struct BuildTreeOk {
    pub tree: ProgramTree,
    pub parse_errors: Vec<ModuleParseErrors>,
}

/// Per-call options for tree construction.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildTreeOpts {
    /// When `true`, `_test.kara` files in the [`WalkResult`] are merged into
    /// their production sibling's [`Module`] entry (production items first,
    /// then test items). The merged module records `test_items_start` so the
    /// `karac test` runner can identify which top-level functions originated
    /// from the test companion. When `false` (default — used by `karac
    /// build`), test files reach this function only when the walker was
    /// invoked with `WalkerOpts::include_tests = true`; if both are false
    /// no test files appear at all.
    pub merge_test_companions: bool,
}

/// Fatal errors that stop tree construction entirely.
#[derive(Debug)]
pub enum BuildTreeError {
    /// Filesystem read failed for a specific source file.
    Io { path: PathBuf, error: String },
    /// `src/` exists but contains no compilable `.kara` files after platform
    /// / test filtering. Not buildable — there's nothing to compile.
    EmptyProject { src_dir: PathBuf },
    /// Neither `src/main.kara` nor `src/lib.kara` was found. A Kāra package
    /// must declare one entry file or the other.
    NoEntryFile { src_dir: PathBuf },
}

impl BuildTreeError {
    /// Diagnostic code, when one is assigned. Slice 4 does not reserve codes
    /// for the tree-build layer — these surface through the generic CLI
    /// bucket like walker errors do.
    pub fn code(&self) -> Option<&'static str> {
        None
    }
}

impl std::fmt::Display for BuildTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildTreeError::Io { path, error } => {
                write!(f, "cannot read `{}`: {}", path.display(), error)
            }
            BuildTreeError::EmptyProject { src_dir } => write!(
                f,
                "`{}` contains no compilable `.kara` files. Add `src/main.kara` or `src/lib.kara`.",
                src_dir.display(),
            ),
            BuildTreeError::NoEntryFile { src_dir } => write!(
                f,
                "`{}` has no entry file. A Kāra package needs either `src/main.kara` (binary) or `src/lib.kara` (library).",
                src_dir.display(),
            ),
        }
    }
}

// ── Tree construction ───────────────────────────────────────────

/// Parse every file in `walked` into its own `Program`, build the
/// [`ProgramTree`], and collect import edges into [`ModuleGraph`]. Does not
/// run [`detect_cycles`] — that's the caller's job so per-file parse errors
/// can be reported before cycle detection kicks in (a cycle report that
/// races unresolved imports reads badly).
pub fn build_program_tree(walked: &WalkResult) -> Result<BuildTreeOk, BuildTreeError> {
    build_program_tree_with(walked, BuildTreeOpts::default())
}

/// As [`build_program_tree`] but with explicit per-call options. Used by
/// `karac test` (with `merge_test_companions: true`) to fold each
/// `_test.kara` file's items into its production sibling so resolver and
/// typechecker see one logical module per directory entry.
pub fn build_program_tree_with(
    walked: &WalkResult,
    opts: BuildTreeOpts,
) -> Result<BuildTreeOk, BuildTreeError> {
    if walked.modules.is_empty() {
        return Err(BuildTreeError::EmptyProject {
            src_dir: walked.src_dir.clone(),
        });
    }
    if walked.entry == walker::EntryKind::None {
        return Err(BuildTreeError::NoEntryFile {
            src_dir: walked.src_dir.clone(),
        });
    }

    // Parse every walked file once. The parsed records buffer module data
    // before we decide whether to merge test companions into their siblings.
    struct ParsedFile {
        path: ModulePath,
        file: PathBuf,
        items: Vec<Item>,
        imports: Vec<ImportDecl>,
        module_doc_comment: Option<String>,
        role: walker::ModuleRole,
    }
    let mut parsed_files: Vec<ParsedFile> = Vec::with_capacity(walked.modules.len());
    let mut parse_errors: Vec<ModuleParseErrors> = Vec::new();
    for w in &walked.modules {
        let source = fs::read_to_string(&w.file).map_err(|e| BuildTreeError::Io {
            path: w.file.clone(),
            error: e.to_string(),
        })?;
        let parsed = crate::parse(&source);
        let imports = extract_imports(&parsed.program);
        if !parsed.errors.is_empty() {
            parse_errors.push(ModuleParseErrors {
                file: w.file.clone(),
                errors: parsed.errors.clone(),
            });
        }
        parsed_files.push(ParsedFile {
            path: w.path.clone(),
            file: w.file.clone(),
            items: parsed.program.items,
            imports,
            module_doc_comment: parsed.program.module_doc_comment,
            role: w.role,
        });
    }

    let mut modules: Vec<Module> = Vec::with_capacity(parsed_files.len());
    let mut by_path: HashMap<ModulePath, ModuleId> = HashMap::new();
    let mut root: Option<ModuleId> = None;

    if opts.merge_test_companions {
        // Group by path: at most one production file and at most one test
        // companion per path in v1. When both are present, the production
        // file's items come first and the test items are appended.
        let mut production_idx: HashMap<ModulePath, usize> = HashMap::new();
        let mut test_idx: HashMap<ModulePath, usize> = HashMap::new();
        for (i, pf) in parsed_files.iter().enumerate() {
            if pf.role == walker::ModuleRole::Test {
                test_idx.insert(pf.path.clone(), i);
            } else {
                production_idx.insert(pf.path.clone(), i);
            }
        }

        // Emit one merged Module per unique path. Iterate the original walk
        // order so module IDs stay stable for tests that depend on
        // declaration order.
        let mut emitted: HashMap<ModulePath, ()> = HashMap::new();
        for pf in &parsed_files {
            if emitted.contains_key(&pf.path) {
                continue;
            }
            emitted.insert(pf.path.clone(), ());
            let id = modules.len();

            let prod = production_idx.get(&pf.path).map(|&i| &parsed_files[i]);
            let test = test_idx.get(&pf.path).map(|&i| &parsed_files[i]);

            let (
                file,
                mut items,
                mut imports,
                module_doc_comment,
                is_entry,
                test_items_start,
                test_file,
            ) = match (prod, test) {
                (Some(p), Some(t)) => {
                    let mut items = p.items.clone();
                    let prod_count = items.len();
                    items.extend(t.items.iter().cloned());
                    let mut imports = p.imports.clone();
                    imports.extend(t.imports.iter().cloned());
                    (
                        p.file.clone(),
                        items,
                        imports,
                        p.module_doc_comment.clone(),
                        p.role == walker::ModuleRole::Entry,
                        Some(prod_count),
                        Some(t.file.clone()),
                    )
                }
                (Some(p), None) => (
                    p.file.clone(),
                    p.items.clone(),
                    p.imports.clone(),
                    p.module_doc_comment.clone(),
                    p.role == walker::ModuleRole::Entry,
                    None,
                    None,
                ),
                (None, Some(t)) => (
                    t.file.clone(),
                    t.items.clone(),
                    t.imports.clone(),
                    t.module_doc_comment.clone(),
                    false,
                    Some(0),
                    Some(t.file.clone()),
                ),
                (None, None) => unreachable!("path emitted with no parsed file"),
            };
            // Drop unused `mut` warnings.
            let _ = (&mut items, &mut imports);

            let module = Module {
                id,
                path: pf.path.clone(),
                file,
                items,
                imports,
                module_doc_comment,
                is_test_file: prod.is_none() && test.is_some(),
                is_synthetic: false,
                test_items_start,
                test_file,
            };
            if is_entry && module.path.is_empty() {
                root = Some(id);
            }
            by_path.entry(module.path.clone()).or_insert(id);
            modules.push(module);
        }
    } else {
        // Default path: one Module per parsed file, exactly as before.
        for (id, pf) in parsed_files.iter().enumerate() {
            let is_entry = pf.role == walker::ModuleRole::Entry;
            let module = Module {
                id,
                path: pf.path.clone(),
                file: pf.file.clone(),
                items: pf.items.clone(),
                imports: pf.imports.clone(),
                module_doc_comment: pf.module_doc_comment.clone(),
                is_test_file: pf.role == walker::ModuleRole::Test,
                is_synthetic: false,
                test_items_start: None,
                test_file: None,
            };
            if is_entry && module.path.is_empty() {
                root = Some(id);
            }
            by_path.entry(module.path.clone()).or_insert(id);
            modules.push(module);
        }
    }

    // CR-24 slice 8: append the synthetic `std.prelude` placeholder so that
    // `import std.prelude.X;` resolves against a real module entry. The
    // module carries stub items for every prelude name; per-module passes
    // skip it via `Module::is_synthetic`.
    let prelude_id = modules.len();
    let prelude_module = Module {
        id: prelude_id,
        path: prelude::prelude_path(),
        file: PathBuf::from("<synthetic prelude>"),
        items: prelude::synthetic_prelude_items(),
        imports: Vec::new(),
        module_doc_comment: None,
        is_test_file: false,
        is_synthetic: true,
        test_items_start: None,
        test_file: None,
    };
    by_path
        .entry(prelude_module.path.clone())
        .or_insert(prelude_id);
    modules.push(prelude_module);

    let root = root.unwrap_or(0);

    let mut graph = ModuleGraph {
        edges: Vec::new(),
        by_path,
    };

    collect_import_edges(&modules, &mut graph);

    Ok(BuildTreeOk {
        tree: ProgramTree {
            modules,
            root,
            graph,
        },
        parse_errors,
    })
}

/// Walk top-level items and gather every `import` declaration into a side
/// table. The dependency-graph builder walks only these, while every
/// downstream pass continues to iterate `Module.items` directly.
fn extract_imports(program: &Program) -> Vec<ImportDecl> {
    program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Import(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

/// Resolve each `import` to one or more module-graph edges. An import path
/// can name either a module (`import a.b.c;` where `c` is a sub-module) or
/// an item inside a module (`import a.b.c;` where `c` is a top-level item
/// in `a.b`). The resolver in slice 5 disambiguates formally; here we use
/// the longest-prefix match — the whole dotted path first, then with the
/// tail segment stripped. Paths that match nothing are dropped silently so
/// slice 5 can own `E0224`/`E0225`.
fn collect_import_edges(modules: &[Module], graph: &mut ModuleGraph) {
    for m in modules {
        for imp in &m.imports {
            for item in &imp.items {
                let mut full = imp.path.clone();
                full.push(item.name.clone());
                if let Some(&id) = graph.by_path.get(&full) {
                    graph.add_edge(m.id, id);
                } else if let Some(&id) = graph.by_path.get(&imp.path) {
                    graph.add_edge(m.id, id);
                }
            }
        }
    }
}

// ── Cycle detection ─────────────────────────────────────────────

/// A circular module dependency. `nodes` lists modules on the cycle in
/// traversal order; the final edge closes back to `nodes[0]` implicitly.
#[derive(Debug, Clone)]
pub struct Cycle {
    pub nodes: Vec<ModuleId>,
}

impl Cycle {
    /// Canonical dotted-path rendering: `a → a.b → a.b.c → a`. The closing
    /// edge is shown explicitly so the cycle reads as a loop, not a path.
    pub fn format(&self, tree: &ProgramTree) -> String {
        let mut parts: Vec<String> = self
            .nodes
            .iter()
            .map(|id| format_module_path(&tree.modules[*id].path))
            .collect();
        if let Some(first) = parts.first().cloned() {
            parts.push(first);
        }
        parts.join(" → ")
    }
}

fn format_module_path(path: &[String]) -> String {
    if path.is_empty() {
        "<crate root>".to_string()
    } else {
        path.join(".")
    }
}

/// Run Tarjan's SCC on the module graph. Each SCC of size > 1 — and each
/// size-1 SCC with a self-edge — becomes one [`Cycle`] in the result. The
/// returned slice is deterministic: cycles are sorted by their smallest
/// `ModuleId`, and within a cycle the reported node order starts at the
/// smallest `ModuleId` in the SCC and proceeds along a BFS-shortest cycle
/// from that node back to itself.
/// Topological emission order over [`ProgramTree::graph`] — dependencies
/// (importees) appear before their dependents (importers). Used by the
/// multi-file codegen path to concatenate module items in an order where
/// each module's items see only items from already-emitted modules.
///
/// Iterative DFS reverse-postorder rooted at every module (not just
/// [`ProgramTree::root`]) so disconnected modules — files in `src/`
/// that no other file imports — still appear in the output. Synthetic
/// modules are excluded; the multi-file codegen path doesn't compile
/// them.
///
/// This function assumes the graph is acyclic. Callers should run
/// [`detect_cycles`] first and abort if any cycle is reported; passing
/// a cyclic graph here produces a meaningless ordering. A debug-mode
/// assertion fires if the recursion depth grows beyond
/// `tree.modules.len()`, which can only happen on a cyclic input.
pub fn emission_order(tree: &ProgramTree) -> Vec<ModuleId> {
    let n = tree.modules.len();
    if n == 0 {
        return Vec::new();
    }
    let mut adj: Vec<Vec<ModuleId>> = vec![Vec::new(); n];
    for &(u, v) in &tree.graph.edges {
        if u < n && v < n {
            adj[u].push(v);
        }
    }
    for list in adj.iter_mut() {
        list.sort();
        list.dedup();
    }

    enum State {
        Enter(ModuleId),
        Exit(ModuleId),
    }

    let mut visited = vec![false; n];
    let mut out: Vec<ModuleId> = Vec::with_capacity(n);
    // Roots in stable order: tree.root first, then any other module not yet
    // visited (covers disconnected components and synthetic prelude).
    let mut roots: Vec<ModuleId> = Vec::with_capacity(n);
    if tree.root < n {
        roots.push(tree.root);
    }
    for id in 0..n {
        if id != tree.root {
            roots.push(id);
        }
    }

    for root in roots {
        if visited[root] {
            continue;
        }
        let mut stack: Vec<State> = vec![State::Enter(root)];
        while let Some(frame) = stack.pop() {
            match frame {
                State::Enter(v) => {
                    if visited[v] {
                        continue;
                    }
                    visited[v] = true;
                    stack.push(State::Exit(v));
                    // Push children in reverse so popping gives them in
                    // sorted order — preserves determinism across runs.
                    for &w in adj[v].iter().rev() {
                        if !visited[w] {
                            stack.push(State::Enter(w));
                        }
                    }
                }
                State::Exit(v) => {
                    if !tree.modules[v].is_synthetic {
                        out.push(v);
                    }
                }
            }
        }
    }
    out
}

pub fn detect_cycles(tree: &ProgramTree) -> Vec<Cycle> {
    let n = tree.modules.len();
    if n == 0 {
        return Vec::new();
    }

    // Build an adjacency list keyed by ModuleId (which is a 0..n index).
    let mut adj: Vec<Vec<ModuleId>> = vec![Vec::new(); n];
    for &(u, v) in &tree.graph.edges {
        if u < n && v < n {
            adj[u].push(v);
        }
    }
    // Deterministic neighbor order.
    for list in adj.iter_mut() {
        list.sort();
    }

    let sccs = tarjan_scc(&adj);

    let mut cycles: Vec<Cycle> = Vec::new();
    for scc in sccs {
        if scc.len() == 1 {
            let v = scc[0];
            if !adj[v].contains(&v) {
                continue;
            }
            cycles.push(Cycle { nodes: vec![v] });
            continue;
        }
        let nodes: std::collections::HashSet<ModuleId> = scc.iter().copied().collect();
        let start = *scc.iter().min().unwrap();
        let path = shortest_cycle_through(start, &nodes, &adj);
        cycles.push(Cycle { nodes: path });
    }

    cycles.sort_by_key(|c| *c.nodes.iter().min().unwrap_or(&0));
    cycles
}

/// Iterative Tarjan's strongly-connected-components algorithm. Returns one
/// vector per SCC; the vectors themselves are in unspecified order within
/// each SCC.
fn tarjan_scc(adj: &[Vec<ModuleId>]) -> Vec<Vec<ModuleId>> {
    let n = adj.len();
    let mut index_of: Vec<Option<usize>> = vec![None; n];
    let mut lowlink: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<ModuleId> = Vec::new();
    let mut call_stack: Vec<(ModuleId, usize)> = Vec::new();
    let mut counter: usize = 0;
    let mut sccs: Vec<Vec<ModuleId>> = Vec::new();

    for start in 0..n {
        if index_of[start].is_some() {
            continue;
        }
        index_of[start] = Some(counter);
        lowlink[start] = counter;
        counter += 1;
        stack.push(start);
        on_stack[start] = true;
        call_stack.push((start, 0));

        while let Some(&(v, i)) = call_stack.last() {
            if i < adj[v].len() {
                let w = adj[v][i];
                call_stack.last_mut().unwrap().1 += 1;
                match index_of[w] {
                    None => {
                        index_of[w] = Some(counter);
                        lowlink[w] = counter;
                        counter += 1;
                        stack.push(w);
                        on_stack[w] = true;
                        call_stack.push((w, 0));
                    }
                    Some(w_idx) if on_stack[w] && w_idx < lowlink[v] => {
                        lowlink[v] = w_idx;
                    }
                    _ => {}
                }
            } else {
                let v_idx = index_of[v].unwrap();
                if lowlink[v] == v_idx {
                    let mut scc = Vec::new();
                    while let Some(u) = stack.pop() {
                        on_stack[u] = false;
                        scc.push(u);
                        if u == v {
                            break;
                        }
                    }
                    sccs.push(scc);
                }
                let child_low = lowlink[v];
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    if child_low < lowlink[parent] {
                        lowlink[parent] = child_low;
                    }
                }
            }
        }
    }

    sccs
}

/// Given a node in an SCC, find the shortest cycle that starts and ends at
/// it using BFS restricted to SCC members. The returned vector lists the
/// cycle nodes in traversal order (not repeating the closing edge back to
/// `start`).
fn shortest_cycle_through(
    start: ModuleId,
    scc: &std::collections::HashSet<ModuleId>,
    adj: &[Vec<ModuleId>],
) -> Vec<ModuleId> {
    use std::collections::VecDeque;
    let mut parent: HashMap<ModuleId, ModuleId> = HashMap::new();
    parent.insert(start, start);
    let mut queue: VecDeque<ModuleId> = VecDeque::new();
    queue.push_back(start);

    while let Some(u) = queue.pop_front() {
        for &w in &adj[u] {
            if !scc.contains(&w) {
                continue;
            }
            if w == start && u != start {
                // Reconstruct u → ... → start via parent chain.
                let mut chain: Vec<ModuleId> = vec![u];
                let mut cur = u;
                while cur != start {
                    cur = parent[&cur];
                    chain.push(cur);
                }
                chain.reverse();
                return chain;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = parent.entry(w) {
                e.insert(u);
                queue.push_back(w);
            }
        }
    }

    // Fallback: SCC with a self-edge only — return the single node. Any
    // other shape is ruled out because `scc` has size > 1 and Tarjan's
    // guarantees every pair is mutually reachable.
    vec![start]
}

// ── Re-export chain resolution (CR-24 slice 7) ──────────────────

/// Safety cap for re-export recursion. Slice-4 cycle detection already
/// rejects circular module imports before any resolver or typechecker pass
/// runs, so a well-formed tree cannot produce a chain anywhere near this
/// depth — the cap is pure defence against malformed trees.
const REEXPORT_MAX_DEPTH: usize = 64;

/// True iff `module` defines a real top-level item named `name` (i.e., not
/// just a re-export binding of the same name). Impl blocks, layouts,
/// non-`pub` imports, `use`/`alias`/`independent` decls do not count.
fn module_defines_local_item(module: &Module, name: &str) -> bool {
    module.items.iter().any(|item| match item {
        Item::Function(f) => f.name == name,
        Item::StructDef(s) => s.name == name,
        Item::EnumDef(e) => e.name == name,
        Item::TraitDef(t) => t.name == name,
        Item::TraitAlias(t) => t.name == name,
        Item::MarkerTrait(t) => t.name == name,
        Item::ConstDecl(c) => c.name == name,
        Item::TypeAlias(t) => t.name == name,
        Item::DistinctType(d) => d.name == name,
        Item::ExternFunction(e) => e.name == name,
        Item::ExternBlock(b) => b.items.iter().any(|it| match it {
            ExternItem::Function(f) => f.name == name,
            ExternItem::OpaqueType(o) => o.name == name,
        }),
        Item::EffectResource(r) => r.name == name,
        Item::EffectGroup(g) => g.name == name,
        Item::EffectVerbDecl(v) => v.verb_name == name,
        Item::ImplBlock(_)
        | Item::LayoutDef(_)
        | Item::UseDecl(_)
        | Item::Import(_)
        | Item::AliasDecl(_)
        | Item::IndependentDecl(_) => false,
    })
}

/// True iff `name` resolves at `module_path` either directly or through a
/// `pub import` chain. Submodule re-exports are ignored — they are part of
/// the module-path tree, not the item namespace.
pub fn module_exposes_item(tree: &ProgramTree, module_path: &[String], name: &str) -> bool {
    canonical_origin(tree, module_path, name).is_some()
}

/// Resolve `name` at `module_path` to its canonical `(defining_module_path,
/// item_name)`. Walks `pub import` chains transitively.
///
/// - A real top-level item shadows any re-export with the same name.
/// - Re-exports recurse into `(imp.path, ii.name)` — the *original* name,
///   not the alias — so `pub import a.b.X as Y;` chases `a.b.X`.
/// - Submodule re-exports (`pub import db.connection;` where `db.connection`
///   is itself a module path) are not items; those names yield `None`.
/// - Cycle safety: the module-graph cycle detector rejects circular imports
///   before this runs, so chains are acyclic. Depth cap is a safety belt
///   against malformed trees (e.g., callers that skip cycle detection).
pub fn canonical_origin(
    tree: &ProgramTree,
    module_path: &[String],
    name: &str,
) -> Option<(ModulePath, String)> {
    canonical_origin_with_depth(tree, module_path, name, 0)
}

fn canonical_origin_with_depth(
    tree: &ProgramTree,
    module_path: &[String],
    name: &str,
    depth: usize,
) -> Option<(ModulePath, String)> {
    if depth > REEXPORT_MAX_DEPTH {
        return None;
    }
    let id = tree.graph.lookup(module_path)?;
    let module = tree.module(id);
    if module_defines_local_item(module, name) {
        return Some((module_path.to_vec(), name.to_string()));
    }
    for item in &module.items {
        let Item::Import(imp) = item else { continue };
        if !imp.is_pub {
            continue;
        }
        for ii in &imp.items {
            let bound = ii.alias.as_deref().unwrap_or(&ii.name);
            if bound != name {
                continue;
            }
            // Submodule re-exports are not items — skip.
            let mut full = imp.path.clone();
            full.push(ii.name.clone());
            if tree.graph.lookup(&full).is_some() {
                continue;
            }
            if let Some(found) = canonical_origin_with_depth(tree, &imp.path, &ii.name, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Resolve `name` at `module_path` to the visibility of its canonical
/// defining item, following `pub import` re-exports. Returns `None` when
/// the name does not exist (or only names a submodule re-export).
pub fn canonical_item_visibility(
    tree: &ProgramTree,
    module_path: &[String],
    name: &str,
) -> Option<Visibility> {
    let (origin_path, origin_name) = canonical_origin(tree, module_path, name)?;
    let id = tree.graph.lookup(&origin_path)?;
    let module = tree.module(id);
    for item in &module.items {
        match item {
            Item::Function(f) if f.name == origin_name => return Some(f.visibility()),
            Item::StructDef(s) if s.name == origin_name => return Some(s.visibility()),
            Item::EnumDef(e) if e.name == origin_name => return Some(e.visibility()),
            Item::TraitDef(t) if t.name == origin_name => return Some(t.visibility()),
            Item::ConstDecl(c) if c.name == origin_name => return Some(c.visibility()),
            Item::TypeAlias(t) if t.name == origin_name => return Some(t.visibility()),
            Item::DistinctType(d) if d.name == origin_name => return Some(d.visibility()),
            Item::ExternFunction(e) if e.name == origin_name => return Some(e.visibility()),
            Item::ExternBlock(b) => {
                for it in &b.items {
                    match it {
                        ExternItem::Function(f) if f.name == origin_name => {
                            return Some(f.visibility());
                        }
                        ExternItem::OpaqueType(o) if o.name == origin_name => {
                            return Some(o.visibility());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    None
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_module(id: ModuleId, path: &[&str]) -> Module {
        Module {
            id,
            path: path.iter().map(|s| s.to_string()).collect(),
            file: PathBuf::from(format!("/tmp/mod{id}.kara")),
            items: Vec::new(),
            imports: Vec::new(),
            module_doc_comment: None,
            is_test_file: false,
            is_synthetic: false,
            test_items_start: None,
            test_file: None,
        }
    }

    fn mk_tree(modules: Vec<Module>, edges: &[(ModuleId, ModuleId)]) -> ProgramTree {
        let mut by_path = HashMap::new();
        for m in &modules {
            by_path.insert(m.path.clone(), m.id);
        }
        ProgramTree {
            root: 0,
            modules,
            graph: ModuleGraph {
                edges: edges.to_vec(),
                by_path,
            },
        }
    }

    #[test]
    fn detect_cycles_empty_graph() {
        let tree = mk_tree(vec![mk_module(0, &[])], &[]);
        assert!(detect_cycles(&tree).is_empty());
    }

    #[test]
    fn detect_cycles_linear_dag_no_cycles() {
        //  0 → 1 → 2
        let tree = mk_tree(
            vec![
                mk_module(0, &["a"]),
                mk_module(1, &["b"]),
                mk_module(2, &["c"]),
            ],
            &[(0, 1), (1, 2)],
        );
        assert!(detect_cycles(&tree).is_empty());
    }

    #[test]
    fn detect_cycles_diamond_no_cycles() {
        //    0
        //   / \
        //  1   2
        //   \ /
        //    3
        let tree = mk_tree(
            vec![
                mk_module(0, &["a"]),
                mk_module(1, &["b"]),
                mk_module(2, &["c"]),
                mk_module(3, &["d"]),
            ],
            &[(0, 1), (0, 2), (1, 3), (2, 3)],
        );
        assert!(detect_cycles(&tree).is_empty());
    }

    #[test]
    fn detect_cycles_self_loop() {
        // 0 → 0
        let tree = mk_tree(vec![mk_module(0, &["a"])], &[(0, 0)]);
        let cycles = detect_cycles(&tree);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].nodes, vec![0]);
    }

    #[test]
    fn detect_cycles_two_cycle() {
        // 0 ⇄ 1
        let tree = mk_tree(
            vec![mk_module(0, &["a"]), mk_module(1, &["b"])],
            &[(0, 1), (1, 0)],
        );
        let cycles = detect_cycles(&tree);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].nodes, vec![0, 1]);
    }

    #[test]
    fn detect_cycles_three_cycle() {
        // 0 → 1 → 2 → 0
        let tree = mk_tree(
            vec![
                mk_module(0, &["a"]),
                mk_module(1, &["b"]),
                mk_module(2, &["c"]),
            ],
            &[(0, 1), (1, 2), (2, 0)],
        );
        let cycles = detect_cycles(&tree);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].nodes, vec![0, 1, 2]);
    }

    #[test]
    fn detect_cycles_chooses_smallest_starting_id() {
        //  2 → 3 → 4 → 2
        // (plus some unrelated DAG nodes 0, 1)
        let tree = mk_tree(
            vec![
                mk_module(0, &["dag0"]),
                mk_module(1, &["dag1"]),
                mk_module(2, &["a"]),
                mk_module(3, &["b"]),
                mk_module(4, &["c"]),
            ],
            &[(0, 1), (1, 2), (2, 3), (3, 4), (4, 2)],
        );
        let cycles = detect_cycles(&tree);
        assert_eq!(cycles.len(), 1);
        // Starts at 2 (the min in the SCC), traverses to 3, then 4.
        assert_eq!(cycles[0].nodes, vec![2, 3, 4]);
    }

    #[test]
    fn detect_cycles_two_independent_cycles() {
        // {0 ⇄ 1} and {2 ⇄ 3}
        let tree = mk_tree(
            vec![
                mk_module(0, &["a"]),
                mk_module(1, &["b"]),
                mk_module(2, &["c"]),
                mk_module(3, &["d"]),
            ],
            &[(0, 1), (1, 0), (2, 3), (3, 2)],
        );
        let cycles = detect_cycles(&tree);
        assert_eq!(cycles.len(), 2);
        // Sorted by min ModuleId.
        assert_eq!(cycles[0].nodes, vec![0, 1]);
        assert_eq!(cycles[1].nodes, vec![2, 3]);
    }

    #[test]
    fn detect_cycles_mixed_cycle_and_dag() {
        //  0 → 1 ⇄ 2  (1-2 is a cycle; 0 feeds it but is DAG itself)
        let tree = mk_tree(
            vec![
                mk_module(0, &["a"]),
                mk_module(1, &["b"]),
                mk_module(2, &["c"]),
            ],
            &[(0, 1), (1, 2), (2, 1)],
        );
        let cycles = detect_cycles(&tree);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].nodes, vec![1, 2]);
    }

    #[test]
    fn cycle_format_renders_dotted_paths_with_closing_edge() {
        let tree = mk_tree(
            vec![
                mk_module(0, &["db", "connection"]),
                mk_module(1, &["db", "pool"]),
            ],
            &[(0, 1), (1, 0)],
        );
        let cycles = detect_cycles(&tree);
        let rendered = cycles[0].format(&tree);
        assert_eq!(rendered, "db.connection → db.pool → db.connection");
    }

    #[test]
    fn cycle_format_crate_root_uses_placeholder() {
        let tree = mk_tree(
            vec![mk_module(0, &[]), mk_module(1, &["helper"])],
            &[(0, 1), (1, 0)],
        );
        let cycles = detect_cycles(&tree);
        let rendered = cycles[0].format(&tree);
        assert_eq!(rendered, "<crate root> → helper → <crate root>");
    }
}
