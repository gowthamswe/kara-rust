//! Single source of truth for the prelude — names that are in scope in
//! every Kāra source file without an explicit `import`.
//!
//! ## CR-24 slice 8: auto-injection mechanism
//!
//! Per `docs/design.md § Module System › Prelude`, the long-term design
//! puts stdlib types and traits in real Kāra source under `runtime/stdlib/`
//! and auto-imports them via a synthetic `import std.prelude.*;` at the top
//! of every user module. CR-24 lands only the *mechanism*:
//!
//!   1. The prelude lives at the canonical module path [`PRELUDE_PATH_SEGMENTS`]
//!      (`std.prelude`) in the program tree.
//!   2. A synthetic [`Module`] with stub [`Item`]s for every prelude name is
//!      injected into the [`ProgramTree`] by [`build_program_tree`], so
//!      cross-module resolution recognises `import std.prelude.X;` without
//!      `E0224 UnknownModule`.
//!   3. The same names are still registered directly in the resolver's global
//!      scope and the typechecker's type environment — `register_builtin_types`
//!      remains the *placeholder* implementation that backs the synthetic
//!      module's stub items. Wildcard imports (`import a.b.*;`) are deferred
//!      from CR-24, so we can't actually splat the synthetic module's
//!      contents into every file via the import machinery yet — direct
//!      registration provides the equivalent name visibility today.
//!
//! Real stdlib materialisation (replacing `register_builtin_types` with
//! `runtime/stdlib/*.kara` source baked into the compiler) is a follow-up CR
//! tracked in `docs/implementation_checklist/`.
//!
//! [`Module`]: crate::module::Module
//! [`ProgramTree`]: crate::module::ProgramTree
//! [`build_program_tree`]: crate::module::build_program_tree
//! [`Item`]: crate::ast::Item

use crate::ast::{
    Block, Function, GenericParam, GenericParams, Item, StructDef, TraitDef, Visibility,
};
use crate::token::Span;

/// Canonical path of the synthetic prelude module: `std.prelude`. Stored as
/// `&'static str` segments here; callers that need an owned `Vec<String>`
/// (e.g. for [`crate::module::ModuleGraph::lookup`]) build one via
/// [`prelude_path`].
pub const PRELUDE_PATH_SEGMENTS: &[&str] = &["std", "prelude"];

/// Owned `Vec<String>` form of [`PRELUDE_PATH_SEGMENTS`].
pub fn prelude_path() -> Vec<String> {
    PRELUDE_PATH_SEGMENTS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Primitive type names that the lexer / parser already accept as keywords
/// or identifier-keywords. Registered in scope 0 so unqualified use resolves
/// without an import. Several pieces of the compiler still inline narrower
/// subsets for their own purposes (numeric widths, etc.); this list is the
/// canonical surface every module sees.
pub const PRELUDE_PRIMITIVES: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "f32", "f64", "bool", "char",
    "String",
];

/// Stdlib type names visible without import. These are the placeholder
/// targets that `register_builtin_types` (typechecker.rs) backs with real
/// type-environment entries today; the follow-up stdlib-materialisation CR
/// will replace the shim with parsed Kāra source.
pub const PRELUDE_TYPES: &[&str] = &[
    "Option",
    "Result",
    "Vec",
    "Array",
    "Slice",
    "Map",
    "Set",
    "Entry",
    "Never",
    "StringSlice",
    "F32",
    "F64",
    "Atomic",
    "Ordering",
    "IoError",
    "VarError",
    "SortedSet",
    "Channel",
    "Sender",
    "Receiver",
    "Stats",
    "Regex",
    "RegexError",
    "Match",
    "Client",
    "Response",
    "HttpError",
    "Base64",
    "Hex",
    "Url",
    "DecodeError",
];

/// Operator and conversion trait names visible without import. Lets
/// `impl Add for Foo` and `where T: Ord` resolve out of the box.
pub const PRELUDE_TRAITS: &[&str] = &[
    "From",
    "Into",
    "TryFrom",
    "TryInto",
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Rem",
    "Neg",
    "Eq",
    "Ord",
    "BitAnd",
    "BitOr",
    "BitXor",
    "Shl",
    "Shr",
    "Not",
    "Index",
    "IndexMut",
    "Display",
    "Iterator",
    "IntoIterator",
];

/// Enum variant names from prelude enums (`Option`, `Result`, `Ordering`)
/// surfaced unqualified per design.md § Prelude.
pub const PRELUDE_VARIANTS: &[&str] = &[
    "Some", "None", "Ok", "Err", // Ordering
    "Relaxed", "Acquire", "Release", "AcqRel", "SeqCst",
    // Entry[K, V] — Map.entry(k) returns one of these
    "Occupied", "Vacant",
];

/// Ambient program-rooted effect resources — resources whose provider is
/// installed at program start and lives for the program's lifetime. See
/// `docs/design.md § Provider-Rooted Resources` ("Scope of the rule") and
/// § Nondeterminism as an Explicit Resource. User code can reference these
/// without declaring `effect resource Clock;` manually; the interpreter
/// installs a default provider in the base frame so `Clock.now()` etc.
/// resolve deterministically outside any `with_provider` scope.
///
/// The list is intentionally conservative — each name listed here has at
/// least one built-in method implemented by the interpreter. Additional
/// primitives (`FileSystem`, `Network`, `Heap`, `Stdin`, `Env`) are
/// registered incrementally as their method surfaces land.
pub const PRELUDE_EFFECT_RESOURCES: &[&str] = &[
    "Clock",
    "RandomSource",
    "Env",
    "Stdin",
    "Stdout",
    "Stderr",
    "FileSystem",
];

/// Compiler builtins / I/O functions visible without import. Implementations
/// stay compiler-side (`!` return type, source-location capture, release
/// elision) per `docs/design.md § Module System › Prelude` — only the names
/// live here.
pub const PRELUDE_FUNCTIONS: &[&str] = &[
    "todo",
    "unreachable",
    "dbg",
    "print",
    "println",
    "eprintln",
    "assert",
    "assert_eq",
    "assert_ne",
    // Scoped provider injection — see docs/design.md § Provider-Rooted
    // Resources. The parser accepts it as an ordinary identifier; the
    // interpreter intercepts the `with_provider[R](p, || body)` call shape
    // to push/pop a provider frame (see Interpreter::match_with_provider).
    "with_provider",
];

/// Synthetic span used for every stub item the prelude module emits. The
/// resolver / typechecker recognise span (line 0, column 0) as compiler-
/// generated and allow user definitions to shadow it without raising
/// `E0101 DuplicateDefinition`.
fn synthetic_span() -> Span {
    Span {
        line: 0,
        column: 0,
        offset: 0,
        length: 0,
    }
}

/// Build a stub [`Item`] sequence representing the prelude module's
/// publicly-visible surface. The bodies are intentionally empty — the real
/// shape lives in `register_builtin_types` (typechecker) and the resolver's
/// `register_primitives`. These stubs exist purely so cross-module resolution
/// (`module::canonical_origin`, `module::module_exposes_item`,
/// `resolver::module_top_level_names_for_id`) can see `std.prelude` exposing
/// the right names when user code writes `import std.prelude.X;`.
pub fn synthetic_prelude_items() -> Vec<Item> {
    let span = synthetic_span();
    let mut items: Vec<Item> = Vec::new();

    for &name in PRELUDE_TYPES {
        items.push(stub_struct(name, &span));
    }
    for &name in PRELUDE_TRAITS {
        items.push(stub_trait(name, &span));
    }
    for &name in PRELUDE_FUNCTIONS {
        items.push(stub_function(name, &span));
    }
    items
    // Note: prelude *variant* names (`Some`, `None`, `Ok`, `Err`, …) are not
    // exposed as top-level items here. They reach user code via the
    // resolver's scope-0 registration (`register_prelude_symbols`) instead —
    // mirroring Rust, where `use std::option::Some;` is not the path users
    // import variants through. Users that need to qualify a variant write
    // `Option.Some(x)` or import the enum and use `Some(x)` unqualified.
}

fn stub_struct(name: &str, span: &Span) -> Item {
    Item::StructDef(StructDef {
        span: span.clone(),
        attributes: Vec::new(),
        doc_comment: None,
        is_pub: true,
        is_private: false,
        is_shared: false,
        no_rc: false,
        name: name.to_string(),
        generic_params: stub_generics(name, span),
        where_clause: None,
        fields: Vec::new(),
        invariants: Vec::new(),
    })
}

/// Generic parameter list for the few prelude types whose generic arity is
/// commonly inspected. The stubs do not have to match the *real* generic
/// arity exactly — they exist only so resolver / typechecker queries that
/// read generic parameter count from the typechecker's `register_builtin_types`
/// env entries stay authoritative; the synthetic module shim never
/// participates in type inference.
fn stub_generics(name: &str, span: &Span) -> Option<GenericParams> {
    let params: &[&str] = match name {
        "Option" | "Vec" | "Slice" | "Array" | "Set" | "Atomic" | "SortedSet" | "Channel"
        | "Sender" | "Receiver" => &["T"],
        "Result" => &["T", "E"],
        "Map" | "Entry" => &["K", "V"],
        _ => return None,
    };
    Some(GenericParams {
        span: span.clone(),
        effect_params: Vec::new(),
        params: params
            .iter()
            .map(|p| GenericParam {
                span: span.clone(),
                name: (*p).to_string(),
                bounds: Vec::new(),
                is_const: false,
                const_type: None,
            })
            .collect(),
    })
}

fn stub_trait(name: &str, span: &Span) -> Item {
    Item::TraitDef(TraitDef {
        span: span.clone(),
        attributes: Vec::new(),
        doc_comment: None,
        is_pub: true,
        is_private: false,
        name: name.to_string(),
        generic_params: None,
        supertraits: Vec::new(),
        trait_effects: None,
        where_clause: None,
        items: Vec::new(),
    })
}

fn stub_function(name: &str, span: &Span) -> Item {
    Item::Function(Function {
        span: span.clone(),
        attributes: Vec::new(),
        doc_comment: None,
        is_pub: true,
        is_private: false,
        name: name.to_string(),
        generic_params: None,
        params: Vec::new(),
        self_param: None,
        return_type: None,
        effects: None,
        requires: Vec::new(),
        ensures: Vec::new(),
        where_clause: None,
        body: Block {
            stmts: Vec::new(),
            final_expr: None,
            span: span.clone(),
        },
    })
}

/// True iff `path` names the synthetic prelude module.
pub fn is_prelude_path(path: &[String]) -> bool {
    path.len() == PRELUDE_PATH_SEGMENTS.len()
        && path
            .iter()
            .zip(PRELUDE_PATH_SEGMENTS.iter())
            .all(|(a, b)| a == b)
}

/// Visibility every synthetic prelude item carries. Kept as a helper so
/// downstream call sites do not have to repeat the literal.
#[allow(dead_code)]
pub fn prelude_visibility() -> Visibility {
    Visibility::Pub
}
