//! Doc comment extraction + HTML rendering for `karac doc`.
//!
//! v1 MVP: walks a `ProgramTree`, collects every public-or-default item
//! that carries a `doc_comment`, and emits one HTML page per item under
//! `<output_dir>/<module_path>/<item_name>.html`. Plus a flat
//! `index.html` listing every page.
//!
//! Markdown rendering uses `pulldown-cmark` with HTML output. The
//! signature line is rendered verbatim above the prose so users see
//! `fn double(n: i64) -> i64` even before the prose explains anything.
//!
//! Deferred (follow-ups): full-site nav and `//!` module-level doc
//! comments. Cross-references (`[Vec]`-style links), per-item struct
//! field / enum variant tables, per-field / per-variant doc comments,
//! the JSON search index, and effect display in `pub fn` signatures
//! have all shipped against this skeleton.

use crate::ast::{
    EffectVerbKind, EnumDef, ExternFunction, ExternItem, Function, Item, OpaqueTypeDecl, StructDef,
    StructField, TypeExpr, Variant, VariantKind,
};
use crate::module::{ModulePath, ProgramTree};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Per-public-function effect summary used by the signature renderer.
/// Mirrors `effectchecker::DeclaredEffects` but flattened to what the
/// renderer actually needs — no traced origins, no `EffectSet` nesting,
/// no dependency on `effectchecker` types from inside `doc.rs`.
///
/// `effects` is the concrete (verb, resource) list. `polymorphic` is
/// `true` when the function carries `with _` (or `with fixed + _`); the
/// renderer appends a trailing ` + _` in that case.
#[derive(Debug, Clone, Default)]
pub struct EffectDisplay {
    pub effects: Vec<(EffectVerbKind, String)>,
    pub polymorphic: bool,
}

/// Map from `(module_path, function_name)` to its rendered effect set.
/// `cmd_doc` populates this from `EffectCheckResult` and passes it into
/// `build_docs`; `None` means "no effect display" (the MVP behavior).
pub type EffectsByItem = HashMap<(ModulePath, String), EffectDisplay>;

/// Result of a successful `karac doc` build: the list of HTML files
/// written, in source-order. Used by the CLI for the success summary
/// and by tests for assertions about output layout.
#[derive(Debug, Default)]
pub struct DocBuildResult {
    pub written: Vec<PathBuf>,
}

/// Errors `build_docs` can surface up to the CLI.
#[derive(Debug)]
pub enum DocBuildError {
    /// Failed to create an output directory.
    CreateDir { path: PathBuf, source: String },
    /// Failed to write an HTML file.
    WriteFile { path: PathBuf, source: String },
}

impl std::fmt::Display for DocBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocBuildError::CreateDir { path, source } => {
                write!(
                    f,
                    "failed to create directory {}: {}",
                    path.display(),
                    source
                )
            }
            DocBuildError::WriteFile { path, source } => {
                write!(f, "failed to write {}: {}", path.display(), source)
            }
        }
    }
}

/// Render the doc tree for `tree` into `output_dir`. Each module produces
/// one subdirectory; each documented item in that module produces one
/// HTML page. A flat `index.html` at the root lists every page.
///
/// Items without a `doc_comment` are skipped. Items in the synthetic
/// `std.prelude` module are skipped (they're compiler-internal stubs,
/// not user-facing API).
pub fn build_docs(
    tree: &ProgramTree,
    output_dir: &Path,
    effects: Option<&EffectsByItem>,
) -> Result<DocBuildResult, DocBuildError> {
    let mut result = DocBuildResult::default();

    create_dir_all(output_dir)?;

    // Build the cross-reference link table once. Maps every documentable
    // item name to its path relative to `output_dir`. Doc comments resolve
    // CommonMark `[Name]` and `[Name][]` references against this table; an
    // unresolved reference renders as plain text (no warning — too noisy
    // for in-progress projects). When two modules expose items with the
    // same name, the first one walked wins; this mirrors the index's
    // tie-breaker, which is sorted later anyway.
    let link_table = build_link_table(tree);

    // Pre-walk: collect `IndexEntry` for every documented item before
    // rendering any page. The sidebar (rendered into every page) needs
    // the full entry list, so we can't accumulate it during the write
    // loop the way the MVP did.
    let mut index_entries: Vec<IndexEntry> = Vec::new();
    for module in &tree.modules {
        if module.is_synthetic {
            continue;
        }
        for d in documentables(&module.items) {
            let Some(doc) = documentable_doc(d) else {
                continue;
            };
            let name = documentable_name(d).to_string();
            let module_dir = module_output_dir(output_dir, &module.path);
            let file_path = module_dir.join(format!("{name}.html"));
            index_entries.push(IndexEntry {
                module_path: module.path.clone(),
                item_name: name,
                kind: documentable_kind(d),
                relative_href: relative_href(&file_path, output_dir),
                summary: summarize_doc(doc),
            });
        }
    }

    // Build the per-module doc lookup. Synthetic modules (e.g. the
    // prelude stub) are excluded — they don't have user-authored `//!`
    // doc and shouldn't show up in navigation.
    let module_docs: Vec<(ModulePath, Option<String>)> = tree
        .modules
        .iter()
        .filter(|m| !m.is_synthetic)
        .map(|m| (m.path.clone(), m.module_doc_comment.clone()))
        .collect();

    // Render pass: write item pages with the global sidebar.
    for module in &tree.modules {
        if module.is_synthetic {
            continue;
        }
        let module_dir = module_output_dir(output_dir, &module.path);
        let mut module_had_docs = false;

        for d in documentables(&module.items) {
            let Some(doc) = documentable_doc(d) else {
                continue;
            };
            let name = documentable_name(d).to_string();
            let kind = documentable_kind(d);
            let signature = render_signature(d, &module.path, effects);

            if !module_had_docs {
                create_dir_all(&module_dir)?;
                module_had_docs = true;
            }

            let file_path = module_dir.join(format!("{name}.html"));
            let page_dir = file_path.parent().unwrap_or(output_dir).to_path_buf();
            let extras = render_item_extras(d, &link_table, &page_dir, output_dir);
            let sidebar = render_sidebar(&index_entries, &module_docs, &page_dir, output_dir);
            let html = render_item_page_with_links(
                &name,
                kind,
                &signature,
                doc,
                &extras,
                &sidebar,
                &link_table,
                &page_dir,
                output_dir,
            );
            std::fs::write(&file_path, &html).map_err(|e| DocBuildError::WriteFile {
                path: file_path.clone(),
                source: e.to_string(),
            })?;
            result.written.push(file_path);
        }
    }

    // Per-module index pages. Each non-synthetic module outside the
    // crate root that has at least one documented item or carries a
    // `//!` module doc gets its own `<module_dir>/index.html`. The
    // crate root's index is the global one written below.
    for module in &tree.modules {
        if module.is_synthetic || module.path.is_empty() {
            continue;
        }
        let items_in_module: Vec<&IndexEntry> = index_entries
            .iter()
            .filter(|e| e.module_path == module.path)
            .collect();
        if items_in_module.is_empty() && module.module_doc_comment.is_none() {
            continue;
        }
        let module_dir = module_output_dir(output_dir, &module.path);
        create_dir_all(&module_dir)?;
        let module_index_path = module_dir.join("index.html");
        let sidebar = render_sidebar(&index_entries, &module_docs, &module_dir, output_dir);
        let html = render_module_index(
            &module.path,
            &items_in_module,
            module.module_doc_comment.as_deref(),
            &sidebar,
            &link_table,
            &module_dir,
            output_dir,
        );
        std::fs::write(&module_index_path, html).map_err(|e| DocBuildError::WriteFile {
            path: module_index_path.clone(),
            source: e.to_string(),
        })?;
        result.written.push(module_index_path);
    }

    // Write the global crate-root index.
    let crate_sidebar = render_sidebar(&index_entries, &module_docs, output_dir, output_dir);
    let index_html = render_index(
        &index_entries,
        &module_docs,
        &crate_sidebar,
        &link_table,
        output_dir,
    );
    let index_path = output_dir.join("index.html");
    std::fs::write(&index_path, index_html).map_err(|e| DocBuildError::WriteFile {
        path: index_path.clone(),
        source: e.to_string(),
    })?;
    result.written.push(index_path);

    // Write the JSON search index. Always emitted (even when empty) so a
    // deployed doc site has a stable URL for client-side search to fetch.
    // Not added to `result.written` — the CLI summary counts HTML pages.
    let search_json = render_search_index(&index_entries);
    let search_path = output_dir.join("search-index.json");
    std::fs::write(&search_path, search_json).map_err(|e| DocBuildError::WriteFile {
        path: search_path,
        source: e.to_string(),
    })?;

    Ok(result)
}

fn create_dir_all(path: &Path) -> Result<(), DocBuildError> {
    std::fs::create_dir_all(path).map_err(|e| DocBuildError::CreateDir {
        path: path.to_path_buf(),
        source: e.to_string(),
    })
}

/// Map a module path (e.g. `["db", "connection"]`) to its output
/// directory under the doc root: `<root>/db/connection/`. The crate-root
/// module (empty path) renders into `<root>/` directly.
fn module_output_dir(root: &Path, path: &ModulePath) -> PathBuf {
    let mut p = root.to_path_buf();
    for seg in path {
        p.push(seg);
    }
    p
}

fn relative_href(file_path: &Path, root: &Path) -> String {
    let rel = file_path.strip_prefix(root).unwrap_or(file_path);
    rel.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Per-item kind tag rendered in the index ("fn", "struct", etc.).
#[derive(Debug, Clone, Copy)]
pub enum ItemKind {
    Function,
    Struct,
    Enum,
    Trait,
    Const,
    TypeAlias,
    DistinctType,
    ExternFn,
    ExternOpaqueType,
    Layout,
}

impl ItemKind {
    fn as_str(self) -> &'static str {
        match self {
            ItemKind::Function => "fn",
            ItemKind::Struct => "struct",
            ItemKind::Enum => "enum",
            ItemKind::Trait => "trait",
            ItemKind::Const => "const",
            ItemKind::TypeAlias => "type",
            ItemKind::DistinctType => "distinct",
            ItemKind::ExternFn => "extern fn",
            ItemKind::ExternOpaqueType => "extern type",
            ItemKind::Layout => "layout",
        }
    }
}

struct IndexEntry {
    module_path: ModulePath,
    item_name: String,
    kind: ItemKind,
    relative_href: String,
    /// One-line summary of the doc comment (markdown-stripped, first
    /// non-empty line). Used by the JSON search index so a search bar
    /// can preview match candidates without fetching each page.
    summary: String,
}

/// One documentable view in a module's source order. `Top` is a
/// top-level item; `ExternBlockChild` is one item drawn from inside an
/// `unsafe extern "ABI" { ... }` block. Each child gets its own doc
/// page exactly like the pre-block standalone `extern "ABI" fn ...;`
/// shape did, so the `///` prose on a foreign-import declaration still
/// surfaces in `karac doc`.
#[derive(Copy, Clone)]
enum Documentable<'a> {
    Top(&'a Item),
    ExternBlockChild {
        abi: &'a str,
        function: &'a ExternFunction,
    },
    ExternBlockOpaqueType {
        opaque_type: &'a OpaqueTypeDecl,
    },
}

/// Walk `items` in source order, expanding each `Item::ExternBlock`
/// into one `Documentable` per child. The block itself does not
/// currently produce a documentable view — block-level `///` prose
/// (the `# Safety` carrier) lands with the `undocumented_unsafe`
/// block-level lint slice (slice 5 of the FFI hardening epic).
fn documentables(items: &[Item]) -> Vec<Documentable<'_>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::ExternBlock(block) => {
                for ext in &block.items {
                    match ext {
                        ExternItem::Function(f) => out.push(Documentable::ExternBlockChild {
                            abi: &block.abi,
                            function: f,
                        }),
                        ExternItem::OpaqueType(o) => {
                            out.push(Documentable::ExternBlockOpaqueType { opaque_type: o })
                        }
                    }
                }
            }
            _ => out.push(Documentable::Top(item)),
        }
    }
    out
}

/// Pull the doc comment off a documentable, if any. Returns `None` for
/// kinds the MVP doesn't render (impl blocks, use decls, etc.) and for
/// items with no `///` prose.
fn documentable_doc(d: Documentable<'_>) -> Option<&str> {
    match d {
        Documentable::Top(item) => match item {
            Item::Function(f) => f.doc_comment.as_deref(),
            Item::StructDef(s) => s.doc_comment.as_deref(),
            Item::EnumDef(e) => e.doc_comment.as_deref(),
            Item::TraitDef(t) => t.doc_comment.as_deref(),
            Item::ConstDecl(c) => c.doc_comment.as_deref(),
            Item::TypeAlias(t) => t.doc_comment.as_deref(),
            Item::DistinctType(d) => d.doc_comment.as_deref(),
            Item::ExternFunction(e) => e.doc_comment.as_deref(),
            Item::LayoutDef(l) => l.doc_comment.as_deref(),
            _ => None,
        },
        Documentable::ExternBlockChild { function, .. } => function.doc_comment.as_deref(),
        Documentable::ExternBlockOpaqueType { opaque_type } => opaque_type.doc_comment.as_deref(),
    }
}

fn documentable_name(d: Documentable<'_>) -> &str {
    match d {
        Documentable::Top(item) => match item {
            Item::Function(f) => &f.name,
            Item::StructDef(s) => &s.name,
            Item::EnumDef(e) => &e.name,
            Item::TraitDef(t) => &t.name,
            Item::ConstDecl(c) => &c.name,
            Item::TypeAlias(t) => &t.name,
            Item::DistinctType(d) => &d.name,
            Item::ExternFunction(e) => &e.name,
            Item::LayoutDef(l) => &l.name,
            _ => "",
        },
        Documentable::ExternBlockChild { function, .. } => &function.name,
        Documentable::ExternBlockOpaqueType { opaque_type } => &opaque_type.name,
    }
}

fn documentable_kind(d: Documentable<'_>) -> ItemKind {
    match d {
        Documentable::Top(item) => match item {
            Item::Function(_) => ItemKind::Function,
            Item::StructDef(_) => ItemKind::Struct,
            Item::EnumDef(_) => ItemKind::Enum,
            Item::TraitDef(_) => ItemKind::Trait,
            Item::ConstDecl(_) => ItemKind::Const,
            Item::TypeAlias(_) => ItemKind::TypeAlias,
            Item::DistinctType(_) => ItemKind::DistinctType,
            Item::ExternFunction(_) => ItemKind::ExternFn,
            Item::LayoutDef(_) => ItemKind::Layout,
            _ => ItemKind::Function,
        },
        Documentable::ExternBlockChild { .. } => ItemKind::ExternFn,
        Documentable::ExternBlockOpaqueType { .. } => ItemKind::ExternOpaqueType,
    }
}

/// Best-effort one-line signature for an item. The MVP renders a
/// pretty-but-minimal form; full faithful round-trip rendering belongs
/// in the formatter, which this MVP intentionally does not invoke.
///
/// When `effects` is provided and the item is a `pub fn` whose entry in
/// the map is non-empty, the signature is suffixed with the rendered
/// `with <effects>` clause (e.g. ` with reads(File) + writes(Stdout)`).
/// Private fns and items without an entry in the map are unchanged.
fn render_signature(
    d: Documentable<'_>,
    module_path: &[String],
    effects: Option<&EffectsByItem>,
) -> String {
    let item = match d {
        Documentable::Top(item) => item,
        Documentable::ExternBlockChild { abi, function } => {
            return format!("extern \"{abi}\" fn {}", function.name);
        }
        Documentable::ExternBlockOpaqueType { opaque_type } => {
            return format!("extern type {}", opaque_type.name);
        }
    };
    match item {
        Item::Function(f) => {
            let params = f
                .params
                .iter()
                .map(|p| match p.name() {
                    Some(n) => format!("{n}: {}", type_repr(&p.ty)),
                    None => format!("_: {}", type_repr(&p.ty)),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let ret = f
                .return_type
                .as_ref()
                .map(|t| format!(" -> {}", type_repr(t)))
                .unwrap_or_default();
            let with_clause = if f.is_pub {
                effects
                    .and_then(|map| map.get(&(module_path.to_vec(), f.name.clone())))
                    .and_then(format_with_clause)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            format!("fn {}({params}){ret}{with_clause}", f.name)
        }
        Item::StructDef(s) => format!("struct {}", s.name),
        Item::EnumDef(e) => format!("enum {}", e.name),
        Item::TraitDef(t) => format!("trait {}", t.name),
        Item::ConstDecl(c) => format!("const {}: {}", c.name, type_repr(&c.ty)),
        Item::TypeAlias(t) => format!("type {}", t.name),
        Item::DistinctType(d) => format!("distinct type {}", d.name),
        Item::ExternFunction(e) => format!("extern \"{}\" fn {}", e.abi, e.name),
        Item::LayoutDef(l) => format!("layout {}", l.name),
        _ => String::new(),
    }
}

/// Render the per-item structured surface that goes between the
/// signature block and the doc-comment prose. For structs this is a
/// `<dl class="fields">` of `name: TypeExpr` rows; for enums it's a
/// `<ul class="variants">` of variant declarations with their payload
/// shape (unit / tuple / struct). Returns an empty string for kinds
/// that don't carry public surface here (functions, consts, etc. — the
/// signature line already shows everything).
fn render_item_extras(
    d: Documentable<'_>,
    link_table: &HashMap<String, String>,
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    let item = match d {
        Documentable::Top(item) => item,
        // Foreign-import declarations don't carry per-param `///` prose
        // through the parser today; matches pre-block standalone-form
        // parity. Lift if param-doc surface lands for extern fns.
        Documentable::ExternBlockChild { .. } => return String::new(),
        // Opaque foreign types have no fields, methods, or derives —
        // the signature line is the entire structural surface.
        Documentable::ExternBlockOpaqueType { .. } => return String::new(),
    };
    match item {
        Item::StructDef(s) => render_struct_extras(s, link_table, page_dir, doc_root),
        Item::EnumDef(e) => render_enum_extras(e, link_table, page_dir, doc_root),
        Item::Function(f) => render_param_extras(f, link_table, page_dir, doc_root),
        _ => String::new(),
    }
}

fn render_param_extras(
    f: &Function,
    link_table: &HashMap<String, String>,
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    if !f.params.iter().any(|p| p.doc_comment.is_some()) {
        return String::new();
    }
    let mut out = String::from("<h2>Parameters</h2>\n<dl class=\"params\">\n");
    for p in &f.params {
        let name = p.name().unwrap_or("_");
        out.push_str("<dt>");
        out.push_str(&html_escape(name));
        out.push_str(": ");
        out.push_str(&html_escape(&type_repr(&p.ty)));
        out.push_str("</dt>\n<dd>");
        if let Some(doc) = &p.doc_comment {
            out.push_str(&markdown_to_html(doc, link_table, page_dir, doc_root));
        }
        out.push_str("</dd>\n");
    }
    out.push_str("</dl>\n");
    out
}

fn render_struct_extras(
    s: &StructDef,
    link_table: &HashMap<String, String>,
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    if s.fields.is_empty() {
        return String::new();
    }
    let mut out = String::from("<h2>Fields</h2>\n<dl class=\"fields\">\n");
    for f in &s.fields {
        out.push_str("<dt>");
        out.push_str(&html_escape(&f.name));
        out.push_str("</dt>\n<dd>");
        out.push_str(&html_escape(&type_repr(&f.ty)));
        if let Some(doc) = &f.doc_comment {
            out.push('\n');
            out.push_str(&markdown_to_html(doc, link_table, page_dir, doc_root));
        }
        out.push_str("</dd>\n");
    }
    out.push_str("</dl>\n");
    out
}

fn render_enum_extras(
    e: &EnumDef,
    link_table: &HashMap<String, String>,
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    if e.variants.is_empty() {
        return String::new();
    }
    let mut out = String::from("<h2>Variants</h2>\n<ul class=\"variants\">\n");
    for v in &e.variants {
        out.push_str("<li>");
        match &v.kind {
            VariantKind::Struct(fields) if fields.iter().any(|f| f.doc_comment.is_some()) => {
                // At least one field carries a `///` — render the body as
                // a nested per-field `<dl>` so each field's prose lands
                // next to it, while keeping the `Variant { ... }` shell.
                out.push_str(&html_escape(&v.name));
                out.push_str(" {\n<dl class=\"variant-fields\">\n");
                for f in fields {
                    out.push_str("<dt>");
                    out.push_str(&html_escape(&f.name));
                    out.push_str(": ");
                    out.push_str(&html_escape(&type_repr(&f.ty)));
                    out.push_str("</dt>\n<dd>");
                    if let Some(doc) = &f.doc_comment {
                        out.push_str(&markdown_to_html(doc, link_table, page_dir, doc_root));
                    }
                    out.push_str("</dd>\n");
                }
                out.push_str("</dl>\n}");
            }
            _ => {
                // Unit / Tuple / struct-variant without any field docs:
                // keep the original inline signature line.
                out.push_str(&html_escape(&render_variant(v)));
            }
        }
        if let Some(doc) = &v.doc_comment {
            out.push('\n');
            out.push_str(&markdown_to_html(doc, link_table, page_dir, doc_root));
        }
        out.push_str("</li>\n");
    }
    out.push_str("</ul>\n");
    out
}

fn render_variant(v: &Variant) -> String {
    match &v.kind {
        VariantKind::Unit => v.name.clone(),
        VariantKind::Tuple(types) => {
            let parts = types.iter().map(type_repr).collect::<Vec<_>>().join(", ");
            format!("{}({})", v.name, parts)
        }
        VariantKind::Struct(fields) => {
            let parts = fields
                .iter()
                .map(|f: &StructField| format!("{}: {}", f.name, type_repr(&f.ty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} {{ {} }}", v.name, parts)
        }
    }
}

/// Rough type display. The formatter has the canonical version; the doc
/// renderer just needs something readable. Falls back to the type's
/// `{:?}` debug form when the structural cases don't match.
fn type_repr(t: &TypeExpr) -> String {
    use crate::ast::TypeKind;
    match &t.kind {
        TypeKind::Path(path) => path.segments.join("."),
        TypeKind::Tuple(elems) if elems.is_empty() => "()".to_string(),
        TypeKind::Tuple(elems) => format!(
            "({})",
            elems.iter().map(type_repr).collect::<Vec<_>>().join(", ")
        ),
        TypeKind::Ref(inner) => format!("ref {}", type_repr(inner)),
        TypeKind::MutRef(inner) => format!("mut ref {}", type_repr(inner)),
        _ => format!("{:?}", t.kind),
    }
}

/// Shared CSS for every doc page. Defines the two-column flex layout
/// (sidebar on the left, main on the right) plus the per-element typography
/// rules used by item pages, per-module indexes, and the global index.
const SHARED_CSS: &str = r#"body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
        margin: 0; padding: 0; line-height: 1.5; display: flex;
        max-width: 1200px; margin-left: auto; margin-right: auto; }
aside.sidebar { width: 240px; flex-shrink: 0; padding: 1.5em 1em;
                border-right: 1px solid #eee; font-size: 0.9em;
                box-sizing: border-box; }
aside.sidebar h2 { margin: 0 0 0.5em 0; font-size: 1em; }
aside.sidebar h2 a { color: #333; text-decoration: none; }
aside.sidebar ul { list-style: none; padding-left: 1em; margin: 0.2em 0; }
aside.sidebar li { margin: 0.15em 0; }
aside.sidebar details { margin: 0.4em 0; }
aside.sidebar summary { cursor: pointer; font-family: ui-monospace, "SF Mono", Menlo, monospace;
                        font-weight: 600; }
aside.sidebar summary a { color: #333; text-decoration: none; }
aside.sidebar a:hover { text-decoration: underline; }
main { flex-grow: 1; padding: 1.5em 1.5em; min-width: 0; max-width: 800px; }
nav.local { font-size: 0.9em; margin-bottom: 1em; }
.kind { color: #888; font-size: 0.9em; }
.signature { background: #f4f4f4; padding: 0.6em 0.8em; border-radius: 4px;
             font-family: ui-monospace, "SF Mono", Menlo, monospace;
             white-space: pre-wrap; }
.fields dt { font-family: ui-monospace, "SF Mono", Menlo, monospace;
             font-weight: 600; margin-top: 0.4em; }
.fields dd { margin: 0 0 0.6em 1.5em;
             font-family: ui-monospace, "SF Mono", Menlo, monospace;
             color: #555; }
.variants li { font-family: ui-monospace, "SF Mono", Menlo, monospace;
               margin: 0.2em 0; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: 0.4em 0.6em; border-bottom: 1px solid #eee; }
th { background: #f4f4f4; }
.module-doc { font-size: 0.95em; color: #555; }
.crate-root-doc { margin-bottom: 1em; }
"#;

#[allow(clippy::too_many_arguments)]
fn render_item_page_with_links(
    name: &str,
    kind: ItemKind,
    signature: &str,
    markdown: &str,
    extras_html: &str,
    sidebar_html: &str,
    link_table: &HashMap<String, String>,
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    let html_body = markdown_to_html(markdown, link_table, page_dir, doc_root);
    let title_kind = kind.as_str();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title_kind} {name}</title>
<style>
{SHARED_CSS}</style>
</head>
<body>
{sidebar_html}<main>
<nav class="local"><a href="index.html">← back to module index</a></nav>
<p class="kind">{title_kind}</p>
<h1>{escaped_name}</h1>
<pre class="signature">{escaped_sig}</pre>
{extras_html}{html_body}
</main>
</body>
</html>
"#,
        escaped_name = html_escape(name),
        escaped_sig = html_escape(signature),
    )
}

/// Render the global sidebar shown on every doc page. Modules sort
/// alphabetically; items within each module sort alphabetically. A
/// module appears iff it contains at least one documented item or
/// carries a `//!` block — i.e. exactly the set of modules that have
/// a per-module `index.html` page.
///
/// All hrefs are rewritten via `relative_link` so the same logical
/// link works from a crate-root page (`Foo.html`), a deep page
/// (`../Foo.html`), or the root index itself.
fn render_sidebar(
    entries: &[IndexEntry],
    module_docs: &[(ModulePath, Option<String>)],
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    let mut by_module: BTreeMap<ModulePath, Vec<&IndexEntry>> = BTreeMap::new();
    for e in entries {
        by_module.entry(e.module_path.clone()).or_default().push(e);
    }
    for v in by_module.values_mut() {
        v.sort_by(|a, b| a.item_name.cmp(&b.item_name));
    }

    // Modules that should appear in the sidebar: those with at least
    // one documented item OR a `//!` block. This matches the per-module
    // index emit condition in `build_docs`.
    let mut sidebar_modules: BTreeSet<ModulePath> = BTreeSet::new();
    for (p, doc) in module_docs {
        if !p.is_empty() && doc.is_some() {
            sidebar_modules.insert(p.clone());
        }
    }
    for e in entries {
        if !e.module_path.is_empty() {
            sidebar_modules.insert(e.module_path.clone());
        }
    }

    let crate_root_href = relative_link(page_dir, doc_root, "index.html");
    let mut s = String::new();
    s.push_str("<aside class=\"sidebar\">\n");
    s.push_str(&format!(
        "<h2><a href=\"{}\">Crate</a></h2>\n",
        html_escape(&crate_root_href)
    ));

    if let Some(root_items) = by_module.get(&Vec::<String>::new()) {
        s.push_str("<ul>\n");
        for e in root_items {
            let href = relative_link(page_dir, doc_root, &e.relative_href);
            s.push_str(&format!(
                "<li><a href=\"{}\">{}</a></li>\n",
                html_escape(&href),
                html_escape(&e.item_name)
            ));
        }
        s.push_str("</ul>\n");
    }

    for module_path in &sidebar_modules {
        let title = module_path.join(".");
        let mut module_index_rel = String::new();
        for seg in module_path {
            if !module_index_rel.is_empty() {
                module_index_rel.push('/');
            }
            module_index_rel.push_str(seg);
        }
        module_index_rel.push_str("/index.html");
        let mod_href = relative_link(page_dir, doc_root, &module_index_rel);
        s.push_str("<details open>\n");
        s.push_str(&format!(
            "<summary><a href=\"{}\">{}</a></summary>\n",
            html_escape(&mod_href),
            html_escape(&title)
        ));
        if let Some(items) = by_module.get(module_path) {
            s.push_str("<ul>\n");
            for e in items {
                let href = relative_link(page_dir, doc_root, &e.relative_href);
                s.push_str(&format!(
                    "<li><a href=\"{}\">{}</a></li>\n",
                    html_escape(&href),
                    html_escape(&e.item_name)
                ));
            }
            s.push_str("</ul>\n");
        }
        s.push_str("</details>\n");
    }

    s.push_str("</aside>\n");
    s
}

fn render_index(
    entries: &[IndexEntry],
    module_docs: &[(ModulePath, Option<String>)],
    sidebar_html: &str,
    link_table: &HashMap<String, String>,
    doc_root: &Path,
) -> String {
    let mut rows = String::new();
    let mut sorted: Vec<&IndexEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.module_path
            .cmp(&b.module_path)
            .then(a.item_name.cmp(&b.item_name))
    });

    // Track the current module so we can emit a module-doc row before the
    // first item row of each non-root module.
    let docs_by_path: HashMap<&ModulePath, &str> = module_docs
        .iter()
        .filter_map(|(p, d)| d.as_deref().map(|s| (p, s)))
        .collect();
    let mut last_module: Option<&ModulePath> = None;

    for entry in sorted {
        if last_module != Some(&entry.module_path) {
            // Module changed — emit a module-doc row if this non-root
            // module carries a `//!`. The crate root's doc renders above
            // the table; skip it here.
            if !entry.module_path.is_empty() {
                if let Some(doc) = docs_by_path.get(&entry.module_path) {
                    let body = markdown_to_html(doc, link_table, doc_root, doc_root);
                    rows.push_str(&format!(
                        "<tr><td colspan=\"3\"><div class=\"module-doc\">{body}</div></td></tr>\n"
                    ));
                }
            }
            last_module = Some(&entry.module_path);
        }
        let mod_disp = if entry.module_path.is_empty() {
            "(crate root)".to_string()
        } else {
            entry.module_path.join(".")
        };
        rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td><a href=\"{}\">{}</a></td></tr>\n",
            html_escape(&mod_disp),
            entry.kind.as_str(),
            html_escape(&entry.relative_href),
            html_escape(&entry.item_name),
        ));
    }

    // Crate-root `//!` renders above the table.
    let crate_root_doc = module_docs
        .iter()
        .find(|(p, _)| p.is_empty())
        .and_then(|(_, d)| d.as_deref())
        .map(|doc| {
            let body = markdown_to_html(doc, link_table, doc_root, doc_root);
            format!("<div class=\"module-doc crate-root-doc\">{body}</div>\n")
        })
        .unwrap_or_default();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Kāra documentation</title>
<style>
{SHARED_CSS}</style>
</head>
<body>
{sidebar_html}<main>
<h1>Kāra documentation</h1>
{crate_root_doc}<table>
<thead><tr><th>Module</th><th>Kind</th><th>Item</th></tr></thead>
<tbody>
{rows}</tbody>
</table>
</main>
</body>
</html>
"#
    )
}

/// Render the per-module `index.html` for a non-crate-root module: lists
/// only this module's items (sorted by name), with the module's `//!`
/// doc above the table when present. Includes a "back to crate root"
/// nav link computed relative to this page's directory.
#[allow(clippy::too_many_arguments)]
fn render_module_index(
    module_path: &ModulePath,
    items: &[&IndexEntry],
    module_doc: Option<&str>,
    sidebar_html: &str,
    link_table: &HashMap<String, String>,
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    let title = module_path.join(".");
    let mut rows = String::new();
    let mut sorted: Vec<&&IndexEntry> = items.iter().collect();
    sorted.sort_by(|a, b| a.item_name.cmp(&b.item_name));
    for entry in sorted {
        // The IndexEntry's `relative_href` is relative to the doc root,
        // but this page lives at `<doc_root>/<module>/index.html` — for
        // an item at `<doc_root>/<module>/Foo.html` we want a bare
        // `Foo.html` link, so rewrite via `relative_link`.
        let href = relative_link(page_dir, doc_root, &entry.relative_href);
        rows.push_str(&format!(
            "<tr><td>{}</td><td><a href=\"{}\">{}</a></td></tr>\n",
            entry.kind.as_str(),
            html_escape(&href),
            html_escape(&entry.item_name),
        ));
    }

    let module_doc_html = module_doc
        .map(|doc| {
            let body = markdown_to_html(doc, link_table, page_dir, doc_root);
            format!("<div class=\"module-doc\">{body}</div>\n")
        })
        .unwrap_or_default();

    let crate_root_href = relative_link(page_dir, doc_root, "index.html");
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>module {title}</title>
<style>
{SHARED_CSS}</style>
</head>
<body>
{sidebar_html}<main>
<nav class="local"><a href="{crate_root_href}">← crate root</a></nav>
<h1>module <code>{escaped_title}</code></h1>
{module_doc_html}<table>
<thead><tr><th>Kind</th><th>Item</th></tr></thead>
<tbody>
{rows}</tbody>
</table>
</main>
</body>
</html>
"#,
        escaped_title = html_escape(&title),
    )
}

/// Render a doc-comment Markdown blob to HTML, resolving CommonMark
/// `[Name]` / `[Name][]` link references against `link_table` (item name
/// → href relative to `doc_root`). Hrefs are rewritten to be relative to
/// `page_dir` so a deep-module page can link back to a crate-root item
/// via `../Vec.html`. Unresolved references render as plain text — see
/// `build_docs` for the no-warn rationale.
fn markdown_to_html(
    input: &str,
    link_table: &HashMap<String, String>,
    page_dir: &Path,
    doc_root: &Path,
) -> String {
    use pulldown_cmark::{html, BrokenLink, CowStr, Options, Parser};
    let mut callback = |link: BrokenLink<'_>| -> Option<(CowStr<'_>, CowStr<'_>)> {
        let target_rel = link_table.get(link.reference.as_ref())?;
        let resolved = relative_link(page_dir, doc_root, target_rel);
        Some((CowStr::from(resolved), CowStr::from("")))
    };
    let parser = Parser::new_with_broken_link_callback(input, Options::all(), Some(&mut callback));
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// Compute a forward-slash relative path from `page_dir` to a target
/// expressed as a path relative to `doc_root`. Caller-side: `target_rel`
/// is the value stored in `link_table`, e.g. `"Vec.html"` or
/// `"db/connection.html"`. The result is suitable as an `href` attribute
/// in HTML — it's URL-style (always `/`) regardless of the host
/// filesystem's path separator.
fn relative_link(page_dir: &Path, doc_root: &Path, target_rel: &str) -> String {
    let target_abs = doc_root.join(target_rel);
    let from_components: Vec<_> = page_dir
        .strip_prefix(doc_root)
        .unwrap_or(Path::new(""))
        .components()
        .collect();
    let to_components: Vec<_> = target_abs
        .strip_prefix(doc_root)
        .unwrap_or(Path::new(target_rel))
        .components()
        .collect();
    // Walk past the shared prefix.
    let mut shared = 0;
    while shared < from_components.len()
        && shared < to_components.len()
        && from_components[shared] == to_components[shared]
    {
        shared += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..(from_components.len() - shared) {
        parts.push("..".to_string());
    }
    for c in &to_components[shared..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        // Self-link — degenerate but possible if a page references itself.
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Build the cross-reference table consumed by the broken-link callback.
/// Walks every non-synthetic module in `tree` and records each
/// documentable item's name → `<module-dirs>/Item.html` (path relative
/// to the doc root). Items without a `doc_comment` are still entered —
/// otherwise a `[Vec]` reference whose target is itself undocumented
/// would silently fail to resolve, which is more confusing than helpful.
fn build_link_table(tree: &ProgramTree) -> HashMap<String, String> {
    let mut table = HashMap::new();
    for module in &tree.modules {
        if module.is_synthetic {
            continue;
        }
        for d in documentables(&module.items) {
            let name = documentable_name(d);
            if name.is_empty() {
                continue;
            }
            let mut href = String::new();
            for seg in &module.path {
                href.push_str(seg);
                href.push('/');
            }
            href.push_str(name);
            href.push_str(".html");
            table.entry(name.to_string()).or_insert(href);
        }
    }
    table
}

/// Format the trailing ` with <effects>` clause for a `pub fn` signature.
/// Returns `None` when there's nothing to render (no effects and not
/// polymorphic). The leading space is part of the returned string so
/// the caller can concatenate unconditionally.
fn format_with_clause(display: &EffectDisplay) -> Option<String> {
    if display.effects.is_empty() && !display.polymorphic {
        return None;
    }
    let mut parts: Vec<String> = display
        .effects
        .iter()
        .map(|(verb, resource)| {
            if resource.is_empty() {
                effect_verb_name(verb).to_string()
            } else {
                format!("{}({resource})", effect_verb_name(verb))
            }
        })
        .collect();
    if display.polymorphic {
        parts.push("_".to_string());
    }
    Some(format!(" with {}", parts.join(" + ")))
}

fn effect_verb_name(v: &EffectVerbKind) -> &str {
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Extract a one-line summary from a doc comment. Picks the first
/// non-empty line, strips leading `#` heading markers, trims whitespace.
/// Not a full markdown-to-text pass — for a search-result preview the
/// signal is the leading prose, and a perfect strip would be over-
/// engineering for v1.
fn summarize_doc(doc: &str) -> String {
    for line in doc.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Drop ATX heading markers ("# Title" → "Title") so a doc that
        // opens with `# Overview\n...` doesn't surface the hash in the
        // preview. Other markdown (bold, code spans, etc.) renders fine
        // as plain text in a search dropdown.
        let stripped = trimmed.trim_start_matches('#').trim_start();
        return stripped.to_string();
    }
    String::new()
}

/// Render the JSON search index. Schema: a JSON array of objects, one
/// per documented item, shape:
/// `{"name": "...", "kind": "...", "href": "...", "summary": "..."}`.
/// Always sorted by `(module_path, item_name)` to match the HTML index
/// for deterministic output.
fn render_search_index(entries: &[IndexEntry]) -> String {
    let mut sorted: Vec<&IndexEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.module_path
            .cmp(&b.module_path)
            .then(a.item_name.cmp(&b.item_name))
    });
    let mut out = String::from("[");
    for (i, e) in sorted.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        out.push_str(&json_escape_string(&e.item_name));
        out.push_str(",\"kind\":");
        out.push_str(&json_escape_string(e.kind.as_str()));
        out.push_str(",\"href\":");
        out.push_str(&json_escape_string(&e.relative_href));
        out.push_str(",\"summary\":");
        out.push_str(&json_escape_string(&e.summary));
        out.push('}');
    }
    out.push(']');
    out
}

/// Escape `s` per RFC 8259 and wrap in double quotes. Mirrors the
/// hand-rolled JSON helper in `cli.rs::json_string` — kept private here
/// rather than centralised to avoid coupling `doc.rs` to the CLI module.
fn json_escape_string(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_basic() {
        assert_eq!(html_escape("a < b > c"), "a &lt; b &gt; c");
        assert_eq!(html_escape("\"x\""), "&quot;x&quot;");
        assert_eq!(html_escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn markdown_renders_paragraphs() {
        let h = markdown_to_html(
            "Hello **world**.",
            &HashMap::new(),
            Path::new(""),
            Path::new(""),
        );
        assert!(h.contains("<p>"));
        assert!(h.contains("<strong>world</strong>"));
    }

    #[test]
    fn markdown_resolves_known_link_reference() {
        // `[Vec]` shortcut reference rewrites to a real link when the
        // target name is in the table; URL is computed relative to the
        // current page directory.
        let mut table = HashMap::new();
        table.insert("Vec".to_string(), "Vec.html".to_string());
        let h = markdown_to_html(
            "See [Vec] for details.",
            &table,
            Path::new("/doc"),
            Path::new("/doc"),
        );
        assert!(h.contains("href=\"Vec.html\""), "got: {h}");
        assert!(h.contains(">Vec</a>"));
    }

    #[test]
    fn markdown_unknown_link_reference_renders_as_text() {
        // No entry → callback returns None → pulldown emits the
        // original `[Missing]` text without an anchor.
        let h = markdown_to_html(
            "See [Missing] for details.",
            &HashMap::new(),
            Path::new("/doc"),
            Path::new("/doc"),
        );
        assert!(!h.contains("<a "), "should not produce a link; got: {h}");
        assert!(h.contains("[Missing]"));
    }

    #[test]
    fn relative_link_walks_up_from_deep_module() {
        // From a page at `/doc/db/connection.html` linking to `/doc/Vec.html`,
        // the href should be `../Vec.html`.
        let href = relative_link(Path::new("/doc/db"), Path::new("/doc"), "Vec.html");
        assert_eq!(href, "../Vec.html");
    }

    #[test]
    fn render_item_page_includes_signature_and_body() {
        let html = render_item_page_with_links(
            "double",
            ItemKind::Function,
            "fn double(n: i64) -> i64",
            "Doubles its argument.",
            "",
            "",
            &HashMap::new(),
            Path::new(""),
            Path::new(""),
        );
        assert!(html.contains("<title>fn double</title>"));
        assert!(html.contains("fn double(n: i64) -&gt; i64"));
        assert!(html.contains("Doubles its argument."));
        assert!(html.contains("← back to module index"));
    }

    #[test]
    fn summarize_doc_picks_first_nonempty_line() {
        assert_eq!(summarize_doc("First line.\nSecond line."), "First line.");
        assert_eq!(summarize_doc(""), "");
        // Leading blank line skipped.
        assert_eq!(summarize_doc("\n\nReal content."), "Real content.");
        // ATX heading markers stripped — `# Overview` → `Overview`.
        assert_eq!(summarize_doc("# Overview\n\nMore."), "Overview");
        assert_eq!(summarize_doc("### Deep heading"), "Deep heading");
    }

    #[test]
    fn format_with_clause_handles_each_shape() {
        // Empty + non-polymorphic → no clause.
        let empty = EffectDisplay::default();
        assert_eq!(format_with_clause(&empty), None);

        // Single explicit effect.
        let one = EffectDisplay {
            effects: vec![(EffectVerbKind::Reads, "File".to_string())],
            polymorphic: false,
        };
        assert_eq!(
            format_with_clause(&one).as_deref(),
            Some(" with reads(File)")
        );

        // Multiple effects joined with ` + `, in caller-provided order.
        let two = EffectDisplay {
            effects: vec![
                (EffectVerbKind::Reads, "File".to_string()),
                (EffectVerbKind::Writes, "Stdout".to_string()),
            ],
            polymorphic: false,
        };
        assert_eq!(
            format_with_clause(&two).as_deref(),
            Some(" with reads(File) + writes(Stdout)")
        );

        // `with _` only.
        let poly = EffectDisplay {
            effects: Vec::new(),
            polymorphic: true,
        };
        assert_eq!(format_with_clause(&poly).as_deref(), Some(" with _"));

        // Polymorphic with fixed — `_` appears last.
        let mixed = EffectDisplay {
            effects: vec![(EffectVerbKind::Reads, "File".to_string())],
            polymorphic: true,
        };
        assert_eq!(
            format_with_clause(&mixed).as_deref(),
            Some(" with reads(File) + _")
        );

        // Verb without resource (effects whose verb takes no resource —
        // `panics`, `blocks`, `suspends` — pass an empty resource string).
        let bare = EffectDisplay {
            effects: vec![(EffectVerbKind::Panics, String::new())],
            polymorphic: false,
        };
        assert_eq!(format_with_clause(&bare).as_deref(), Some(" with panics"));
    }

    #[test]
    fn json_escape_string_handles_specials() {
        assert_eq!(json_escape_string("plain"), "\"plain\"");
        assert_eq!(
            json_escape_string("with \"quotes\""),
            "\"with \\\"quotes\\\"\""
        );
        assert_eq!(json_escape_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_escape_string("a\\b"), "\"a\\\\b\"");
        // Control char below 0x20 → `\u00XX`.
        assert_eq!(json_escape_string("\x01"), "\"\\u0001\"");
    }
}
