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
use std::collections::{HashMap, HashSet};

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
fn module_exposes_name(tree: &ProgramTree, path: &[String], name: &str) -> bool {
    module::module_exposes_item(tree, path, name)
}

fn module_top_level_names(tree: &ProgramTree, path: &[String]) -> Vec<String> {
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
fn module_item_visibility(tree: &ProgramTree, path: &[String], name: &str) -> Option<Visibility> {
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
fn visibility_allows_access(vis: Visibility, def_path: &[String], use_path: &[String]) -> bool {
    match vis {
        Visibility::Pub | Visibility::Default => true,
        Visibility::Private => module_directory(def_path) == module_directory(use_path),
    }
}

// ── Resolver ────────────────────────────────────────────────────

pub struct Resolver<'a> {
    program: &'a Program,
    /// Optional multi-module context. When set, `import` declarations are
    /// validated against the project-wide `ProgramTree`; when unset (single-
    /// file mode), imports are silently registered without cross-module
    /// lookup. CR-24 slice 5.
    tree: Option<&'a ProgramTree>,
    /// The id of the module being resolved, used to exclude self from
    /// sibling-lookup diagnostics. Set iff `tree` is set.
    current_module: Option<ModuleId>,
    table: SymbolTable,
    resolutions: HashMap<SpanKey, SymbolId>,
    errors: Vec<ResolveError>,
    /// The target type name when inside an impl block.
    current_impl_type: Option<String>,
    /// Stack of label-stack entries for validating `break` / `continue`
    /// targets. Each entry is `(name, kind)` where `name: Option<String>`
    /// is `None` for an unlabeled loop and `Some(label)` for a labeled
    /// loop / labeled block; `kind: LabelKind` distinguishes loops from
    /// labeled blocks. `continue label` referring to a `Block` entry is
    /// rejected with `error[E_CONTINUE_LABEL_BLOCK]`. The stack is
    /// reset to empty at each closure boundary (LB4 — labels are lexical
    /// to the function-body control flow; closure bodies cannot target
    /// outer labels).
    loop_labels: Vec<(Option<String>, LabelKind)>,
    /// True iff the program being resolved is the synthetic stdlib package
    /// (baked into the compiler binary by CR-202 slice 3). When false,
    /// `#[compiler_builtin]` on any item is rejected with `E0237`. The flag
    /// has no other effect — name resolution semantics are otherwise
    /// identical between user code and stdlib source.
    is_stdlib_source: bool,
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

    // ── Pass 1: Top-level declaration collection ────────────────

    fn collect_top_level_items(&mut self) {
        for item in &self.program.items {
            match item {
                Item::Function(f) => self.collect_function(f),
                Item::StructDef(s) => self.collect_struct(s),
                Item::EnumDef(e) => self.collect_enum(e),
                Item::TraitDef(t) => self.collect_trait(t),
                Item::TraitAlias(t) => self.collect_trait_alias(t),
                Item::MarkerTrait(t) => self.collect_marker_trait(t),
                Item::ImplBlock(i) => self.collect_impl(i),
                Item::EffectResource(e) => self.collect_effect_resource(e),
                Item::EffectGroup(e) => self.collect_effect_group(e),
                Item::EffectVerbDecl(e) => self.collect_effect_verb(e),
                Item::ConstDecl(c) => self.collect_const(c),
                Item::TypeAlias(t) => self.collect_type_alias(t),
                Item::UseDecl(u) => self.collect_use(u),
                Item::Import(i) => self.collect_import(i),
                Item::ExternFunction(e) => self.collect_extern_function(e),
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(f) => self.collect_extern_function(f),
                        }
                    }
                }
                Item::DistinctType(d) => self.collect_distinct_type(d),
                Item::LayoutDef(_) | Item::AliasDecl(_) | Item::IndependentDecl(_) => {}
            }
        }
    }

    fn validate_layouts(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            if let Item::LayoutDef(layout) = item {
                self.validate_layout(layout);
            }
        }
    }

    fn validate_layout(&mut self, layout: &LayoutDef) {
        // Extract the element type name from the collection type.
        // e.g., Vec[Entity] → "Entity", Array[Entity, 100] → "Entity"
        let struct_name = match &layout.collection_type.kind {
            TypeKind::Path(path) => {
                let coll_name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if coll_name != "Vec" && coll_name != "Array" {
                    self.errors.push(ResolveError {
                        message: format!(
                            "layout collection type must be Vec[T] or Array[T, N], found '{}'",
                            coll_name
                        ),
                        span: layout.span.clone(),
                        kind: ResolveErrorKind::UndefinedType,
                        suggestion: None,
                        replacement: None,
                    });
                    return;
                }
                // Extract the element type from generic args.
                match &path.generic_args {
                    Some(args) if !args.is_empty() => {
                        if let GenericArg::Type(te) = &args[0] {
                            if let TypeKind::Path(inner) = &te.kind {
                                inner.segments.first().cloned()
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        let struct_name = match struct_name {
            Some(n) => n,
            None => {
                self.errors.push(ResolveError {
                    message:
                        "layout collection type must specify an element type (e.g., Vec[Entity])"
                            .to_string(),
                    span: layout.span.clone(),
                    kind: ResolveErrorKind::UndefinedType,
                    suggestion: None,
                    replacement: None,
                });
                return;
            }
        };

        // Look up the struct's field names.
        let struct_fields: Vec<String> = if let Some(sym) =
            self.table.lookup_in_scope(ScopeId(0), &struct_name)
        {
            if let SymbolKind::Struct { field_names } = &sym.kind {
                field_names.clone()
            } else if let SymbolKind::Enum { .. } = &sym.kind {
                // Validate split_by_variant for enums.
                for item in &layout.items {
                    if let LayoutItem::Group { name, span, .. } = item {
                        self.errors.push(ResolveError {
                                message: format!(
                                    "layout group '{}' is not allowed for enum types; use split_by_variant",
                                    name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: Some("use split_by_variant instead of group".to_string()),
                                replacement: None,
                            });
                    }
                }
                return;
            } else {
                self.errors.push(ResolveError {
                    message: format!("'{}' is not a struct", struct_name),
                    span: layout.span.clone(),
                    kind: ResolveErrorKind::UndefinedType,
                    suggestion: None,
                    replacement: None,
                });
                return;
            }
        } else {
            self.errors.push(ResolveError {
                message: format!("undefined struct '{}' in layout definition", struct_name),
                span: layout.span.clone(),
                kind: ResolveErrorKind::UndefinedType,
                suggestion: None,
                replacement: None,
            });
            return;
        };

        // Validate layout items: field existence, uniqueness, cold constraints, align(N).
        let mut assigned: HashSet<String> = HashSet::new();
        let mut cold_count = 0usize;
        for item in &layout.items {
            match item {
                LayoutItem::Group {
                    name,
                    fields,
                    align,
                    span,
                } => {
                    for field in fields {
                        if !struct_fields.contains(field) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' does not exist on struct '{}' (in group '{}')",
                                    field, struct_name, name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: None,
                                replacement: None,
                            });
                        } else if !assigned.insert(field.clone()) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' appears in multiple sections in layout '{}'",
                                    field, layout.name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::DuplicateDefinition,
                                suggestion: None,
                                replacement: None,
                            });
                        }
                    }
                    if let Some(n) = align {
                        if *n == 0 || (*n & (*n - 1)) != 0 {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "align({}) is not a power of two in layout '{}' group '{}'",
                                    n, layout.name, name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: Some(
                                    "common values: 8, 16, 32, 64 (cache line), 128 (Apple Silicon cache line)".to_string(),
                                ),
                                replacement: None,
                            });
                        }
                    }
                }
                LayoutItem::Cold { fields, span } => {
                    cold_count += 1;
                    if cold_count > 1 {
                        self.errors.push(ResolveError {
                            message: format!(
                                "layout '{}' has more than one cold section; at most one is allowed",
                                layout.name
                            ),
                            span: span.clone(),
                            kind: ResolveErrorKind::DuplicateDefinition,
                            suggestion: None,
                            replacement: None,
                        });
                    }
                    for field in fields {
                        if !struct_fields.contains(field) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' does not exist on struct '{}' (in cold section)",
                                    field, struct_name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: None,
                                replacement: None,
                            });
                        } else if !assigned.insert(field.clone()) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' appears in multiple sections in layout '{}'",
                                    field, layout.name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::DuplicateDefinition,
                                suggestion: None,
                                replacement: None,
                            });
                        }
                    }
                }
                LayoutItem::SplitByVariant(span) => {
                    self.errors.push(ResolveError {
                        message: "split_by_variant is only valid for enum layout blocks"
                            .to_string(),
                        span: span.clone(),
                        kind: ResolveErrorKind::UndefinedField,
                        suggestion: None,
                        replacement: None,
                    });
                }
            }
        }

        // Warn about unassigned fields (fields not in any group or cold section).
        let unassigned: Vec<&String> = struct_fields
            .iter()
            .filter(|f| !assigned.contains(*f))
            .collect();
        if !unassigned.is_empty() {
            // TODO: Implement proper warning severity and #[allow(layout_unassigned_fields)].
            let field_list = unassigned
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.errors.push(ResolveError {
                message: format!(
                    "layout '{}': fields not assigned to any group or cold section: {}. These will be placed in an implicit trailing hot group.",
                    layout.name, field_list
                ),
                span: layout.span.clone(),
                kind: ResolveErrorKind::UndefinedField,
                suggestion: Some("assign all fields to groups, or suppress with #[allow(layout_unassigned_fields)]".to_string()),
                replacement: None,
            });
        }
    }

    fn check_compiler_builtin_attr(&mut self, attrs: &[Attribute], item_stdlib_origin: bool) {
        // The gate bypasses (a) when the whole resolver session is in
        // stdlib-source mode (CR-202 slice 1's `with_stdlib_source(true)`
        // builder), or (b) when the individual item carries the per-item
        // stdlib-origin tag (CR-202 slice 3b). The per-item tag is what
        // 3c uses to flip baked stdlib items spliced into a user-mode
        // program tree — the resolver session for that tree stays
        // user-mode, but individual baked items get an exemption.
        if self.is_stdlib_source || item_stdlib_origin {
            return;
        }
        for attr in attrs {
            if attr.name == "compiler_builtin" {
                self.errors.push(ResolveError {
                    message: "`#[compiler_builtin]` is reserved for stdlib source baked into the compiler binary"
                        .to_string(),
                    span: attr.span.clone(),
                    kind: ResolveErrorKind::CompilerBuiltinReserved,
                    suggestion: None,
                    replacement: None,
                });
            }
        }
    }

    fn collect_function(&mut self, f: &Function) {
        self.check_compiler_builtin_attr(&f.attributes, f.stdlib_origin);
        let param_names: Vec<String> = f
            .params
            .iter()
            .flat_map(|p| p.pattern.binding_names())
            .collect();
        if let Err(e) = self.table.define(
            f.name.clone(),
            SymbolKind::Function { param_names },
            f.span.clone(),
            f.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_struct(&mut self, s: &StructDef) {
        self.check_compiler_builtin_attr(&s.attributes, s.stdlib_origin);
        let field_names: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();
        if let Err(e) = self.table.define(
            s.name.clone(),
            SymbolKind::Struct { field_names },
            s.span.clone(),
            s.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_enum(&mut self, e: &EnumDef) {
        self.check_compiler_builtin_attr(&e.attributes, e.stdlib_origin);
        let variant_names: Vec<String> = e.variants.iter().map(|v| v.name.clone()).collect();
        let enum_id = match self.table.define(
            e.name.clone(),
            SymbolKind::Enum { variant_names },
            e.span.clone(),
            e.is_pub,
        ) {
            Ok(id) => id,
            Err(err) => {
                self.errors.push(err);
                return;
            }
        };

        // Register each variant in global scope
        for variant in &e.variants {
            let variant_kind = match &variant.kind {
                VariantKind::Unit => VariantSymbolKind::Unit,
                VariantKind::Tuple(types) => VariantSymbolKind::Tuple(types.len()),
                VariantKind::Struct(fields) => {
                    VariantSymbolKind::Struct(fields.iter().map(|f| f.name.clone()).collect())
                }
            };
            // Try to register variant name directly; if collision, that's ok —
            // user must use qualified path
            let _ = self.table.define(
                variant.name.clone(),
                SymbolKind::EnumVariant {
                    parent_enum: enum_id,
                    variant_kind,
                },
                variant.span.clone(),
                e.is_pub,
            );
        }
    }

    fn collect_trait(&mut self, t: &TraitDef) {
        self.check_compiler_builtin_attr(&t.attributes, t.stdlib_origin);
        let method_names: Vec<String> = t
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Method(m) => Some(m.name.clone()),
                TraitItem::AssocType(_) => None,
            })
            .collect();
        if let Err(e) = self.table.define(
            t.name.clone(),
            SymbolKind::Trait { method_names },
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_trait_alias(&mut self, t: &TraitAliasDef) {
        if let Err(e) = self.table.define(
            t.name.clone(),
            SymbolKind::TraitAlias,
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_marker_trait(&mut self, t: &MarkerTraitDef) {
        // Marker traits register in the trait namespace alongside ordinary
        // traits; no methods to track, so the symbol carries an empty
        // method list. Trait-bound resolution and impl coherence treat
        // markers identically to ordinary traits — the marker-ness is a
        // definition-site property, not a use-site property.
        if let Err(e) = self.table.define(
            t.name.clone(),
            SymbolKind::Trait {
                method_names: Vec::new(),
            },
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_impl(&mut self, imp: &ImplBlock) {
        // `#[compiler_builtin]` is reserved for stdlib source — user code
        // is rejected (E0237). The other `collect_*` callers gate top-level
        // items; impl blocks need the same gate at two levels: the block
        // itself, and each method. ImplBlock has no `stdlib_origin` field
        // because baked stdlib impls live in `STDLIB_PROGRAMS` and are
        // walked by the typechecker/interpreter directly — they never
        // reach this resolver path. So `false` here is correct: any impl
        // block walked by `collect_impl` is user-authored, and a
        // session-wide stdlib-source resolver bypasses via `is_stdlib_source`.
        // Methods carry their own `stdlib_origin` (inherited from the
        // `Function` field) and pass it through for the per-item exemption.
        self.check_compiler_builtin_attr(&imp.attributes, false);
        // Methods are registered in type_methods, not global scope.
        // We need the type name from the target_type.
        let type_name = self.type_expr_name(&imp.target_type);
        if let Some(type_name) = type_name {
            for item in &imp.items {
                let method = match item {
                    ImplItem::Method(m) => m,
                    ImplItem::AssocType(_) => continue,
                };
                self.check_compiler_builtin_attr(&method.attributes, method.stdlib_origin);
                let param_names: Vec<String> = method
                    .params
                    .iter()
                    .flat_map(|p| p.pattern.binding_names())
                    .collect();
                let method_id = SymbolId(self.table.symbols.len());
                self.table.symbols.push(Symbol {
                    id: method_id,
                    name: method.name.clone(),
                    kind: SymbolKind::Function { param_names },
                    span: method.span.clone(),
                    is_pub: method.is_pub,
                    scope: self.table.current_scope,
                });
                self.table.register_method(&type_name, method_id);
            }
        }
    }

    fn collect_effect_resource(&mut self, e: &EffectResourceDecl) {
        const RESERVED: &[&str] = &["CompileTimeEnv", "CompileTimeHeap"];
        if RESERVED.contains(&e.name.as_str()) {
            self.errors.push(ResolveError {
                message: format!(
                    "resource name '{}' is reserved for the deferred comptime feature; \
                     see deferred.md § Comptime Effect Defaults",
                    e.name
                ),
                span: e.span.clone(),
                kind: ResolveErrorKind::ReservedEffectResource,
                suggestion: None,
                replacement: None,
            });
            return;
        }
        if let Err(err) = self.table.define(
            e.name.clone(),
            SymbolKind::EffectResource,
            e.span.clone(),
            true, // effect resources are always accessible
        ) {
            self.errors.push(err);
        }
    }

    fn collect_effect_group(&mut self, e: &EffectGroupDecl) {
        if let Err(err) = self.table.define(
            e.name.clone(),
            SymbolKind::EffectGroup,
            e.span.clone(),
            e.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_effect_verb(&mut self, e: &EffectVerbDecl) {
        if let Err(err) = self.table.define(
            e.verb_name.clone(),
            SymbolKind::EffectVerb,
            e.span.clone(),
            true,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_const(&mut self, c: &ConstDecl) {
        if let Err(err) = self.table.define(
            c.name.clone(),
            SymbolKind::Constant,
            c.span.clone(),
            c.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_type_alias(&mut self, t: &TypeAliasDef) {
        if let Err(err) = self.table.define(
            t.name.clone(),
            SymbolKind::TypeAlias,
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_distinct_type(&mut self, d: &crate::ast::DistinctTypeDef) {
        if let Err(err) = self.table.define(
            d.name.clone(),
            SymbolKind::DistinctType,
            d.span.clone(),
            d.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_use(&mut self, u: &UseDecl) {
        if let Some(last) = u.path.last() {
            if let Err(err) = self.table.define(
                last.clone(),
                SymbolKind::Import {
                    path: u.path.clone(),
                },
                u.span.clone(),
                u.is_pub,
            ) {
                self.errors.push(err);
            }
        }
    }

    /// Resolve a CR-24 `import` declaration against the `ProgramTree`:
    ///
    /// 1. Ensure the dotted prefix in `imp.path` names a module in the graph
    ///    (`E0224 UnknownModule` on miss, with a Levenshtein suggestion over
    ///    all known module paths).
    /// 2. For each brace-listed item, bind `alias.or(name)` in the current
    ///    scope as `SymbolKind::Import { path }`. If the `path + name` is
    ///    itself a submodule, the binding is a module reference; otherwise
    ///    the target module must expose a matching top-level item
    ///    (`E0225 UnknownItemInModule` on miss).
    ///
    /// Single-file mode (no tree attached) skips cross-module validation and
    /// just registers the symbol so downstream passes see the name in scope.
    /// This keeps `karac run file.kara` unchanged.
    fn collect_import(&mut self, imp: &ImportDecl) {
        let Some(tree) = self.tree else {
            // Single-file mode — bind without validation.
            for item in &imp.items {
                let bound = item.alias.clone().unwrap_or_else(|| item.name.clone());
                let mut full = imp.path.clone();
                full.push(item.name.clone());
                if let Err(e) = self.table.define(
                    bound,
                    SymbolKind::Import { path: full },
                    item.span.clone(),
                    imp.is_pub,
                ) {
                    self.errors.push(e);
                }
            }
            return;
        };

        // Validate the dotted prefix exists.
        if !tree.graph.by_path.contains_key(&imp.path) && !imp.path.is_empty() {
            let wanted = imp.path.join(".");
            let all_paths: Vec<String> = tree
                .graph
                .by_path
                .keys()
                .map(|p| {
                    if p.is_empty() {
                        "<crate>".to_string()
                    } else {
                        p.join(".")
                    }
                })
                .collect();
            let candidates: Vec<&str> = all_paths.iter().map(|s| s.as_str()).collect();
            let suggestion = suggest_similar(&wanted, &candidates);
            let mut message = format!("unknown module `{wanted}`");
            if let Some(ref s) = suggestion {
                message.push_str(&format!(", did you mean `{s}`?"));
            }
            // The replacement covers exactly the dotted prefix tokens in
            // source — `imp.path_spans` is the per-segment span vector
            // from the parser, paired with `imp.path`. A non-empty
            // `imp.path` (guarded above) therefore implies a non-empty
            // `path_spans`, and the contiguous span runs from the first
            // segment's offset to the last segment's end.
            let replacement = suggestion.as_ref().and_then(|s| {
                let first = imp.path_spans.first()?;
                let last = imp.path_spans.last()?;
                let offset = first.offset;
                let length = (last.offset + last.length).saturating_sub(offset);
                Some(Box::new(TextEdit {
                    offset,
                    length,
                    replacement: s.clone(),
                }))
            });
            self.errors.push(ResolveError {
                message,
                span: imp.span.clone(),
                kind: ResolveErrorKind::UnknownModule,
                suggestion,
                replacement,
            });
            // Still register names locally so downstream passes do not
            // compound with cascading UndefinedName errors.
            for item in &imp.items {
                let bound = item.alias.clone().unwrap_or_else(|| item.name.clone());
                let mut full = imp.path.clone();
                full.push(item.name.clone());
                if let Err(e) = self.table.define(
                    bound,
                    SymbolKind::Import { path: full },
                    item.span.clone(),
                    imp.is_pub,
                ) {
                    self.errors.push(e);
                }
            }
            return;
        }

        // Prefix exists. Look up each brace-listed item.
        let importer_path = self
            .current_module
            .map(|id| tree.module(id).path.clone())
            .unwrap_or_default();
        for item in &imp.items {
            let mut full = imp.path.clone();
            full.push(item.name.clone());

            let binds_submodule = tree.graph.by_path.contains_key(&full);
            let binds_item = !binds_submodule && module_exposes_name(tree, &imp.path, &item.name);

            if !binds_submodule && !binds_item {
                // Look at target module's top-level items for suggestions,
                // plus any submodule siblings.
                let mut candidates_owned: Vec<String> = module_top_level_names(tree, &imp.path);
                for p in tree.graph.by_path.keys() {
                    if p.len() == imp.path.len() + 1 && p.starts_with(&imp.path) {
                        candidates_owned.push(p.last().cloned().unwrap_or_default());
                    }
                }
                let candidates: Vec<&str> = candidates_owned.iter().map(|s| s.as_str()).collect();
                let suggestion = suggest_similar(&item.name, &candidates);

                let module_label = if imp.path.is_empty() {
                    "<crate root>".to_string()
                } else {
                    imp.path.join(".")
                };
                let mut message =
                    format!("unknown item `{}` in module `{module_label}`", item.name);
                if let Some(ref s) = suggestion {
                    message.push_str(&format!(", did you mean `{s}`?"));
                } else if candidates_owned.len() <= 10 && !candidates_owned.is_empty() {
                    // Design.md § Path resolution algorithm — for small
                    // modules, list the available exports.
                    message.push_str(&format!("; available: {}", candidates_owned.join(", ")));
                }
                let replacement = suggestion.as_ref().map(|s| {
                    Box::new(TextEdit {
                        offset: item.span.offset,
                        length: item.span.length,
                        replacement: s.clone(),
                    })
                });
                self.errors.push(ResolveError {
                    message,
                    span: item.span.clone(),
                    kind: ResolveErrorKind::UnknownItemInModule,
                    suggestion,
                    replacement,
                });
            } else if binds_item {
                // Slice 6 + 7: enforce three-level visibility against the
                // canonical defining item. Following the `pub import` chain
                // ensures re-exports are transparent — E0222 fires based on
                // where the item is really defined, not the re-exporter's
                // location. Submodule bindings (`binds_submodule`) have no
                // item visibility to check.
                let (def_path, def_name) =
                    match module::canonical_origin(tree, &imp.path, &item.name) {
                        Some(p) => p,
                        None => (imp.path.clone(), item.name.clone()),
                    };
                if let Some(vis) = module_item_visibility(tree, &imp.path, &item.name) {
                    if !visibility_allows_access(vis, &def_path, &importer_path) {
                        let def_label = if def_path.is_empty() {
                            "<crate root>".to_string()
                        } else {
                            def_path.join(".")
                        };
                        let message = format!(
                            "`{}` in module `{}` is `private` — visible only to files in the same directory",
                            def_name, def_label,
                        );
                        self.errors.push(ResolveError {
                            message,
                            span: item.span.clone(),
                            kind: ResolveErrorKind::PrivateItemAccess,
                            suggestion: Some(format!(
                                "mark `{}` as `pub` or move the caller into the same directory",
                                def_name
                            )),
                            replacement: None,
                        });
                    }
                }
            }

            // Slice 7: the bound symbol records the *canonical* path so that
            // re-exports are transparent to downstream phases — method
            // resolution, trait coherence, and typechecker cross-module
            // lookups all see a single identity regardless of which alias
            // the name travelled through.
            let canonical_full = if binds_item {
                module::canonical_origin(tree, &imp.path, &item.name)
                    .map(|(mut path, name)| {
                        path.push(name);
                        path
                    })
                    .unwrap_or_else(|| full.clone())
            } else {
                full.clone()
            };
            let bound = item.alias.clone().unwrap_or_else(|| item.name.clone());
            if let Err(e) = self.table.define(
                bound,
                SymbolKind::Import {
                    path: canonical_full,
                },
                item.span.clone(),
                imp.is_pub,
            ) {
                self.errors.push(e);
            }
        }
    }

    fn collect_extern_function(&mut self, e: &ExternFunction) {
        if let Err(err) = self.table.define(
            e.name.clone(),
            SymbolKind::ExternFunction,
            e.span.clone(),
            true,
        ) {
            self.errors.push(err);
        }
    }

    /// Extract a simple name from a type expression (for impl block target types).
    fn type_expr_name(&self, ty: &TypeExpr) -> Option<String> {
        match &ty.kind {
            TypeKind::Path(path) => path.segments.last().cloned(),
            _ => None,
        }
    }

    /// Whether `name` is declared as an associated function on at least one
    /// visible trait. Used to suppress the undefined-name error for bare
    /// identifier callees that the typechecker can dispatch via expected-type
    /// inference (e.g. `let x: T = default()` where `T: Default`).
    fn is_trait_assoc_fn_name(&self, name: &str) -> bool {
        for item in &self.program.items {
            if let Item::TraitDef(t) = item {
                for ti in &t.items {
                    if let TraitItem::Method(m) = ti {
                        if m.name == name && m.self_param.is_none() {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    // ── Pass 2: Resolve all items ───────────────────────────────

    fn resolve_items(&mut self) {
        // Clone items to avoid borrow conflict
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) => self.resolve_function(f),
                Item::StructDef(s) => self.resolve_struct_def(s),
                Item::EnumDef(e) => self.resolve_enum_def(e),
                Item::TraitDef(t) => self.resolve_trait_def(t),
                Item::ImplBlock(i) => self.resolve_impl_block(i),
                Item::ConstDecl(c) => self.resolve_const_decl(c),
                Item::TypeAlias(t) => self.resolve_type_alias_def(t),
                Item::ExternFunction(e) => self.resolve_extern_function(e),
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(f) => self.resolve_extern_function(f),
                        }
                    }
                }
                Item::EffectGroup(g) => self.resolve_effect_group_def(g),
                _ => {}
            }
        }
    }

    fn resolve_function(&mut self, f: &Function) {
        self.table.push_scope(ScopeKind::Function);

        // Register generic type params (with inline trait bounds). Where-clause
        // bounds, if any, are merged into the same per-param bound list below.
        let params_by_name = if let Some(ref generics) = f.generic_params {
            self.define_generic_params(generics)
        } else {
            HashMap::new()
        };
        if let Some(ref wc) = f.where_clause {
            self.resolve_where_clause(wc, &params_by_name);
        }

        // Register self if present
        if f.self_param.is_some() {
            let _ = self.table.define(
                "self".to_string(),
                SymbolKind::SelfValue,
                f.span.clone(),
                false,
            );
        }

        // Register parameters
        for param in &f.params {
            self.define_pattern_bindings(&param.pattern, false);
            self.resolve_type_expr(&param.ty);
        }

        // Resolve return type
        if let Some(ref ret_ty) = f.return_type {
            self.resolve_type_expr(ret_ty);
        }

        // Resolve effect annotations
        if let Some(ref effects) = f.effects {
            self.resolve_effect_list(effects);
        }

        // Resolve body
        self.resolve_block(&f.body);

        self.table.pop_scope();
    }

    fn resolve_struct_def(&mut self, s: &StructDef) {
        if let Some(ref generics) = s.generic_params {
            self.table.push_scope(ScopeKind::Block);
            let params_by_name = self.define_generic_params(generics);
            if let Some(ref wc) = s.where_clause {
                self.resolve_where_clause(wc, &params_by_name);
            }
            for field in &s.fields {
                self.resolve_type_expr(&field.ty);
            }
            self.table.pop_scope();
        } else {
            for field in &s.fields {
                self.resolve_type_expr(&field.ty);
            }
        }
    }

    fn resolve_enum_def(&mut self, e: &EnumDef) {
        if let Some(ref generics) = e.generic_params {
            self.table.push_scope(ScopeKind::Block);
            let params_by_name = self.define_generic_params(generics);
            if let Some(ref wc) = e.where_clause {
                self.resolve_where_clause(wc, &params_by_name);
            }
            self.resolve_enum_variants(&e.variants);
            self.table.pop_scope();
        } else {
            self.resolve_enum_variants(&e.variants);
        }
    }

    fn resolve_enum_variants(&mut self, variants: &[Variant]) {
        for variant in variants {
            match &variant.kind {
                VariantKind::Tuple(types) => {
                    for ty in types {
                        self.resolve_type_expr(ty);
                    }
                }
                VariantKind::Struct(fields) => {
                    for field in fields {
                        self.resolve_type_expr(&field.ty);
                    }
                }
                VariantKind::Unit => {}
            }
        }
    }

    fn resolve_trait_def(&mut self, t: &TraitDef) {
        // Push a trait-level scope that exposes `Self` (and any trait-level
        // generic params) to every method signature and default body.
        self.table.push_scope(ScopeKind::Block);
        let self_id = self
            .table
            .define(
                "Self".to_string(),
                SymbolKind::TypeParam,
                t.span.clone(),
                false,
            )
            .ok();
        // Supertrait constraints (`trait Foo: Bar + Baz`) are bounds on `Self`
        // — every `Self` value is also a `Bar` and a `Baz`. Recording them
        // here lets the typechecker dispatch `Self.method()` calls to
        // supertrait methods and bare `method()` calls in default bodies via
        // the same trait-bound machinery.
        if let Some(id) = self_id {
            self.table.record_generic_bounds(id, &t.supertraits);
        }
        for bound in &t.supertraits {
            self.resolve_trait_bound(bound);
        }
        let mut params_by_name: HashMap<String, SymbolId> = HashMap::new();
        if let Some(ref generics) = t.generic_params {
            params_by_name = self.define_generic_params(generics);
        }
        if let Some(ref wc) = t.where_clause {
            self.resolve_where_clause(wc, &params_by_name);
        }

        for item in &t.items {
            match item {
                TraitItem::Method(method) => {
                    self.table.push_scope(ScopeKind::Function);

                    // Method-level generic params + where clause.
                    let method_params = if let Some(ref mg) = method.generic_params {
                        self.define_generic_params(mg)
                    } else {
                        HashMap::new()
                    };
                    if let Some(ref wc) = method.where_clause {
                        self.resolve_where_clause(wc, &method_params);
                    }

                    // Register self if present
                    if method.self_param.is_some() {
                        let _ = self.table.define(
                            "self".to_string(),
                            SymbolKind::SelfValue,
                            method.span.clone(),
                            false,
                        );
                    }

                    for param in &method.params {
                        self.define_pattern_bindings(&param.pattern, false);
                        self.resolve_type_expr(&param.ty);
                    }

                    if let Some(ref ret_ty) = method.return_type {
                        self.resolve_type_expr(ret_ty);
                    }

                    if let Some(ref effects) = method.effects {
                        self.resolve_effect_list(effects);
                    }

                    // Resolve default method body if present
                    if let Some(ref body) = method.body {
                        self.resolve_block_no_scope(body);
                    }

                    self.table.pop_scope();
                }
                TraitItem::AssocType(_assoc) => {
                    // Associated type declarations are collected but not resolved here.
                    // Bounds on associated types are checked during type checking.
                }
            }
        }

        self.table.pop_scope();
    }

    /// Reject `impl <OperatorTrait> for <UserType>` in v1 — operator traits
    /// (Add/Sub/Eq/etc.) are stdlib-only. Lifting the restriction is a
    /// one-line edit (remove or shrink `OPERATOR_TRAIT_NAMES`).
    /// `From`/`Into` are NOT operator traits — user impls are required for
    /// `?` cross-error propagation and stay allowed.
    /// Reject `impl Into[T] for S` and `impl TryInto[T] for S`. The design
    /// models these as blanket impls derived from `From` / `TryFrom`; a direct
    /// impl would conflict with the blanket and break the `x.into()` lowering.
    /// User must write `impl From[S] for T` (or `impl TryFrom[S] for T`) instead.
    fn check_into_trait_restriction(&mut self, trait_path: &PathExpr) {
        let trait_name = match trait_path.segments.last() {
            Some(name) => name.as_str(),
            None => return,
        };
        let source_trait = match trait_name {
            "Into" => "From",
            "TryInto" => "TryFrom",
            _ => return,
        };
        self.errors.push(ResolveError {
            message: format!(
                "user-defined `impl {trait_name} for T` is not allowed; \
                 `{trait_name}` is derived from `{source_trait}` via a blanket impl"
            ),
            span: trait_path.span.clone(),
            kind: ResolveErrorKind::IntoTraitImplNotAllowed,
            suggestion: Some(format!(
                "implement `{source_trait}` instead; `x.into()` will dispatch through it"
            )),
            replacement: None,
        });
    }

    /// Reject `impl[T, U, with E] Trait[U] for T { ... }` and any other
    /// impl block that binds a named effect variable at the impl level.
    /// Effect polymorphism on trait methods is expressed by declaring the
    /// method `with _` on the trait; impl-level binding would imply a
    /// per-monomorphization rewrite that the language does not model.
    fn check_impl_level_effect_vars(&mut self, imp: &ImplBlock) {
        let generics = match &imp.generic_params {
            Some(g) if !g.effect_params.is_empty() => g,
            _ => return,
        };
        let var_list = generics
            .effect_params
            .iter()
            .map(|n| format!("`{}`", n))
            .collect::<Vec<_>>()
            .join(", ");
        self.errors.push(ResolveError {
            message: format!(
                "impl-level effect variables ({var_list}) are not supported; \
                 use `with _` on the trait method instead"
            ),
            span: generics.span.clone(),
            kind: ResolveErrorKind::ImplLevelEffectVarNotAllowed,
            suggestion: Some(
                "remove the `with E` from the impl's generic parameters and declare the \
                 trait method `with _` so impls may carry any effects"
                    .to_string(),
            ),
            replacement: None,
        });
    }

    fn check_operator_trait_restriction(&mut self, trait_path: &PathExpr, target: &TypeExpr) {
        const OPERATOR_TRAIT_NAMES: &[&str] = &[
            "Add", "Sub", "Mul", "Div", "Rem", "Neg", "Eq", "Ord", "BitAnd", "BitOr", "BitXor",
            "Shl", "Shr", "Not", "Index", "IndexMut", "Display",
        ];
        const STDLIB_ALLOWLIST: &[&str] = &[
            "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "f32", "f64", "bool",
            "char", "String", "F32", "F64",
        ];

        let trait_name = match trait_path.segments.last() {
            Some(name) => name.as_str(),
            None => return,
        };
        if !OPERATOR_TRAIT_NAMES.contains(&trait_name) {
            return;
        }

        let target_name = self.type_expr_name(target).unwrap_or_default();
        if STDLIB_ALLOWLIST.contains(&target_name.as_str()) {
            return;
        }

        // Carve-out: relational operator traits (`Eq`, `Ord`) may be implemented
        // on user-defined types. User types routinely need custom equality and
        // ordering (map keys, domain-model invariants); the general stdlib-only
        // restriction is too strict. Arithmetic, bitwise, and indexing traits
        // stay restricted until the "heterogeneous Rhs / Output" design lands.
        const RELATIONAL_TRAITS: &[&str] = &["Eq", "Ord"];
        if RELATIONAL_TRAITS.contains(&trait_name) {
            return;
        }

        // Vec[T] gets a tailored hint pointing at the explicit alternatives.
        let (message, suggestion) = if trait_name == "Add" && target_name == "Vec" {
            (
                "`impl Add for Vec[T]` is not supported by design".to_string(),
                Some("use `.concat(other)` for concatenation or `.extend(other)` for in-place append".to_string()),
            )
        } else {
            (
                format!(
                    "user-defined `impl {trait_name} for {target_name}` is not supported in v1; \
                     operator traits are stdlib-only"
                ),
                Some(format!(
                    "remove the impl block; arithmetic and comparison are dispatched through stdlib `{trait_name}` impls"
                )),
            )
        };
        self.errors.push(ResolveError {
            message,
            span: trait_path.span.clone(),
            kind: ResolveErrorKind::OperatorTraitImplRestricted,
            suggestion,
            replacement: None,
        });
    }

    fn resolve_impl_block(&mut self, imp: &ImplBlock) {
        let type_name = self.type_expr_name(&imp.target_type).unwrap_or_default();

        self.current_impl_type = Some(type_name.clone());
        self.table.push_scope(ScopeKind::Impl {
            target_type: type_name,
        });

        // Register impl-level generic params before resolving target/trait types
        let params_by_name = if let Some(ref generics) = imp.generic_params {
            self.define_generic_params(generics)
        } else {
            HashMap::new()
        };
        if let Some(ref wc) = imp.where_clause {
            self.resolve_where_clause(wc, &params_by_name);
        }

        // Resolve target type (may reference impl generic params)
        self.resolve_type_expr(&imp.target_type);

        // Resolve trait name
        if let Some(ref trait_path) = imp.trait_name {
            self.resolve_path_expr(trait_path);
            self.check_operator_trait_restriction(trait_path, &imp.target_type);
            self.check_into_trait_restriction(trait_path);
        }
        self.check_impl_level_effect_vars(imp);

        // Register Self as a type
        let _ = self.table.define(
            "Self".to_string(),
            SymbolKind::TypeParam,
            imp.span.clone(),
            false,
        );

        for item in &imp.items {
            match item {
                ImplItem::Method(method) => self.resolve_function(method),
                ImplItem::AssocType(binding) => {
                    self.resolve_type_expr(&binding.ty);
                }
            }
        }

        self.table.pop_scope();
        self.current_impl_type = None;
    }

    fn resolve_const_decl(&mut self, c: &ConstDecl) {
        self.resolve_type_expr(&c.ty);
        self.resolve_expr(&c.value);
    }

    fn resolve_type_alias_def(&mut self, t: &TypeAliasDef) {
        // Register generic params in a temp scope
        if let Some(ref generics) = t.generic_params {
            self.table.push_scope(ScopeKind::Block);
            self.define_generic_params(generics);
            self.resolve_type_expr(&t.ty);
            self.table.pop_scope();
        } else {
            self.resolve_type_expr(&t.ty);
        }
    }

    fn resolve_extern_function(&mut self, e: &ExternFunction) {
        self.table.push_scope(ScopeKind::Function);
        for param in &e.params {
            self.define_pattern_bindings(&param.pattern, false);
            self.resolve_type_expr(&param.ty);
        }
        if let Some(ref ret_ty) = e.return_type {
            self.resolve_type_expr(ret_ty);
        }
        if let Some(ref effects) = e.effects {
            self.resolve_effect_list(effects);
        }
        self.table.pop_scope();
    }

    fn resolve_effect_group_def(&mut self, g: &EffectGroupDecl) {
        for term in &g.body {
            match term {
                EffectGroupTerm::Verb(verb) => {
                    self.resolve_effect_verb(verb);
                }
                EffectGroupTerm::GroupRef(name) => {
                    if self.table.lookup(name).is_none() {
                        self.error_undefined_name(name, g.span.clone());
                    }
                }
            }
        }
    }

    // ── Block & Statement resolution ────────────────────────────

    fn resolve_block(&mut self, block: &Block) {
        self.table.push_scope(ScopeKind::Block);
        for stmt in &block.stmts {
            self.resolve_stmt(stmt);
        }
        if let Some(ref expr) = block.final_expr {
            self.resolve_expr(expr);
        }
        self.table.pop_scope();
    }

    /// Resolve a block without pushing a new scope (used when the caller
    /// already pushed a scope, e.g. for-loop body where the binding is
    /// in the same scope as the body).
    fn resolve_block_no_scope(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.resolve_stmt(stmt);
        }
        if let Some(ref expr) = block.final_expr {
            self.resolve_expr(expr);
        }
    }

    fn resolve_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                is_mut,
                pattern,
                ty,
                value,
            } => {
                // Resolve the value first (before introducing bindings)
                self.resolve_expr(value);
                if let Some(ref ty) = ty {
                    self.resolve_type_expr(ty);
                }
                // Now define the pattern bindings
                self.define_pattern_bindings(pattern, *is_mut);
            }
            StmtKind::LetUninit {
                is_mut,
                name,
                name_span,
                ty,
            } => {
                self.resolve_type_expr(ty);
                if let Err(e) = self.table.define(
                    name.clone(),
                    SymbolKind::Variable { is_mut: *is_mut },
                    name_span.clone(),
                    false,
                ) {
                    self.errors.push(e);
                }
            }
            StmtKind::LetElse {
                pattern,
                ty,
                value,
                else_block,
            } => {
                self.resolve_expr(value);
                if let Some(ref ty) = ty {
                    self.resolve_type_expr(ty);
                }
                self.resolve_block(else_block);
                self.define_pattern_bindings(pattern, false);
            }
            StmtKind::Defer { body } => {
                self.resolve_block(body);
            }
            StmtKind::ErrDefer { binding, body } => {
                self.table.push_scope(ScopeKind::Block);
                if let Some(name) = binding {
                    if let Err(e) = self.table.define(
                        name.clone(),
                        SymbolKind::Variable { is_mut: false },
                        stmt.span.clone(),
                        false,
                    ) {
                        self.errors.push(e);
                    }
                }
                self.resolve_block(body);
                self.table.pop_scope();
            }
            StmtKind::Assign { target, value } => {
                self.resolve_expr(target);
                self.resolve_expr(value);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                self.resolve_expr(target);
                self.resolve_expr(value);
            }
            StmtKind::Expr(expr) => {
                self.resolve_expr(expr);
            }
        }
    }

    // ── Expression resolution ───────────────────────────────────

    fn resolve_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Integer(_, _)
            | ExprKind::Float(_, _)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_)
            | ExprKind::Bool(_)
            | ExprKind::Return(None)
            | ExprKind::Error => {}

            ExprKind::Continue { label } => {
                if let Some(name) = label {
                    let entry = self
                        .loop_labels
                        .iter()
                        .find(|(l, _)| l.as_deref() == Some(name.as_str()));
                    match entry {
                        Some((_, LabelKind::Loop)) => {
                            // accepted — `continue` to a labeled loop
                        }
                        Some((_, LabelKind::Block)) => {
                            // LB2 — `continue` to a labeled block is rejected.
                            self.errors.push(ResolveError {
                                message: format!(
                                    "error[E_CONTINUE_LABEL_BLOCK]: continue label `{}` refers to a labeled block; continue is only valid for loops",
                                    name
                                ),
                                span: expr.span.clone(),
                                kind: ResolveErrorKind::ContinueOnBlockLabel,
                                suggestion: Some(format!(
                                    "rename the label or restructure `{}` as a loop if iteration is intended",
                                    name
                                )),
                                replacement: None,
                            });
                        }
                        None => {
                            self.errors.push(ResolveError {
                                message: format!("undefined loop label `{}`", name),
                                span: expr.span.clone(),
                                kind: ResolveErrorKind::UndefinedLabel,
                                suggestion: None,
                                replacement: None,
                            });
                        }
                    }
                }
            }

            ExprKind::Break { label, value: None } => {
                if let Some(name) = label {
                    if !self
                        .loop_labels
                        .iter()
                        .any(|(l, _)| l.as_deref() == Some(name.as_str()))
                    {
                        self.errors.push(ResolveError {
                            message: format!("undefined loop label `{}`", name),
                            span: expr.span.clone(),
                            kind: ResolveErrorKind::UndefinedLabel,
                            suggestion: None,
                            replacement: None,
                        });
                    }
                }
            }

            ExprKind::Identifier(name) => {
                if let Some(sym) = self.table.lookup(name) {
                    let id = sym.id;
                    self.record_resolution(&expr.span, id);
                } else {
                    self.error_undefined_name(name, expr.span.clone());
                }
            }

            ExprKind::Path { segments, .. } => {
                // Resolve the first segment, then qualified access
                if let Some(first) = segments.first() {
                    if let Some(sym) = self.table.lookup(first) {
                        let id = sym.id;
                        self.record_resolution(&expr.span, id);
                    } else {
                        self.error_undefined_name(first, expr.span.clone());
                    }
                }
            }

            ExprKind::SelfValue => {
                if let Some(sym) = self.table.lookup("self") {
                    let id = sym.id;
                    self.record_resolution(&expr.span, id);
                } else {
                    self.errors.push(ResolveError {
                        message: "'self' used outside of impl method".to_string(),
                        span: expr.span.clone(),
                        kind: ResolveErrorKind::UndefinedName,
                        suggestion: None,
                        replacement: None,
                    });
                }
            }

            ExprKind::SelfType => {
                if let Some(sym) = self.table.lookup("Self") {
                    let id = sym.id;
                    self.record_resolution(&expr.span, id);
                } else {
                    self.errors.push(ResolveError {
                        message: "'Self' used outside of impl block".to_string(),
                        span: expr.span.clone(),
                        kind: ResolveErrorKind::UndefinedName,
                        suggestion: None,
                        replacement: None,
                    });
                }
            }

            ExprKind::PipePlaceholder => {
                // Validated in type checker; nothing to resolve
            }

            ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
                self.resolve_expr(left);
                self.resolve_expr(right);
            }

            ExprKind::Unary { operand, .. } => {
                self.resolve_expr(operand);
            }

            ExprKind::Question(inner) | ExprKind::OptionalChain { object: inner, .. } => {
                self.resolve_expr(inner);
            }

            ExprKind::NilCoalesce { left, right } => {
                self.resolve_expr(left);
                self.resolve_expr(right);
            }

            ExprKind::Call { callee, args } => {
                // Bare-identifier callee that is undefined but matches a
                // trait-declared associated function name: defer resolution
                // to the typechecker, which dispatches via the expected type
                // (`let x: T = default()` → `T.default()`). The resolver does
                // not know the expected type so it cannot pick a target; it
                // suppresses the undefined-name error and lets the typechecker
                // produce a more targeted diagnostic if no expected type is
                // available.
                let mut deferred = false;
                if let ExprKind::Identifier(name) = &callee.kind {
                    if self.table.lookup(name).is_none() && self.is_trait_assoc_fn_name(name) {
                        deferred = true;
                    }
                }
                if !deferred {
                    self.resolve_expr(callee);
                }
                for arg in args {
                    self.resolve_expr(&arg.value);
                }
            }

            ExprKind::MethodCall {
                object,
                args,
                turbofish,
                ..
            } => {
                self.resolve_expr(object);
                for arg in args {
                    self.resolve_expr(&arg.value);
                }
                if let Some(ref tf) = turbofish {
                    for ty in tf {
                        self.resolve_type_expr(ty);
                    }
                }
            }

            ExprKind::FieldAccess { object, .. } => {
                self.resolve_expr(object);
            }

            ExprKind::TupleIndex { object, .. } => {
                self.resolve_expr(object);
            }

            ExprKind::Index { object, index } => {
                self.resolve_expr(object);
                self.resolve_expr(index);
            }

            ExprKind::Block(block) => {
                self.resolve_block(block);
            }

            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.resolve_expr(condition);
                self.resolve_block(then_block);
                if let Some(ref else_expr) = else_branch {
                    self.resolve_expr(else_expr);
                }
            }

            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.resolve_expr(value);
                self.table.push_scope(ScopeKind::Block);
                self.resolve_pattern(pattern);
                self.resolve_block(then_block);
                self.table.pop_scope();
                if let Some(ref else_expr) = else_branch {
                    self.resolve_expr(else_expr);
                }
            }

            ExprKind::Match { scrutinee, arms } => {
                self.resolve_expr(scrutinee);
                for arm in arms {
                    self.table.push_scope(ScopeKind::MatchArm);
                    self.resolve_pattern(&arm.pattern);
                    self.resolve_expr(&arm.body);
                    self.table.pop_scope();
                }
            }

            ExprKind::While {
                condition,
                body,
                label,
            } => {
                self.resolve_expr(condition);
                self.loop_labels.push((label.clone(), LabelKind::Loop));
                self.table.push_scope(ScopeKind::Loop);
                self.resolve_block_no_scope(body);
                self.table.pop_scope();
                self.loop_labels.pop();
            }

            ExprKind::WhileLet {
                pattern,
                value,
                body,
                label,
            } => {
                self.resolve_expr(value);
                self.loop_labels.push((label.clone(), LabelKind::Loop));
                self.table.push_scope(ScopeKind::Loop);
                self.resolve_pattern(pattern);
                self.resolve_block_no_scope(body);
                self.table.pop_scope();
                self.loop_labels.pop();
            }

            ExprKind::For {
                pattern,
                iterable,
                body,
                label,
            } => {
                self.resolve_expr(iterable);
                self.loop_labels.push((label.clone(), LabelKind::Loop));
                self.table.push_scope(ScopeKind::Loop);
                self.define_pattern_bindings(pattern, false);
                self.resolve_block_no_scope(body);
                self.table.pop_scope();
                self.loop_labels.pop();
            }

            ExprKind::Loop { body, label } => {
                self.loop_labels.push((label.clone(), LabelKind::Loop));
                self.table.push_scope(ScopeKind::Loop);
                self.resolve_block_no_scope(body);
                self.table.pop_scope();
                self.loop_labels.pop();
            }

            ExprKind::LabeledBlock { label, body, .. } => {
                // LB1 — labeled block: register label with `Block` kind so
                // the resolver can reject `continue label` referring here.
                self.loop_labels
                    .push((Some(label.clone()), LabelKind::Block));
                self.table.push_scope(ScopeKind::Block);
                self.resolve_block_no_scope(body);
                self.table.pop_scope();
                self.loop_labels.pop();
            }

            ExprKind::Closure {
                params,
                capture_mode: _,
                prefix_span: _,
                body,
            } => {
                // LB4 — closure-boundary rule. Save the current label stack
                // and replace it with an empty stack while resolving the
                // closure body, so a `break label` / `continue label` inside
                // the body cannot target an enclosing loop / block label.
                // Restored on exit. Also fixes the missing closure-boundary
                // rule for labeled loops as a side-effect (audit finding
                // 2026-05-08).
                let saved_labels = std::mem::take(&mut self.loop_labels);
                self.table.push_scope(ScopeKind::Closure);
                for param in params {
                    self.define_pattern_bindings(&param.pattern, false);
                    if let Some(ref ty) = param.ty {
                        self.resolve_type_expr(ty);
                    }
                }
                self.resolve_expr(body);
                self.table.pop_scope();
                self.loop_labels = saved_labels;
            }

            ExprKind::Return(inner) => {
                if let Some(ref expr) = inner {
                    self.resolve_expr(expr);
                }
            }

            ExprKind::Break {
                label,
                value: Some(ref expr),
            } => {
                if let Some(name) = label {
                    if !self
                        .loop_labels
                        .iter()
                        .any(|(l, _)| l.as_deref() == Some(name.as_str()))
                    {
                        self.errors.push(ResolveError {
                            message: format!("undefined loop label `{}`", name),
                            span: expr.span.clone(),
                            kind: ResolveErrorKind::UndefinedLabel,
                            suggestion: None,
                            replacement: None,
                        });
                    }
                }
                self.resolve_expr(expr);
            }

            ExprKind::Tuple(exprs) => {
                for e in exprs {
                    self.resolve_expr(e);
                }
            }

            ExprKind::StructLiteral {
                path,
                fields,
                spread,
            } => {
                // Resolve the struct type name
                if let Some(first) = path.first() {
                    if let Some(sym) = self.table.lookup(first) {
                        let id = sym.id;
                        self.record_resolution(&expr.span, id);
                    } else {
                        self.error_undefined_name(first, expr.span.clone());
                    }
                }
                for field in fields {
                    self.resolve_expr(&field.value);
                }
                if let Some(ref spread_expr) = spread {
                    self.resolve_expr(spread_expr);
                }
            }

            ExprKind::Cast { expr: inner, ty } => {
                self.resolve_expr(inner);
                self.resolve_type_expr(ty);
            }

            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.resolve_expr(s);
                }
                if let Some(e) = end {
                    self.resolve_expr(e);
                }
            }

            ExprKind::Unsafe(block) => {
                self.resolve_block(block);
            }

            ExprKind::Try(block) => {
                self.resolve_block(block);
            }

            ExprKind::ArrayLiteral(elements) => {
                for elem in elements {
                    self.resolve_expr(elem);
                }
            }

            ExprKind::RepeatLiteral { value, count, .. } => {
                self.resolve_expr(value);
                self.resolve_expr(count);
            }

            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for elem in items {
                    self.resolve_expr(elem);
                }
            }

            ExprKind::InterpolatedStringLit(parts) => {
                for part in parts {
                    if let crate::ast::ParsedInterpolationPart::Expr(e) = part {
                        self.resolve_expr(e);
                    }
                }
            }

            ExprKind::MapLiteral(entries) => {
                for (key, val) in entries {
                    self.resolve_expr(key);
                    self.resolve_expr(val);
                }
            }

            ExprKind::Seq(block) | ExprKind::Par(block) => {
                self.resolve_block(block);
            }

            ExprKind::Lock {
                mutex, alias, body, ..
            } => {
                // Resolve the mutex identifier
                if let Some(sym) = self.table.lookup(mutex) {
                    let id = sym.id;
                    self.record_resolution(&expr.span, id);
                } else {
                    self.error_undefined_name(mutex, expr.span.clone());
                }
                // Resolve body with optional alias binding
                self.table.push_scope(ScopeKind::Block);
                if let Some(ref alias_name) = alias {
                    let _ = self.table.define(
                        alias_name.clone(),
                        SymbolKind::Variable { is_mut: false },
                        expr.span.clone(),
                        false,
                    );
                }
                self.resolve_block_no_scope(body);
                self.table.pop_scope();
            }

            ExprKind::Providers { bindings, body } => {
                // Each binding key names an effect resource. Resolve against
                // the symbol table so undefined resources surface early (same
                // policy as `effect resource` references in effect verbs).
                // Values are plain expressions; body is a child scope.
                for b in bindings {
                    match self.table.lookup(&b.resource) {
                        Some(sym) if matches!(sym.kind, SymbolKind::EffectResource) => {
                            let id = sym.id;
                            self.record_resolution(&b.resource_span, id);
                        }
                        Some(_) => {
                            self.errors.push(ResolveError {
                                message: format!("'{}' is not an effect resource", b.resource),
                                span: b.resource_span.clone(),
                                kind: ResolveErrorKind::UndefinedName,
                                suggestion: None,
                                replacement: None,
                            });
                        }
                        None => {
                            self.errors.push(ResolveError {
                                message: format!("undefined effect resource '{}'", b.resource),
                                span: b.resource_span.clone(),
                                kind: ResolveErrorKind::UndefinedName,
                                suggestion: None,
                                replacement: None,
                            });
                        }
                    }
                    self.resolve_expr(&b.value);
                }
                self.resolve_block(body);
            }
        }
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
