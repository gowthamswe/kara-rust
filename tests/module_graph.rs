//! Integration tests for CR-24 slice 4 — the module graph builder — and
//! slice 5 — cross-module name resolution.
//!
//! Unit tests for cycle detection on synthetic graphs live in
//! `src/module.rs#tests`. This file exercises `build_program_tree` end to end:
//! it walks a scratch project, parses every file, indexes modules by path,
//! and reports parse errors plus the slice-5 import edge set. Slice-5 tests
//! also drive `Resolver::with_tree` so `E0224` / `E0225` diagnostics and the
//! real (now non-empty) cycle detection are covered.

use karac::ast::Program;
use karac::module::{build_program_tree, BuildTreeError, ProgramTree};
use karac::resolver::{ResolveErrorKind, Resolver};
use karac::walker::{walk_project, WalkerOpts};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static TEST_ID: AtomicU32 = AtomicU32::new(0);

struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let id = TEST_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "karac-module-graph-{}-{}-{}",
            std::process::id(),
            tag,
            id,
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create scratch dir");
        ScratchDir { path }
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(&full).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        full
    }

    fn root(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn walked(root: &Path) -> karac::walker::WalkResult {
    walk_project(root, WalkerOpts::default()).expect("walker succeeds in happy-path fixtures")
}

/// Count user-authored modules, ignoring the compiler-injected `std.prelude`
/// placeholder added by CR-24 slice 8.
fn user_module_count(tree: &ProgramTree) -> usize {
    tree.modules.iter().filter(|m| !m.is_synthetic).count()
}

#[test]
fn build_tree_single_entry_module() {
    let d = ScratchDir::new("single-entry");
    d.write("src/main.kara", "fn main() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    assert_eq!(user_module_count(&built.tree), 1);
    let m = &built.tree.modules[0];
    assert!(m.path.is_empty(), "entry hoists to crate root");
    assert_eq!(m.items.len(), 1, "one fn item parsed");
    assert!(built.parse_errors.is_empty());
    assert!(built.tree.graph.edges.is_empty());
    assert_eq!(built.tree.root, m.id);
}

#[test]
fn build_tree_indexes_nested_modules_by_path() {
    let d = ScratchDir::new("nested");
    d.write("src/main.kara", "fn main() {}\n");
    d.write("src/greet.kara", "pub fn greet() {}\n");
    d.write("src/db/connection.kara", "pub fn open() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    assert_eq!(user_module_count(&built.tree), 3);
    let idx = &built.tree.graph.by_path;
    assert!(idx.contains_key::<[String]>(&[]));
    assert!(idx.contains_key::<[String]>(&["greet".to_string()]));
    assert!(idx.contains_key::<[String]>(&["db".to_string(), "connection".to_string()]));
    // Slice 5 lands the import parser; slice 4 has zero collected edges.
    assert!(built.tree.graph.edges.is_empty());
}

#[test]
fn build_tree_reports_per_file_parse_errors() {
    let d = ScratchDir::new("parse-err");
    d.write("src/main.kara", "fn main() {}\n");
    d.write("src/broken.kara", "fn oops(\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("tree still constructs around parse errors");
    assert_eq!(built.parse_errors.len(), 1);
    let pe = &built.parse_errors[0];
    assert!(pe.file.ends_with("broken.kara"));
    assert!(!pe.errors.is_empty());
    // Tree still contains both modules — parse errors are non-fatal for
    // construction so cascade reporting can proceed.
    assert_eq!(user_module_count(&built.tree), 2);
}

#[test]
fn build_tree_rejects_empty_src_dir() {
    let d = ScratchDir::new("empty-src");
    // kara.toml exists at root, src/ empty — the walker reports an empty
    // module list, and the tree builder turns that into EmptyProject.
    fs::create_dir_all(d.root().join("src")).unwrap();

    let w = walked(d.root());
    let err = build_program_tree(&w).unwrap_err();
    assert!(
        matches!(err, BuildTreeError::EmptyProject { .. }),
        "expected EmptyProject, got {err:?}",
    );
}

#[test]
fn build_tree_requires_entry_file() {
    let d = ScratchDir::new("no-entry");
    // Nested module without main.kara / lib.kara.
    d.write("src/helper.kara", "pub fn hi() {}\n");

    let w = walked(d.root());
    let err = build_program_tree(&w).unwrap_err();
    assert!(
        matches!(err, BuildTreeError::NoEntryFile { .. }),
        "expected NoEntryFile, got {err:?}",
    );
}

#[test]
fn build_tree_lib_entry_is_valid() {
    let d = ScratchDir::new("lib-entry");
    d.write("src/lib.kara", "pub fn add() {}\n");
    d.write("src/util.kara", "pub fn util() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("lib entry builds");
    // The lib entry is the crate root — its module path is empty.
    let root = &built.tree.modules[built.tree.root];
    assert!(root.path.is_empty());
    assert_eq!(user_module_count(&built.tree), 2);
}

#[test]
fn build_tree_cycle_detection_runs_without_panic_on_clean_tree() {
    let d = ScratchDir::new("no-cycles");
    d.write("src/main.kara", "fn main() {}\n");
    d.write("src/a.kara", "pub fn a() {}\n");
    d.write("src/b.kara", "pub fn b() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let cycles = karac::module::detect_cycles(&built.tree);
    assert!(cycles.is_empty(), "zero imports → no cycles");
}

// ── Slice 5: cross-module name resolution ───────────────────────

/// Resolve a module through the full tree and return its errors.
fn resolve_module_errors(
    tree: &ProgramTree,
    module_id: usize,
) -> Vec<karac::resolver::ResolveError> {
    let program = Program {
        items: tree.module(module_id).items.clone(),
        ..Program::default()
    };
    Resolver::new(&program)
        .with_tree(tree, module_id)
        .resolve()
        .errors
}

#[test]
fn slice5_import_collects_edges_and_resolves_cleanly() {
    let d = ScratchDir::new("import-ok");
    d.write("src/main.kara", "import greet.hello;\nfn main() {}\n");
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    assert!(
        built.parse_errors.is_empty(),
        "parse errors: {:?}",
        built.parse_errors
    );
    assert_eq!(
        built.tree.graph.edges.len(),
        1,
        "exactly one importer→importee edge"
    );

    for (id, _) in built.tree.modules.iter().enumerate() {
        let errs = resolve_module_errors(&built.tree, id);
        assert!(errs.is_empty(), "module {id} had resolve errors: {errs:?}");
    }
}

#[test]
fn slice5_import_unknown_module_emits_e0224_with_suggestion() {
    let d = ScratchDir::new("unknown-module");
    d.write("src/main.kara", "import greet.hello;\nfn main() {}\n");
    // Typo: `greeet` vs `greet` — should suggest "greet".
    d.write("src/typo.kara", "import greeet.hello;\npub fn other() {}\n");
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let typo_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["typo".to_string()])
        .copied()
        .expect("typo module indexed");
    let errs = resolve_module_errors(&built.tree, typo_id);
    assert_eq!(errs.len(), 1, "one resolve error; got {errs:?}");
    assert_eq!(errs[0].kind, ResolveErrorKind::UnknownModule);
    assert_eq!(
        errs[0].suggestion.as_deref(),
        Some("greet"),
        "suggestion should correct `greeet` → `greet`",
    );
}

#[test]
fn slice5_import_unknown_item_emits_e0225_with_suggestion() {
    let d = ScratchDir::new("unknown-item");
    d.write("src/main.kara", "import greet.helllo;\nfn main() {}\n");
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let main_id = built.tree.root;
    let errs = resolve_module_errors(&built.tree, main_id);
    let kind_errs: Vec<_> = errs
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::UnknownItemInModule)
        .collect();
    assert_eq!(kind_errs.len(), 1, "one E0225; got {errs:?}");
    assert_eq!(kind_errs[0].suggestion.as_deref(), Some("hello"));
}

#[test]
fn slice5_import_unknown_item_carries_machine_applicable_replacement() {
    // Round 12.28: E0225 (UnknownItemInModule) gains the same `replacement`
    // metadata the resolver already attaches to E0228 (UndefinedName) and
    // E0229 (UndefinedType). The TextEdit covers the misspelled name only
    // (item.span is just the name token, not the optional `as alias` clause),
    // so applying the edit converts `helllo` → `hello` in place. IDE
    // quick-fix UIs and `karac fix` consume this without further dispatcher
    // work.
    let main_src = "import greet.helllo;\nfn main() {}\n";
    let d = ScratchDir::new("unknown-item-replacement");
    d.write("src/main.kara", main_src);
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let main_id = built.tree.root;
    let errs = resolve_module_errors(&built.tree, main_id);
    let err = errs
        .iter()
        .find(|e| e.kind == ResolveErrorKind::UnknownItemInModule)
        .expect("E0225 not emitted");
    let edit = err
        .replacement
        .as_deref()
        .expect("E0225 should carry a TextEdit when a suggestion exists");
    assert_eq!(
        edit.replacement, "hello",
        "replacement text should equal the suggestion",
    );
    let original = &main_src[edit.offset..edit.offset + edit.length];
    assert_eq!(
        original, "helllo",
        "edit span should cover only the misspelled name, got `{original}`",
    );
    let mut rewritten = main_src.to_string();
    rewritten.replace_range(edit.offset..edit.offset + edit.length, &edit.replacement);
    assert_eq!(
        rewritten, "import greet.hello;\nfn main() {}\n",
        "applying the edit should produce the corrected source",
    );
}

#[test]
fn slice5_import_unknown_item_no_replacement_when_no_suggestion() {
    // Sentinel for the negative case: when the misspelled name is so far
    // from any candidate that `suggest_similar` returns None, the resolver
    // emits E0225 without a `replacement` payload — no machine-applicable
    // fix is offered. Pinned because the round 12.28 plumbing populates
    // `replacement` only when `suggestion.is_some()`.
    let d = ScratchDir::new("unknown-item-no-suggestion");
    d.write("src/main.kara", "import greet.zzzzzzzzzz;\nfn main() {}\n");
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    let err = errs
        .iter()
        .find(|e| e.kind == ResolveErrorKind::UnknownItemInModule)
        .expect("E0225 not emitted");
    assert!(
        err.suggestion.is_none(),
        "expected no suggestion for `zzzzzzzzzz`, got {:?}",
        err.suggestion,
    );
    assert!(
        err.replacement.is_none(),
        "expected no replacement when no suggestion is offered",
    );
}

#[test]
fn slice5_import_unknown_module_carries_machine_applicable_replacement() {
    // Round 12.29: E0223 (UnknownModule) gains the same `replacement`
    // metadata as E0225/E0228/E0229. The TextEdit covers exactly the
    // misspelled prefix tokens — `imp.path_spans` is the per-segment span
    // vector populated by the parser — so applying the edit converts
    // `greeet` → `greet` in place without disturbing the trailing item
    // name or the `as alias` clause.
    let typo_src = "import greeet.hello;\npub fn other() {}\n";
    let d = ScratchDir::new("unknown-module-replacement");
    d.write("src/main.kara", "import greet.hello;\nfn main() {}\n");
    d.write("src/typo.kara", typo_src);
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let typo_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["typo".to_string()])
        .copied()
        .expect("typo module indexed");
    let errs = resolve_module_errors(&built.tree, typo_id);
    let err = errs
        .iter()
        .find(|e| e.kind == ResolveErrorKind::UnknownModule)
        .expect("E0223 not emitted");
    let edit = err
        .replacement
        .as_deref()
        .expect("E0223 should carry a TextEdit when a suggestion exists");
    assert_eq!(
        edit.replacement, "greet",
        "replacement text should equal the suggestion",
    );
    let original = &typo_src[edit.offset..edit.offset + edit.length];
    assert_eq!(
        original, "greeet",
        "edit span should cover only the misspelled prefix, got `{original}`",
    );
    let mut rewritten = typo_src.to_string();
    rewritten.replace_range(edit.offset..edit.offset + edit.length, &edit.replacement);
    assert_eq!(
        rewritten, "import greet.hello;\npub fn other() {}\n",
        "applying the edit should produce the corrected source",
    );
}

#[test]
fn slice5_import_unknown_module_multi_segment_replacement_covers_full_prefix() {
    // Multi-segment prefix variant — when the misspelled prefix spans more
    // than one dotted segment (e.g., `grret.helpers` where `greet.helpers`
    // exists), the replacement covers the contiguous prefix range from the
    // first segment's offset to the last segment's end. The trailing item
    // name (`do_thing`) and surrounding whitespace stay untouched.
    let typo_src = "import grret.helpers.do_thing;\npub fn other() {}\n";
    let d = ScratchDir::new("unknown-module-multi-segment");
    d.write("src/main.kara", "fn main() {}\n");
    d.write("src/typo.kara", typo_src);
    d.write("src/greet.kara", "pub fn placeholder() {}\n");
    d.write("src/greet/helpers.kara", "pub fn do_thing() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let typo_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["typo".to_string()])
        .copied()
        .expect("typo module indexed");
    let errs = resolve_module_errors(&built.tree, typo_id);
    let err = errs
        .iter()
        .find(|e| e.kind == ResolveErrorKind::UnknownModule)
        .expect("E0223 not emitted");
    let edit = err
        .replacement
        .as_deref()
        .expect("E0223 should carry a TextEdit when a suggestion exists");
    assert_eq!(edit.replacement, "greet.helpers");
    let original = &typo_src[edit.offset..edit.offset + edit.length];
    assert_eq!(
        original, "grret.helpers",
        "edit span should cover the full misspelled prefix, got `{original}`",
    );
    let mut rewritten = typo_src.to_string();
    rewritten.replace_range(edit.offset..edit.offset + edit.length, &edit.replacement);
    assert_eq!(
        rewritten,
        "import greet.helpers.do_thing;\npub fn other() {}\n",
    );
}

#[test]
fn slice5_import_unknown_module_no_replacement_when_no_suggestion() {
    // Negative-case sentinel: a far-off prefix that `suggest_similar` cannot
    // match returns `replacement: None` alongside `suggestion: None` —
    // mirrors the round-12.28 negative-case sentinel for E0225.
    let d = ScratchDir::new("unknown-module-no-suggestion");
    d.write("src/main.kara", "fn main() {}\n");
    d.write(
        "src/typo.kara",
        "import zzzzzzzzzz.hello;\npub fn other() {}\n",
    );
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let typo_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["typo".to_string()])
        .copied()
        .expect("typo module indexed");
    let errs = resolve_module_errors(&built.tree, typo_id);
    let err = errs
        .iter()
        .find(|e| e.kind == ResolveErrorKind::UnknownModule)
        .expect("E0223 not emitted");
    assert!(
        err.suggestion.is_none(),
        "expected no suggestion for `zzzzzzzzzz`, got {:?}",
        err.suggestion,
    );
    assert!(
        err.replacement.is_none(),
        "expected no replacement when no suggestion is offered",
    );
}

#[test]
fn slice5_import_alias_rename_binds_under_alias() {
    let d = ScratchDir::new("alias-rename");
    d.write(
        "src/main.kara",
        "import greet.hello as greeter;\nfn main() {}\n",
    );
    d.write("src/greet.kara", "pub fn hello() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(errs.is_empty(), "alias import should resolve: {errs:?}");
}

#[test]
fn slice5_brace_grouped_import_binds_each_item() {
    let d = ScratchDir::new("brace-group");
    d.write(
        "src/main.kara",
        "import greet.{hello, goodbye as bye};\nfn main() {}\n",
    );
    d.write("src/greet.kara", "pub fn hello() {}\npub fn goodbye() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(errs.is_empty(), "brace group should resolve: {errs:?}");
}

#[test]
fn slice5_cycle_detection_fires_on_real_imports() {
    let d = ScratchDir::new("real-cycle");
    d.write("src/main.kara", "import a.thing;\nfn main() {}\n");
    d.write("src/a.kara", "import b.other;\npub fn thing() {}\n");
    d.write("src/b.kara", "import a.thing;\npub fn other() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let cycles = karac::module::detect_cycles(&built.tree);
    assert!(
        !cycles.is_empty(),
        "slice 5 imports should produce at least one cycle",
    );
}

#[test]
fn slice5_import_submodule_binds_as_module_reference() {
    // `import db.connection;` binds `connection` as a module handle when
    // `db.connection` is itself a module (design.md § Binding rule).
    let d = ScratchDir::new("module-binding");
    d.write("src/main.kara", "import db.connection;\nfn main() {}\n");
    d.write("src/db.kara", "pub fn open() {}\n");
    d.write("src/db/connection.kara", "pub fn connect() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "module-handle import should resolve: {errs:?}"
    );
}

#[test]
fn slice5_pub_import_parses_as_reexport() {
    // `pub import` is the slice-7 re-export syntax — slice 5 just parses it
    // and registers the symbol as public. Full re-export semantics land in
    // slice 7.
    let d = ScratchDir::new("pub-import");
    d.write("src/main.kara", "import mid.thing;\nfn main() {}\n");
    // Paths are always absolute from the crate root — `mid` re-exports by
    // reaching into `mid.inner.thing` with the full path.
    d.write("src/mid.kara", "pub import mid.inner.thing;\n");
    d.write("src/mid/inner.kara", "pub fn thing() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let mid_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["mid".to_string()])
        .copied()
        .expect("mid module indexed");
    let errs = resolve_module_errors(&built.tree, mid_id);
    assert!(errs.is_empty(), "pub import should resolve: {errs:?}");
}

// ── Slice 6: cross-module visibility ────────────────────────────

#[test]
fn slice6_pub_item_is_importable_across_directories() {
    let d = ScratchDir::new("pub-ok");
    d.write(
        "src/main.kara",
        "import db.connection.open;\nfn main() {}\n",
    );
    d.write("src/db/connection.kara", "pub fn open() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(errs.is_empty(), "pub is always reachable: {errs:?}");
}

#[test]
fn slice6_private_item_accessible_from_same_directory() {
    // `helper` lives in `src/db/helper.kara` and is `private`. `connection`
    // lives in `src/db/connection.kara` (same directory) so the import
    // resolves cleanly.
    let d = ScratchDir::new("private-same-dir");
    d.write("src/main.kara", "fn main() {}\n");
    d.write(
        "src/db/connection.kara",
        "import db.helper.do_thing;\npub fn open() {}\n",
    );
    d.write("src/db/helper.kara", "private fn do_thing() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let connection_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["db".to_string(), "connection".to_string()])
        .copied()
        .expect("connection indexed");
    let errs = resolve_module_errors(&built.tree, connection_id);
    assert!(
        errs.is_empty(),
        "same-directory private is reachable: {errs:?}",
    );
}

#[test]
fn slice6_private_item_rejected_across_directories() {
    // `helper` is `private` and lives in `src/db/`. `main.kara` lives in
    // `src/`, a different directory — import should trip E0222.
    let d = ScratchDir::new("private-cross-dir");
    d.write("src/main.kara", "import db.helper.secret;\nfn main() {}\n");
    d.write("src/db/helper.kara", "private fn secret() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    let priv_errs: Vec<_> = errs
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::PrivateItemAccess)
        .collect();
    assert_eq!(priv_errs.len(), 1, "exactly one E0222, got {errs:?}",);
    let msg = &priv_errs[0].message;
    assert!(
        msg.contains("private"),
        "E0222 message should mention private visibility, got: {msg}",
    );
}

#[test]
fn slice6_default_visibility_reachable_from_other_directory() {
    // No keyword → Default (project-internal). In v1 single-package mode,
    // this is always visible within the project.
    let d = ScratchDir::new("default-cross-dir");
    d.write(
        "src/main.kara",
        "import db.helper.helper_fn;\nfn main() {}\n",
    );
    d.write("src/db/helper.kara", "fn helper_fn() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "default visibility should be project-internal, got: {errs:?}",
    );
}

#[test]
fn slice6_private_struct_rejected_cross_directory() {
    // E0222 fires on any item kind, not just fns.
    let d = ScratchDir::new("private-struct");
    d.write("src/main.kara", "import db.schema.User;\nfn main() {}\n");
    d.write("src/db/schema.kara", "private struct User {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.iter()
            .any(|e| e.kind == ResolveErrorKind::PrivateItemAccess),
        "expected E0222 for private struct, got {errs:?}",
    );
}

#[test]
fn slice6_mixing_pub_and_private_is_parse_error() {
    let d = ScratchDir::new("mixed-vis");
    d.write("src/main.kara", "pub private fn bad() {}\nfn main() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    assert!(
        !built.parse_errors.is_empty(),
        "`pub private` should be a parse error",
    );
}

// ── Slice 6 follow-up: typechecker cross-module checks ──────────

use karac::typechecker::{TypeChecker, TypeErrorKind};

fn typecheck_module_errors(
    tree: &ProgramTree,
    module_id: usize,
) -> Vec<karac::typechecker::TypeError> {
    let program = Program {
        items: tree.module(module_id).items.clone(),
        ..Program::default()
    };
    let resolved = Resolver::new(&program).with_tree(tree, module_id).resolve();
    TypeChecker::new(&program, &resolved)
        .with_tree(tree, module_id)
        .check()
        .errors
}

#[test]
fn slice6b_pub_signature_with_imported_non_pub_type_trips_e0221() {
    // `pub fn open() -> Connection` in `main` leaks a non-pub imported
    // type. The imported `Connection` is declared without `pub` in
    // `db.kara`, so it has `Default` visibility — project-internal, not
    // part of the package's public API.
    let d = ScratchDir::new("e0221-cross-module");
    d.write(
        "src/main.kara",
        "import db.Connection;\npub fn open() -> Connection { todo() }\nfn main() {}\n",
    );
    d.write("src/db.kara", "struct Connection {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = typecheck_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::PrivateTypeInPublicSignature),
        "expected cross-module E0221 for non-pub imported type, got {errs:?}",
    );
}

#[test]
fn slice6b_pub_signature_with_imported_pub_type_is_ok() {
    let d = ScratchDir::new("e0221-pub-ok");
    d.write(
        "src/main.kara",
        "import db.Connection;\npub fn open() -> Connection { todo() }\nfn main() {}\n",
    );
    d.write("src/db.kara", "pub struct Connection {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = typecheck_module_errors(&built.tree, built.tree.root);
    assert!(
        !errs
            .iter()
            .any(|e| e.kind == TypeErrorKind::PrivateTypeInPublicSignature),
        "pub imported type should NOT trip E0221, got {errs:?}",
    );
}

#[test]
fn slice6b_cross_module_non_pub_field_access_rejected() {
    // CR-18 field-access half: `u.password_hash` from outside the defining
    // module where `password_hash` is not `pub`. Skipping the struct literal
    // keeps the test focused on the field-access rule — cross-module struct
    // construction is a separate typechecker path.
    let d = ScratchDir::new("field-access");
    d.write(
        "src/main.kara",
        "import db.User;\nfn touch(u: User) { let _x = u.password_hash; }\nfn main() {}\n",
    );
    d.write("src/db.kara", "pub struct User { password_hash: i64 }\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = typecheck_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::PrivateTypeInPublicSignature
                && e.message.contains("password_hash")),
        "expected cross-module field-access error, got {errs:?}",
    );
}

#[test]
fn slice6b_same_module_non_pub_field_access_ok() {
    // In the defining module, non-pub fields are freely accessible.
    let d = ScratchDir::new("field-access-same");
    d.write(
        "src/main.kara",
        "struct User { password_hash: i64 }\nfn touch(u: User) { let _x = u.password_hash; }\nfn main() {}\n",
    );

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = typecheck_module_errors(&built.tree, built.tree.root);
    assert!(
        !errs
            .iter()
            .any(|e| e.kind == TypeErrorKind::PrivateTypeInPublicSignature),
        "same-module field access should NOT trip, got {errs:?}",
    );
}

#[test]
fn slice6b_cross_module_pub_field_access_ok() {
    let d = ScratchDir::new("field-access-pub");
    d.write(
        "src/main.kara",
        "import db.User;\nfn touch(u: User) { let _x = u.id; }\nfn main() {}\n",
    );
    d.write("src/db.kara", "pub struct User { pub id: i64 }\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = typecheck_module_errors(&built.tree, built.tree.root);
    assert!(
        !errs
            .iter()
            .any(|e| e.kind == TypeErrorKind::PrivateTypeInPublicSignature),
        "pub field access across modules should NOT trip, got {errs:?}",
    );
}

// ── Slice 7: `pub import` re-exports ────────────────────────────

#[test]
fn slice7_pub_import_reexports_name_to_third_module() {
    // `mid` re-exports `thing` from `mid.inner`. `main` reaches `thing`
    // through the shorter `mid.thing` path.
    let d = ScratchDir::new("reexport-short-path");
    d.write("src/main.kara", "import mid.thing;\nfn main() {}\n");
    d.write("src/mid.kara", "pub import mid.inner.thing;\n");
    d.write("src/mid/inner.kara", "pub fn thing() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "re-exported name should resolve via shorter path: {errs:?}",
    );
}

#[test]
fn slice7_non_pub_import_does_not_reexport() {
    // Plain `import` (without pub) is internal — the name stays inside the
    // importer and is NOT surfaced to third modules.
    let d = ScratchDir::new("non-pub-no-reexport");
    d.write("src/main.kara", "import mid.thing;\nfn main() {}\n");
    d.write("src/mid.kara", "import mid.inner.thing;\n"); // no pub
    d.write("src/mid/inner.kara", "pub fn thing() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.iter()
            .any(|e| e.kind == ResolveErrorKind::UnknownItemInModule),
        "plain import should not re-export — expected E0225, got {errs:?}",
    );
}

#[test]
fn slice7_pub_import_chain_resolves_to_canonical() {
    // Two-hop chain: `top` re-exports from `mid`, which re-exports from
    // `mid.inner`. End consumer imports `top.thing` successfully.
    let d = ScratchDir::new("reexport-chain");
    d.write("src/main.kara", "import top.thing;\nfn main() {}\n");
    d.write("src/top.kara", "pub import mid.thing;\n");
    d.write("src/mid.kara", "pub import mid.inner.thing;\n");
    d.write("src/mid/inner.kara", "pub fn thing() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "two-hop re-export chain should resolve: {errs:?}",
    );
}

#[test]
fn slice7_pub_import_with_alias_reexports_under_alias() {
    // `pub import a.b.Long as Short;` — consumers import the module under
    // the alias.
    let d = ScratchDir::new("reexport-alias");
    d.write("src/main.kara", "import mid.Short;\nfn main() {}\n");
    d.write("src/mid.kara", "pub import mid.inner.Long as Short;\n");
    d.write("src/mid/inner.kara", "pub struct Long {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "alias re-export should resolve under alias: {errs:?}",
    );
}

#[test]
fn slice7_reexport_edges_participate_in_cycle_check() {
    // `pub import` edges count toward the module graph, so a cycle formed
    // by re-exports is rejected just like any other import cycle.
    let d = ScratchDir::new("reexport-cycle");
    d.write("src/main.kara", "fn main() {}\n");
    // `a.thing` uses `b.thing`; `b` re-exports `a.thing` — circular.
    d.write("src/a.kara", "import b.thing;\npub fn thing() {}\n");
    d.write("src/b.kara", "pub import a.thing;\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let cycles = karac::module::detect_cycles(&built.tree);
    assert!(
        !cycles.is_empty(),
        "pub-import edges should still form a cycle",
    );
}

#[test]
fn slice7_pub_import_private_across_directories_rejected() {
    // `helper` is `private` in `src/db/`; re-exporting it from `src/lib.kara`
    // (a different directory) is forbidden — the canonical check routes
    // through the original `src/db/helper.kara`, not through `lib.kara`.
    let d = ScratchDir::new("reexport-private-cross-dir");
    d.write("src/lib.kara", "pub import db.helper.secret;\n");
    d.write("src/db/helper.kara", "private fn secret() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    let priv_errs: Vec<_> = errs
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::PrivateItemAccess)
        .collect();
    assert_eq!(
        priv_errs.len(),
        1,
        "pub-import of cross-dir private item should trip E0222: {errs:?}",
    );
}

#[test]
fn slice7_consumer_cannot_reach_private_through_pub_import() {
    // Even if `mid` is adjacent to `helper` and re-exports a private item
    // with `pub import`, an external caller outside `mid`'s directory still
    // gets E0222 — re-exports preserve canonical identity, they do not
    // promote visibility past the canonical directory rule.
    let d = ScratchDir::new("reexport-preserves-private");
    d.write("src/main.kara", "import db.secret;\nfn main() {}\n");
    // `db.kara` and `db/helper.kara` share directory — `db.kara` is
    // `src/db.kara` so its directory is `src/`. `db/helper.kara`'s
    // directory is `src/db/` — different. So this re-export itself is
    // rejected at `db.kara`'s resolve; meanwhile the caller in `main.kara`
    // still sees an unreachable `secret` when visibility is checked.
    d.write("src/db.kara", "pub import db.helper.secret;\n");
    d.write("src/db/helper.kara", "private fn secret() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    // The re-export site (db.kara) itself trips E0222, confirming re-export
    // does not bypass the private-visibility rule.
    let db_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["db".to_string()])
        .copied()
        .expect("db module indexed");
    let db_errs = resolve_module_errors(&built.tree, db_id);
    assert!(
        db_errs
            .iter()
            .any(|e| e.kind == ResolveErrorKind::PrivateItemAccess),
        "re-exporter itself must have access to the item being re-exported: {db_errs:?}",
    );
}

#[test]
fn slice7_cross_module_field_access_through_reexport() {
    // `main` imports `User` from `mid` (re-exported from `db`). The
    // canonical origin is `db.kara`, so field-access checks consult the
    // struct definition there. A `pub` field should be reachable; a
    // non-`pub` field should still be rejected.
    let d = ScratchDir::new("reexport-field-access");
    d.write(
        "src/main.kara",
        "import mid.User;\nfn touch(u: User) { let _x = u.id; }\nfn main() {}\n",
    );
    d.write("src/mid.kara", "pub import db.User;\n");
    d.write("src/db.kara", "pub struct User { pub id: i64 }\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = typecheck_module_errors(&built.tree, built.tree.root);
    assert!(
        !errs
            .iter()
            .any(|e| e.kind == TypeErrorKind::PrivateTypeInPublicSignature),
        "pub field access through re-export should succeed: {errs:?}",
    );
}

#[test]
fn slice7_cross_module_non_pub_field_rejected_through_reexport() {
    // Same setup but the field isn't `pub` — canonical-identity check still
    // treats it as a cross-module private-field access.
    let d = ScratchDir::new("reexport-non-pub-field");
    d.write(
        "src/main.kara",
        "import mid.User;\nfn touch(u: User) { let _x = u.password_hash; }\nfn main() {}\n",
    );
    d.write("src/mid.kara", "pub import db.User;\n");
    d.write("src/db.kara", "pub struct User { password_hash: i64 }\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = typecheck_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::PrivateTypeInPublicSignature
                && e.message.contains("password_hash")),
        "non-pub field access through re-export should still fail: {errs:?}",
    );
}

// ── Slice 8: prelude auto-injection mechanism ──────────────────

#[test]
fn slice8_synthetic_prelude_module_is_in_program_tree() {
    // The compiler injects `std.prelude` into every project tree so that
    // import resolution treats it like any other module path.
    let d = ScratchDir::new("prelude-in-tree");
    d.write("src/main.kara", "fn main() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let prelude_path = vec!["std".to_string(), "prelude".to_string()];
    let id = built
        .tree
        .graph
        .by_path
        .get(&prelude_path)
        .copied()
        .expect("std.prelude is indexed");
    let m = built.tree.module(id);
    assert!(
        m.is_synthetic,
        "std.prelude should be flagged synthetic so per-module passes skip it",
    );
    // CR-202 slice 3c: Option is now provided by the real
    // `runtime/stdlib/option.kara` source, spliced into the prelude
    // module as an `Item::EnumDef` (its actual shape). Prior to slice 3c
    // this was a placeholder `Item::StructDef` produced by `stub_struct`.
    assert!(
        m.items
            .iter()
            .any(|i| matches!(i, karac::ast::Item::EnumDef(e) if e.name == "Option")),
        "synthetic prelude exposes Option as an EnumDef (baked source) top-level item",
    );
}

#[test]
fn slice8_explicit_import_of_prelude_type_resolves() {
    // Users never need to write this, but the path must work — it is the
    // mechanism the design.md prelude story is built on.
    let d = ScratchDir::new("explicit-prelude-import");
    d.write(
        "src/main.kara",
        "import std.prelude.Option;\nfn main() {}\n",
    );

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "import std.prelude.Option should resolve cleanly: {errs:?}",
    );
}

#[test]
fn slice8_explicit_import_with_alias_works() {
    let d = ScratchDir::new("prelude-import-alias");
    d.write(
        "src/main.kara",
        "import std.prelude.Option as Maybe;\nfn main() {}\n",
    );

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "aliased import from std.prelude should resolve cleanly: {errs:?}",
    );
}

#[test]
fn slice8_explicit_import_of_unknown_prelude_item_emits_e0225() {
    // The synthetic prelude module exposes a fixed surface — a typo or
    // unknown name produces the same E0225 with suggestion as any other
    // cross-module import.
    let d = ScratchDir::new("prelude-unknown");
    d.write(
        "src/main.kara",
        "import std.prelude.Optoin;\nfn main() {}\n",
    );

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    let e0225s: Vec<_> = errs
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::UnknownItemInModule)
        .collect();
    assert_eq!(e0225s.len(), 1, "expected one E0225, got {errs:?}");
    assert_eq!(
        e0225s[0].suggestion.as_deref(),
        Some("Option"),
        "Levenshtein should suggest `Option` for `Optoin`",
    );
}

#[test]
fn slice8_prelude_names_in_scope_without_explicit_import() {
    // The whole point of the prelude: `Option`, `Some`, `println` etc. must
    // resolve in user code without anyone writing an import. Existing
    // behaviour the synthetic-module mechanism must not regress.
    let d = ScratchDir::new("prelude-implicit");
    d.write(
        "src/main.kara",
        "fn main() { let x: Option[i32] = Some(1); println(x); }\n",
    );

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "prelude names must resolve without explicit import: {errs:?}",
    );
}

#[test]
fn slice8_synthetic_prelude_skipped_by_typecheck_pass() {
    // A regression check that the prelude module's stub items don't drown
    // the typechecker in errors — the per-module pass must skip it.
    let d = ScratchDir::new("prelude-typecheck-skip");
    d.write("src/main.kara", "fn main() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let prelude_id = built
        .tree
        .graph
        .by_path
        .get::<[String]>(&["std".to_string(), "prelude".to_string()])
        .copied()
        .expect("std.prelude indexed");
    // Sanity: the synthetic module flag is what keeps cli::typecheck_modules
    // from running over `std.prelude`. Verify the flag is set; the CLI
    // pipeline test exercises the actual skip path.
    assert!(built.tree.module(prelude_id).is_synthetic);
}

#[test]
fn slice8_prelude_does_not_participate_in_cycle_detection() {
    // The synthetic module has no imports, so it cannot create cycles —
    // verify a clean tree stays clean once the prelude is in it.
    let d = ScratchDir::new("prelude-no-cycles");
    d.write("src/main.kara", "import a.foo;\nfn main() {}\n");
    d.write("src/a.kara", "pub fn foo() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let cycles = karac::module::detect_cycles(&built.tree);
    assert!(cycles.is_empty(), "prelude must not introduce cycles");
}

#[test]
fn slice8_user_can_shadow_prelude_name() {
    // Users may define their own `Option` (or any prelude name) — this is
    // explicitly allowed per design.md. The synthetic module mechanism
    // must not turn this into a duplicate-definition error.
    let d = ScratchDir::new("prelude-shadow");
    d.write("src/main.kara", "struct Option {}\nfn main() {}\n");

    let w = walked(d.root());
    let built = build_program_tree(&w).expect("build tree");
    let errs = resolve_module_errors(&built.tree, built.tree.root);
    assert!(
        errs.is_empty(),
        "shadowing prelude `Option` must not be a resolve error: {errs:?}",
    );
}
