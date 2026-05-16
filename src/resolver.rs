// src/resolver.rs

//! Name resolution for the Kāra language.
//!
//! Walks the AST produced by the parser, builds a symbol table, resolves all
//! name references to their definitions, and reports errors for undefined
//! names, duplicates, and visibility violations.

use crate::ast::*;
use crate::edit_distance::suggest_similar;
use crate::module::{self, ModuleId, ProgramTree};
use crate::token::Span;
use std::collections::HashMap;

mod collect;
mod resolve_block;
mod resolve_items;

// ── IDs ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub usize);

/// HashMap key derived from a Span's (offset, length).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanKey(pub usize, pub usize);

impl SpanKey {
    pub fn from_span(span: &Span) -> Self {
        SpanKey(span.offset, span.length)
    }
}

// ── Symbols ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Symbol {
    pub id: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    pub span: Span,
    pub is_pub: bool,
    pub scope: ScopeId,
}

#[derive(Debug, Clone)]
pub enum SymbolKind {
    Variable {
        is_mut: bool,
    },
    Function {
        param_names: Vec<String>,
    },
    Struct {
        field_names: Vec<String>,
    },
    Enum {
        variant_names: Vec<String>,
    },
    EnumVariant {
        parent_enum: SymbolId,
        variant_kind: VariantSymbolKind,
    },
    Trait {
        method_names: Vec<String>,
    },
    /// `trait NAME = bound1 + bound2 + ...;` — recognized at parse but
    /// not yet expanded. Use sites in typechecker emit
    /// `E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET`. Bound substitution lands in
    /// P1 (see `docs/deferred.md` § Trait Aliases — Expansion).
    TraitAlias,
    Constant,
    TypeParam,
    /// `const N: Type` const-generic parameter. Distinguished from
    /// `TypeParam` so the typechecker's declaration-site permitted-type
    /// check (spec at `design.md § Type Inference > Const generic
    /// parameters`) can branch on the symbol kind. The associated type
    /// expression is read from the source AST during typechecking.
    ConstParam,
    EffectResource,
    EffectGroup,
    EffectVerb,
    TypeAlias,
    Module,
    Import {
        path: Vec<String>,
    },
    SelfValue,
    ExternFunction,
    Primitive,
    DistinctType,
    /// Opaque foreign type declared inside an `unsafe extern "ABI" { ... }`
    /// block: `type Name;`. The Kāra side knows the name but neither the
    /// size, alignment, nor field layout — values of the type can only
    /// appear behind a pointer (`*const`/`*mut`) or reference
    /// (`ref`/`mut ref`). The typechecker rejects by-value uses with
    /// `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`.
    OpaqueForeignType,
}

#[derive(Debug, Clone)]
pub enum VariantSymbolKind {
    Unit,
    Tuple(usize),
    Struct(Vec<String>),
}

// ── Scopes ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Scope {
    pub id: ScopeId,
    pub parent: Option<ScopeId>,
    pub kind: ScopeKind,
    pub names: HashMap<String, SymbolId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScopeKind {
    Global,
    Function,
    Block,
    Impl { target_type: String },
    Closure,
    Loop,
    MatchArm,
}

// ── Symbol Table ────────────────────────────────────────────────

pub struct SymbolTable {
    symbols: Vec<Symbol>,
    scopes: Vec<Scope>,
    current_scope: ScopeId,
    pub type_methods: HashMap<String, Vec<SymbolId>>,
    /// Trait bounds recorded for `TypeParam` symbols (generic parameters). The
    /// stored list is the union of inline bounds (`T: Bound`) and where-clause
    /// bounds (`where T: Bound`) — both apply simultaneously per design. Used
    /// by the typechecker to dispatch `T.method(args)` and bare `method(args)`
    /// calls to methods declared on a bound trait.
    pub generic_param_bounds: HashMap<SymbolId, Vec<TraitBound>>,
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolTable {
    pub fn new() -> Self {
        let global_scope = Scope {
            id: ScopeId(0),
            parent: None,
            kind: ScopeKind::Global,
            names: HashMap::new(),
        };
        let mut table = SymbolTable {
            symbols: Vec::new(),
            scopes: vec![global_scope],
            current_scope: ScopeId(0),
            type_methods: HashMap::new(),
            generic_param_bounds: HashMap::new(),
        };
        table.register_primitives();
        table
    }

    /// Seed scope 0 with every prelude name. CR-24 slice 8 routes the lists
    /// through `crate::prelude` so the resolver, the typechecker, and the
    /// synthetic `std.prelude` module entry in [`crate::module::ProgramTree`]
    /// agree on a single source of truth. The actual symbol kinds still
    /// match what the rest of the resolver expects (primitives for type and
    /// trait names; functions for compiler builtins and enum variants), so
    /// there is no behaviour change at this layer.
    fn register_primitives(&mut self) {
        let synthetic_span = Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        };

        let push = |table: &mut SymbolTable, name: &str, kind: SymbolKind| {
            let id = SymbolId(table.symbols.len());
            table.symbols.push(Symbol {
                id,
                name: name.to_string(),
                kind,
                span: synthetic_span.clone(),
                is_pub: true,
                scope: ScopeId(0),
            });
            table.scopes[0].names.insert(name.to_string(), id);
        };

        for name in crate::prelude::PRELUDE_PRIMITIVES {
            push(self, name, SymbolKind::Primitive);
        }
        for name in crate::prelude::PRELUDE_FUNCTIONS {
            push(
                self,
                name,
                SymbolKind::Function {
                    param_names: Vec::new(),
                },
            );
        }
        for name in crate::prelude::PRELUDE_TYPES {
            push(self, name, SymbolKind::Primitive);
        }
        for name in crate::prelude::PRELUDE_TRAITS {
            push(self, name, SymbolKind::Primitive);
        }
        for name in crate::prelude::PRELUDE_VARIANTS {
            push(
                self,
                name,
                SymbolKind::Function {
                    param_names: Vec::new(),
                },
            );
        }
        for name in crate::prelude::PRELUDE_EFFECT_RESOURCES {
            push(self, name, SymbolKind::EffectResource);
        }

        // `process` is a built-in module (for `process::exit`). Not tracked
        // in `prelude.rs` because it is not part of the prelude per design —
        // it is a permanent magic module the resolver makes visible.
        push(self, "process", SymbolKind::Module);

        // Lowercase stdlib module aliases per design.md § I/O: users write
        // `env.args()`, `env.var(name)` — lowercase module, capitalized
        // resource name dispatches at interpreter/codegen layer.
        push(self, "env", SymbolKind::Module);
    }

    pub fn push_scope(&mut self, kind: ScopeKind) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        self.scopes.push(Scope {
            id,
            parent: Some(self.current_scope),
            kind,
            names: HashMap::new(),
        });
        self.current_scope = id;
        id
    }

    pub fn pop_scope(&mut self) {
        if let Some(parent) = self.scopes[self.current_scope.0].parent {
            self.current_scope = parent;
        }
    }

    pub fn current_scope_id(&self) -> ScopeId {
        self.current_scope
    }

    /// Reserved identifiers that cannot be used as user-defined names.
    const RESERVED_IDENTIFIERS: &'static [(&'static str, &'static str)] = &[
        ("Fn", "reserved for closure/function type constructor"),
        (
            "split_by_variant",
            "reserved as a contextual keyword in layout blocks",
        ),
    ];

    pub fn define(
        &mut self,
        name: String,
        kind: SymbolKind,
        span: Span,
        is_pub: bool,
    ) -> Result<SymbolId, ResolveError> {
        // Check reserved identifiers
        for &(reserved, reason) in Self::RESERVED_IDENTIFIERS {
            if name == reserved {
                return Err(ResolveError {
                    message: format!("'{}' is {}", name, reason),
                    span,
                    kind: ResolveErrorKind::ReservedIdentifier,
                    suggestion: None,
                    replacement: None,
                });
            }
        }

        let scope_id = self.current_scope;
        let scope = &self.scopes[scope_id.0];
        if let Some(&existing_id) = scope.names.get(&name) {
            let existing = &self.symbols[existing_id.0];
            // Allow user definitions to shadow prelude/built-in symbols (synthetic span 0:0)
            let is_prelude = existing.span.line == 0 && existing.span.column == 0;
            if !is_prelude {
                return Err(ResolveError {
                    message: format!(
                        "'{}' is already defined in this scope (first defined at {}:{})",
                        name, existing.span.line, existing.span.column
                    ),
                    span,
                    kind: ResolveErrorKind::DuplicateDefinition,
                    suggestion: None,
                    replacement: None,
                });
            }
        }
        let id = SymbolId(self.symbols.len());
        self.symbols.push(Symbol {
            id,
            name: name.clone(),
            kind,
            span,
            is_pub,
            scope: scope_id,
        });
        self.scopes[scope_id.0].names.insert(name, id);
        Ok(id)
    }

    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        let mut scope_id = self.current_scope;
        loop {
            let scope = &self.scopes[scope_id.0];
            if let Some(&sym_id) = scope.names.get(name) {
                return Some(&self.symbols[sym_id.0]);
            }
            match scope.parent {
                Some(parent) => scope_id = parent,
                None => return None,
            }
        }
    }

    pub fn lookup_in_scope(&self, scope_id: ScopeId, name: &str) -> Option<&Symbol> {
        self.scopes[scope_id.0]
            .names
            .get(name)
            .map(|&id| &self.symbols[id.0])
    }

    pub fn get_symbol(&self, id: SymbolId) -> &Symbol {
        &self.symbols[id.0]
    }

    pub fn register_method(&mut self, type_name: &str, method_id: SymbolId) {
        self.type_methods
            .entry(type_name.to_string())
            .or_default()
            .push(method_id);
    }

    /// Append trait bounds to the entry for `param_id`. Idempotent on identical
    /// bounds is not enforced — callers should not record the same bound twice.
    pub fn record_generic_bounds(&mut self, param_id: SymbolId, bounds: &[TraitBound]) {
        if bounds.is_empty() {
            return;
        }
        self.generic_param_bounds
            .entry(param_id)
            .or_default()
            .extend(bounds.iter().cloned());
    }

    /// Trait bounds attached to `param_id`. Empty slice if the symbol is not a
    /// generic parameter or has no bounds.
    pub fn get_generic_bounds(&self, param_id: SymbolId) -> &[TraitBound] {
        self.generic_param_bounds
            .get(&param_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Iterate every registered symbol. Used by tests that need to assert
    /// on symbols defined in nested scopes (e.g. generic params inside a
    /// function body) that aren't reachable via `lookup_in_scope` against
    /// the global scope.
    pub fn all_symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    pub fn lookup_method(&self, type_name: &str, method_name: &str) -> Option<&Symbol> {
        self.type_methods.get(type_name).and_then(|methods| {
            methods
                .iter()
                .map(|&id| &self.symbols[id.0])
                .find(|sym| sym.name == method_name)
        })
    }

    /// Collect all visible names from the current scope chain (for typo suggestions).
    pub fn visible_names(&self) -> Vec<&str> {
        let mut names = Vec::new();
        let mut scope_id = self.current_scope;
        loop {
            let scope = &self.scopes[scope_id.0];
            for name in scope.names.keys() {
                names.push(name.as_str());
            }
            match scope.parent {
                Some(parent) => scope_id = parent,
                None => break,
            }
        }
        names
    }
}

// ── Errors ──────────────────────────────────────────────────────

/// A precise source-text edit attached to a diagnostic. Consumers like
/// `karac fix` apply each edit by replacing `source[offset..offset+length]`
/// with `replacement`. Distinct from `suggestion` (a free-form
/// human-readable hint): `TextEdit` is *machine-applicable* — only attached
/// where the compiler can name an exact byte range and a verbatim
/// replacement (today: `did you mean` corrections on undefined names /
/// types where the resolver knows the misspelled identifier's span and
/// the visible-name candidate).
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub offset: usize,
    pub length: usize,
    pub replacement: String,
}

#[derive(Debug, Clone)]
pub struct ResolveError {
    pub message: String,
    pub span: Span,
    pub kind: ResolveErrorKind,
    pub suggestion: Option<String>,
    /// Machine-applicable edit, when one can be derived. `karac fix` walks
    /// the diagnostics produced by the resolver looking for `Some(...)`
    /// here and applies each edit to the source file. `None` means the
    /// suggestion (if any) is descriptive only — humans can act on it but
    /// no precise byte-range rewrite was synthesized. Boxed so the sparse
    /// case (most errors carry no edit) doesn't bloat the enum's payload
    /// in fallible-resolver paths that return `Result<_, ResolveError>`.
    pub replacement: Option<Box<TextEdit>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveErrorKind {
    UndefinedName,
    DuplicateDefinition,
    ReservedIdentifier,
    PrivateAccess,
    UndefinedType,
    UndefinedVariant,
    UndefinedField,
    UndefinedLabel,
    /// User-defined `impl Add for MyType` (and peers) — operator-trait
    /// implementations are restricted to stdlib types in v1.
    OperatorTraitImplRestricted,
    /// User-defined `impl Into[T] for S` / `impl TryInto[T] for S` —
    /// rejected because these are derived from `From` / `TryFrom` via blanket
    /// impl. The user must implement `From` / `TryFrom` instead.
    IntoTraitImplNotAllowed,
    /// `impl[..., with E] Trait for Type` — impl-level effect-variable
    /// quantification is not supported. Effect polymorphism is expressed
    /// via `with _` on the trait method declaration, not bound at impl
    /// level. See design.md § Conversion Traits.
    ImplLevelEffectVarNotAllowed,
    /// `import a.b.c;` — the prefix `a.b` does not match any module in the
    /// project graph (CR-24 slice 5, `E0224`).
    UnknownModule,
    /// `import a.b.Item;` — `a.b` exists but has no top-level `Item`, and
    /// `a.b.Item` is not itself a module (CR-24 slice 5, `E0225`).
    UnknownItemInModule,
    /// Cross-module visibility violation: the imported or referenced item is
    /// declared `private` (same-directory-only) and the importer lives in a
    /// different directory (CR-24 slice 6, `E0222`).
    PrivateItemAccess,
    /// `effect resource CompileTimeEnv;` or `effect resource CompileTimeHeap;`
    /// — these names are reserved for the deferred comptime feature (`E0228`).
    ReservedEffectResource,
    /// `#[compiler_builtin]` on an item in user code. The attribute is
    /// reserved for stdlib source baked into the compiler binary
    /// (CR-202 slice 1). `E0237`.
    CompilerBuiltinReserved,
    /// `continue label` where `label` refers to a labeled block (rather
    /// than a loop). `continue` is only valid for loop labels — reject
    /// the use site with `error[E_CONTINUE_LABEL_BLOCK]`. The diagnostic
    /// carries a secondary span pointing at the labeled-block declaration
    /// so users can rename the label or restructure the block.
    /// See design.md § Loops > "Labeled blocks".
    ContinueOnBlockLabel,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

// ── Result ──────────────────────────────────────────────────────

pub struct ResolveResult {
    pub resolutions: HashMap<SpanKey, SymbolId>,
    pub symbol_table: SymbolTable,
    pub errors: Vec<ResolveError>,
}

// ── Cross-module lookup helpers (CR-24 slice 5) ─────────────────

/// True iff the module at `path` exposes a top-level item called `name` —
/// either as a real item or via a `pub import` re-export (slice 7).
/// Visibility is **not** enforced here — `E0221` / `E0222` layer on top.
pub(crate) fn module_exposes_name(tree: &ProgramTree, path: &[String], name: &str) -> bool {
    module::module_exposes_item(tree, path, name)
}

pub(crate) fn module_top_level_names(tree: &ProgramTree, path: &[String]) -> Vec<String> {
    match tree.graph.by_path.get(path) {
        Some(&id) => module_top_level_names_for_id(tree, id),
        None => Vec::new(),
    }
}

/// List the names a module exposes to other modules — real top-level items
/// plus `pub import` re-exports (slice 7). Submodule re-exports are excluded
/// because they are not items.
fn module_top_level_names_for_id(tree: &ProgramTree, id: ModuleId) -> Vec<String> {
    let module = tree.module(id);
    let mut names = Vec::new();
    for item in &module.items {
        match item {
            Item::Function(f) => names.push(f.name.clone()),
            Item::StructDef(s) => names.push(s.name.clone()),
            Item::EnumDef(e) => names.push(e.name.clone()),
            Item::TraitDef(t) => names.push(t.name.clone()),
            Item::TraitAlias(t) => names.push(t.name.clone()),
            Item::MarkerTrait(t) => names.push(t.name.clone()),
            Item::ConstDecl(c) => names.push(c.name.clone()),
            Item::TypeAlias(t) => names.push(t.name.clone()),
            Item::DistinctType(d) => names.push(d.name.clone()),
            Item::ExternFunction(e) => names.push(e.name.clone()),
            Item::ExternBlock(b) => {
                for it in &b.items {
                    match it {
                        ExternItem::Function(f) => names.push(f.name.clone()),
                        ExternItem::OpaqueType(o) => names.push(o.name.clone()),
                    }
                }
            }
            Item::EffectResource(r) => names.push(r.name.clone()),
            Item::EffectGroup(g) => names.push(g.name.clone()),
            Item::EffectVerbDecl(v) => names.push(v.verb_name.clone()),
            Item::Import(imp) if imp.is_pub => {
                // `pub import` re-exports expose each bound item name at
                // the re-exporter's public surface. Submodule re-exports
                // are filtered out — they are module paths, not items.
                for ii in &imp.items {
                    let bound = ii.alias.clone().unwrap_or_else(|| ii.name.clone());
                    let mut full = imp.path.clone();
                    full.push(ii.name.clone());
                    if tree.graph.lookup(&full).is_none() {
                        names.push(bound);
                    }
                }
            }
            // Enum variants surface through their parent enum; impl blocks
            // aren't top-level named items; non-`pub` imports stay internal;
            // use / alias / independent / layout have no importable name.
            Item::ImplBlock(_)
            | Item::LayoutDef(_)
            | Item::UseDecl(_)
            | Item::Import(_)
            | Item::AliasDecl(_)
            | Item::IndependentDecl(_) => {}
        }
    }
    names
}

/// Find the `Visibility` that `name` has when looked up at `path` from an
/// outside module — following `pub import` re-export chains to the canonical
/// defining item. Returns `None` when the module or the item does not exist
/// (the slice-5 `E0224`/`E0225` diagnostics already cover those cases).
pub(crate) fn module_item_visibility(
    tree: &ProgramTree,
    path: &[String],
    name: &str,
) -> Option<Visibility> {
    module::canonical_item_visibility(tree, path, name)
}

/// The directory in the crate tree that holds this module's source file.
/// Entry files (`main.kara` / `lib.kara`) and top-level modules all live in
/// `src/` — represented as an empty path. A nested module like
/// `db.connection` lives in `db/` — its directory is `["db"]`. Test files
/// (`foo_test.kara`) share the directory of their sibling per design.md.
///
/// Implementation: the walker already hoists entry files to the empty path,
/// so we just drop the last segment of the module path to recover the
/// directory. The "test file shares directory" rule falls out of the walker's
/// `is_test_file` classification: test files share the same `ModulePath` as
/// their subject, so this function returns the same directory for them.
fn module_directory(path: &[String]) -> Vec<String> {
    if path.is_empty() {
        Vec::new()
    } else {
        path[..path.len() - 1].to_vec()
    }
}

/// True iff an item declared in module `def_path` with visibility `vis` is
/// accessible from module `use_path`, per design.md § Three-level
/// visibility. `Default` is project-internal (always OK in v1's
/// single-package mode); `Private` requires same parent directory; `Pub` is
/// always OK.
pub(crate) fn visibility_allows_access(
    vis: Visibility,
    def_path: &[String],
    use_path: &[String],
) -> bool {
    match vis {
        Visibility::Pub | Visibility::Default => true,
        Visibility::Private => module_directory(def_path) == module_directory(use_path),
    }
}

// ── Resolver ────────────────────────────────────────────────────

pub struct Resolver<'a> {
    pub(crate) program: &'a Program,
    /// Optional multi-module context. When set, `import` declarations are
    /// validated against the project-wide `ProgramTree`; when unset (single-
    /// file mode), imports are silently registered without cross-module
    /// lookup. CR-24 slice 5.
    pub(crate) tree: Option<&'a ProgramTree>,
    /// The id of the module being resolved, used to exclude self from
    /// sibling-lookup diagnostics. Set iff `tree` is set.
    pub(crate) current_module: Option<ModuleId>,
    pub(crate) table: SymbolTable,
    pub(crate) resolutions: HashMap<SpanKey, SymbolId>,
    pub(crate) errors: Vec<ResolveError>,
    /// The target type name when inside an impl block.
    pub(crate) current_impl_type: Option<String>,
    /// Stack of label-stack entries for validating `break` / `continue`
    /// targets. Each entry is `(name, kind)` where `name: Option<String>`
    /// is `None` for an unlabeled loop and `Some(label)` for a labeled
    /// loop / labeled block; `kind: LabelKind` distinguishes loops from
    /// labeled blocks. `continue label` referring to a `Block` entry is
    /// rejected with `error[E_CONTINUE_LABEL_BLOCK]`. The stack is
    /// reset to empty at each closure boundary (LB4 — labels are lexical
    /// to the function-body control flow; closure bodies cannot target
    /// outer labels).
    pub(crate) loop_labels: Vec<(Option<String>, LabelKind)>,
    /// True iff the program being resolved is the synthetic stdlib package
    /// (baked into the compiler binary by CR-202 slice 3). When false,
    /// `#[compiler_builtin]` on any item is rejected with `E0237`. The flag
    /// has no other effect — name resolution semantics are otherwise
    /// identical between user code and stdlib source.
    pub(crate) is_stdlib_source: bool,
}

impl<'a> Resolver<'a> {
    pub fn new(program: &'a Program) -> Self {
        Resolver {
            program,
            tree: None,
            current_module: None,
            table: SymbolTable::new(),
            resolutions: HashMap::new(),
            errors: Vec::new(),
            current_impl_type: None,
            loop_labels: Vec::new(),
            is_stdlib_source: false,
        }
    }

    /// Attach a project-wide `ProgramTree` so `import` declarations can be
    /// validated across modules. Use [`Resolver::new`] followed by
    /// `.with_tree(tree, module_id)` when resolving a specific module in the
    /// project.
    pub fn with_tree(mut self, tree: &'a ProgramTree, module_id: ModuleId) -> Self {
        self.tree = Some(tree);
        self.current_module = Some(module_id);
        self
    }

    /// Mark the program as stdlib source (the synthetic package baked into
    /// the compiler binary by CR-202 slice 3). When set, `#[compiler_builtin]`
    /// is permitted; when unset (the default), it is rejected with `E0237`.
    pub fn with_stdlib_source(mut self, is_stdlib: bool) -> Self {
        self.is_stdlib_source = is_stdlib;
        self
    }

    pub fn resolve(mut self) -> ResolveResult {
        // Pass 1: collect all top-level declarations
        self.collect_top_level_items();
        // Pass 1.5: validate layout blocks against collected struct definitions
        self.validate_layouts();
        // Pass 2: resolve all bodies
        self.resolve_items();

        ResolveResult {
            resolutions: self.resolutions,
            symbol_table: self.table,
            errors: self.errors,
        }
    }

    fn error_undefined_name(&mut self, name: &str, span: Span) {
        let visible = self.table.visible_names();
        let suggestion = suggest_similar(name, &visible);
        let mut message = format!("undefined name '{}'", name);
        if let Some(ref s) = suggestion {
            message.push_str(&format!(", did you mean '{}'?", s));
        }
        let replacement = suggestion.as_ref().map(|s| {
            Box::new(TextEdit {
                offset: span.offset,
                length: span.length,
                replacement: s.clone(),
            })
        });
        self.errors.push(ResolveError {
            message,
            span,
            kind: ResolveErrorKind::UndefinedName,
            suggestion,
            replacement,
        });
    }

    fn error_undefined_type(&mut self, name: &str, span: Span) {
        let visible = self.table.visible_names();
        let suggestion = suggest_similar(name, &visible);
        let mut message = format!("undefined type '{}'", name);
        if let Some(ref s) = suggestion {
            message.push_str(&format!(", did you mean '{}'?", s));
        }
        let replacement = suggestion.as_ref().map(|s| {
            Box::new(TextEdit {
                offset: span.offset,
                length: span.length,
                replacement: s.clone(),
            })
        });
        self.errors.push(ResolveError {
            message,
            span,
            kind: ResolveErrorKind::UndefinedType,
            suggestion,
            replacement,
        });
    }

    fn record_resolution(&mut self, span: &Span, id: SymbolId) {
        self.resolutions.insert(SpanKey::from_span(span), id);
    }

    // ── Type resolution ─────────────────────────────────────────

    fn resolve_type_expr(&mut self, ty: &TypeExpr) {
        match &ty.kind {
            TypeKind::Path(path) => {
                self.resolve_path_expr(path);
            }
            TypeKind::Tuple(types) => {
                for t in types {
                    self.resolve_type_expr(t);
                }
            }
            TypeKind::Array { element, size } => {
                self.resolve_type_expr(element);
                self.resolve_expr(size);
            }
            TypeKind::Pointer { inner, .. } => {
                self.resolve_type_expr(inner);
            }
            TypeKind::FnType {
                params,
                return_type,
                effect_spec,
                is_once: _,
            } => {
                for p in params {
                    self.resolve_type_expr(p);
                }
                if let Some(ref ret) = return_type {
                    self.resolve_type_expr(ret);
                }
                if let Some(ref spec) = effect_spec {
                    match spec {
                        EffectSpec::Specific(list) => self.resolve_effect_list(list),
                        EffectSpec::Polymorphic => {}
                    }
                }
            }
            TypeKind::Ref(inner) | TypeKind::MutRef(inner) | TypeKind::Weak(inner) => {
                self.resolve_type_expr(inner);
            }
            TypeKind::MutSlice(element) => {
                self.resolve_type_expr(element);
            }
            TypeKind::Unit | TypeKind::Error => {}
        }
    }

    fn resolve_path_expr(&mut self, path: &PathExpr) {
        // Resolve the first segment as a type name
        if let Some(first) = path.segments.first() {
            if let Some(sym) = self.table.lookup(first) {
                let id = sym.id;
                self.record_resolution(&path.span, id);
            } else {
                self.error_undefined_type(first, path.span.clone());
            }
        }
        // Resolve generic args
        if let Some(ref args) = path.generic_args {
            for arg in args {
                match arg {
                    GenericArg::Type(ty) => self.resolve_type_expr(ty),
                    GenericArg::Const(expr) => self.resolve_expr(expr),
                }
            }
        }
    }

    /// Resolve the trait name and any generic args inside a `TraitBound`.
    /// Records a resolution for the trait path when found. Undefined trait
    /// names are *not* reported here — the typechecker emits a more specific
    /// "unknown trait" diagnostic during bound validation, and double-erroring
    /// would be noise.
    fn resolve_trait_bound(&mut self, bound: &TraitBound) {
        if let Some(first) = bound.path.first() {
            if let Some(sym) = self.table.lookup(first) {
                let id = sym.id;
                self.record_resolution(&bound.span, id);
            }
        }
        if let Some(ref args) = bound.generic_args {
            for arg in args {
                match arg {
                    GenericArg::Type(ty) => self.resolve_type_expr(ty),
                    GenericArg::Const(expr) => self.resolve_expr(expr),
                }
            }
        }
    }

    /// Define each generic param as a `TypeParam` symbol and record its inline
    /// bounds. Trait paths in bounds are resolved so they appear in the
    /// resolution map. Returns the mapping from param name to defined SymbolId
    /// (used by where-clause resolution to merge clause-level bounds in).
    fn define_generic_params(&mut self, generics: &GenericParams) -> HashMap<String, SymbolId> {
        let mut by_name = HashMap::new();
        for param in &generics.params {
            let kind = if param.is_const {
                SymbolKind::ConstParam
            } else {
                SymbolKind::TypeParam
            };
            match self
                .table
                .define(param.name.clone(), kind, param.span.clone(), false)
            {
                Ok(id) => {
                    self.table.record_generic_bounds(id, &param.bounds);
                    by_name.insert(param.name.clone(), id);
                }
                Err(e) => self.errors.push(e),
            }
            for bound in &param.bounds {
                self.resolve_trait_bound(bound);
            }
            // Const params reference their declared type via the source AST;
            // resolve the type expression so its references appear in the
            // resolution map alongside other resolved type expressions.
            if let Some(ty) = &param.const_type {
                self.resolve_type_expr(ty);
            }
        }
        by_name
    }

    /// Walk a where clause and merge `where T: Bound` constraints into the
    /// existing generic-param bound map. `params_by_name` lets the helper map
    /// the textual `T` to the freshly-defined param SymbolId without searching
    /// scopes (which could match an unrelated outer `T` shadowed by ours).
    /// Trait paths in bounds and equality RHS types are resolved so references
    /// land in the resolution map.
    fn resolve_where_clause(
        &mut self,
        where_clause: &WhereClause,
        params_by_name: &HashMap<String, SymbolId>,
    ) {
        for constraint in &where_clause.constraints {
            match constraint {
                WhereConstraint::TypeBound {
                    type_name, bounds, ..
                } => {
                    if let Some(&param_id) = params_by_name.get(type_name) {
                        self.table.record_generic_bounds(param_id, bounds);
                    }
                    for bound in bounds {
                        self.resolve_trait_bound(bound);
                    }
                }
                WhereConstraint::AssocTypeEq { ty, .. } => {
                    self.resolve_type_expr(ty);
                }
                WhereConstraint::ConstPredicate { expr, .. } => {
                    self.resolve_expr(expr);
                }
            }
        }
    }

    // ── Pattern resolution ──────────────────────────────────────

    fn resolve_pattern(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => {
                let _ = self.table.define(
                    name.clone(),
                    SymbolKind::Variable { is_mut: false },
                    pattern.span.clone(),
                    false,
                );
            }
            PatternKind::Literal(_) => {}
            PatternKind::Struct { path, fields } => {
                // Resolve the struct/variant path
                if let Some(first) = path.first() {
                    if let Some(sym) = self.table.lookup(first) {
                        let id = sym.id;
                        self.record_resolution(&pattern.span, id);
                    } else {
                        self.error_undefined_name(first, pattern.span.clone());
                    }
                }
                // Define field bindings
                for field in fields {
                    if let Some(ref sub_pattern) = field.pattern {
                        self.resolve_pattern(sub_pattern);
                    } else {
                        // Shorthand: field name becomes binding
                        let _ = self.table.define(
                            field.name.clone(),
                            SymbolKind::Variable { is_mut: false },
                            field.span.clone(),
                            false,
                        );
                    }
                }
            }
            PatternKind::TupleVariant { path, patterns } => {
                // Resolve the variant path
                if let Some(first) = path.first() {
                    if let Some(sym) = self.table.lookup(first) {
                        let id = sym.id;
                        self.record_resolution(&pattern.span, id);
                    } else {
                        self.error_undefined_name(first, pattern.span.clone());
                    }
                }
                for p in patterns {
                    self.resolve_pattern(p);
                }
            }
            PatternKind::Tuple(patterns) => {
                for p in patterns {
                    self.resolve_pattern(p);
                }
            }
            PatternKind::Or(alternatives) => {
                for alt in alternatives {
                    self.resolve_pattern(alt);
                }
            }
            PatternKind::RangePattern { .. } => {
                // No bindings to define
            }
            PatternKind::AtBinding { name, pattern } => {
                let _ = self.table.define(
                    name.clone(),
                    SymbolKind::Variable { is_mut: false },
                    pattern.span.clone(),
                    false,
                );
                self.resolve_pattern(pattern);
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for p in prefix {
                    self.resolve_pattern(p);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    let _ = self.table.define(
                        name.clone(),
                        SymbolKind::Variable { is_mut: false },
                        pattern.span.clone(),
                        false,
                    );
                }
                for p in suffix {
                    self.resolve_pattern(p);
                }
            }
        }
    }

    /// Define bindings from a let-pattern (used for `let` statements).
    fn define_pattern_bindings(&mut self, pattern: &Pattern, is_mut: bool) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                if let Err(e) = self.table.define(
                    name.clone(),
                    SymbolKind::Variable { is_mut },
                    pattern.span.clone(),
                    false,
                ) {
                    self.errors.push(e);
                }
            }
            PatternKind::Struct { path, fields } => {
                // Resolve the struct name
                if let Some(first) = path.first() {
                    if let Some(sym) = self.table.lookup(first) {
                        let id = sym.id;
                        self.record_resolution(&pattern.span, id);
                    } else {
                        self.error_undefined_name(first, pattern.span.clone());
                    }
                }
                for field in fields {
                    if let Some(ref sub_pattern) = field.pattern {
                        self.define_pattern_bindings(sub_pattern, is_mut);
                    } else {
                        let _ = self.table.define(
                            field.name.clone(),
                            SymbolKind::Variable { is_mut },
                            field.span.clone(),
                            false,
                        );
                    }
                }
            }
            PatternKind::TupleVariant { path, patterns } => {
                if let Some(first) = path.first() {
                    if let Some(sym) = self.table.lookup(first) {
                        let id = sym.id;
                        self.record_resolution(&pattern.span, id);
                    } else {
                        self.error_undefined_name(first, pattern.span.clone());
                    }
                }
                for p in patterns {
                    self.define_pattern_bindings(p, is_mut);
                }
            }
            PatternKind::Tuple(patterns) => {
                for p in patterns {
                    self.define_pattern_bindings(p, is_mut);
                }
            }
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
            PatternKind::Or(alternatives) => {
                // Bindings from first alternative (all alts should bind same names)
                if let Some(first) = alternatives.first() {
                    self.define_pattern_bindings(first, is_mut);
                }
            }
            PatternKind::AtBinding { name, pattern } => {
                if let Err(e) = self.table.define(
                    name.clone(),
                    SymbolKind::Variable { is_mut },
                    pattern.span.clone(),
                    false,
                ) {
                    self.errors.push(e);
                }
                self.define_pattern_bindings(pattern, is_mut);
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for p in prefix {
                    self.define_pattern_bindings(p, is_mut);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    if let Err(e) = self.table.define(
                        name.clone(),
                        SymbolKind::Variable { is_mut },
                        pattern.span.clone(),
                        false,
                    ) {
                        self.errors.push(e);
                    }
                }
                for p in suffix {
                    self.define_pattern_bindings(p, is_mut);
                }
            }
        }
    }

    // ── Effect resolution ───────────────────────────────────────

    fn resolve_effect_list(&mut self, effects: &EffectList) {
        for item in &effects.items {
            match item {
                EffectItem::Verb(verb) => {
                    self.resolve_effect_verb(verb);
                }
                EffectItem::Group(name) => {
                    if let Some(sym) = self.table.lookup(name) {
                        let id = sym.id;
                        self.record_resolution(&effects.span, id);
                    } else {
                        self.error_undefined_name(name, effects.span.clone());
                    }
                }
                EffectItem::Polymorphic => {}
                EffectItem::Variable(_) => {} // declared in [with E]; no resolution needed
            }
        }
    }

    fn resolve_effect_verb(&mut self, verb: &EffectVerb) {
        for resource in &verb.resources {
            let name = resource.path.join(".");
            let first = resource.path.first().map(|s| s.as_str()).unwrap_or("");
            if let Some(sym) = self.table.lookup(first) {
                let id = sym.id;
                self.record_resolution(&resource.span, id);
            } else {
                self.errors.push(ResolveError {
                    message: format!("undefined effect resource '{}'", name),
                    span: resource.span.clone(),
                    kind: ResolveErrorKind::UndefinedName,
                    suggestion: None,
                    replacement: None,
                });
            }
            // Resolve parameterized resource expression
            if let Some(ref param_expr) = resource.param {
                self.resolve_expr(param_expr);
            }
        }
    }
}
