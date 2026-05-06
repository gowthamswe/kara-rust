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
    Block, Function, GenericParam, GenericParams, Item, Program, StructDef, TraitDef, Visibility,
};
use crate::token::Span;
use std::sync::LazyLock;

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
    // CR-202 slice 5a: `PartialEq` is now a real registered trait
    // (via baked `runtime/stdlib/partial_eq.kara`) rather than a
    // side-set name consulted only through `derived_traits`. Listing
    // it here makes it visible at scope-0 so user code can write
    // `impl PartialEq for ...` and reference the bound in
    // `where T: PartialEq`.
    "PartialEq",
    // CR-202 slice 5c: `PartialOrd` joins as the partial-ordering
    // counterpart to PartialEq.
    "PartialOrd",
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

// ── Baked stdlib source (CR-202 slice 3a) ───────────────────────────
//
// Real Kāra source for prelude types is authored under `runtime/stdlib/*.kara`
// and embedded into the compiler binary via `include_str!`. The pilot scope
// is `Option` only (slice 3); slice 4+ adds one file at a time, retiring the
// corresponding arm of `register_builtin_types` at each step.
//
// 3a is plumbing-only: this constant and [`STDLIB_PROGRAMS`] expose the
// parsed AST for downstream consumption, but no current pipeline code
// reads them. Slice 3c will splice the parsed `EnumDef` for Option into
// the synthetic prelude module's items list, replacing the stub
// `StructDef` that lives in this file today.

/// Embedded stdlib sources, keyed by their on-disk basename (relative to
/// `runtime/stdlib/`). Sources are baked at compile time via `include_str!`
/// so the resulting binary is self-contained.
pub const STDLIB_SOURCES: &[(&str, &str)] = &[
    ("option.kara", include_str!("../runtime/stdlib/option.kara")),
    ("result.kara", include_str!("../runtime/stdlib/result.kara")),
    ("vec.kara", include_str!("../runtime/stdlib/vec.kara")),
    ("partial_eq.kara", include_str!("../runtime/stdlib/partial_eq.kara")),
    ("eq.kara", include_str!("../runtime/stdlib/eq.kara")),
    ("partial_ord.kara", include_str!("../runtime/stdlib/partial_ord.kara")),
    ("ord.kara", include_str!("../runtime/stdlib/ord.kara")),
];

/// Parsed AST of every entry in [`STDLIB_SOURCES`]. Parsed lazily on first
/// access and cached for the lifetime of the process. The vector preserves
/// the source order from `STDLIB_SOURCES`, so callers that need
/// deterministic load order (e.g. resolve trait/struct dependencies) get
/// it for free.
///
/// Panics if any baked source fails to parse — a parse failure indicates
/// a bug in the stdlib source itself, not in user code, and there is no
/// recoverable path. The error message names the offending file so the
/// fix is obvious.
pub static STDLIB_PROGRAMS: LazyLock<Vec<(&'static str, Program)>> = LazyLock::new(|| {
    let mut out = Vec::with_capacity(STDLIB_SOURCES.len());
    for &(name, src) in STDLIB_SOURCES {
        let parsed = crate::parse(src);
        if !parsed.errors.is_empty() {
            let msgs = parsed
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n  ");
            panic!(
                "baked stdlib source `{}` failed to parse:\n  {}",
                name, msgs
            );
        }
        out.push((name, parsed.program));
    }
    out
});

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
        // Slice 3c: prelude type names that have a baked source declaration
        // splice in the real `Item` from `STDLIB_PROGRAMS` (with
        // `stdlib_origin = true` so the slice-3b resolver gate bypass
        // applies even though user-mode resolver sessions walk this
        // module). All other names continue to use the placeholder stub.
        // Slice 4 grows the baked surface one type at a time.
        if let Some(item) = baked_item_for(name) {
            items.push(item);
        } else {
            items.push(stub_struct(name, &span));
        }
    }
    for &name in PRELUDE_TRAITS {
        // Slice 5a extends the same bake-or-stub split to traits.
        if let Some(item) = baked_item_for(name) {
            items.push(item);
        } else {
            items.push(stub_trait(name, &span));
        }
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

/// Look up a top-level item by name across every baked stdlib program.
/// Returns a clone with `stdlib_origin = true` flipped on the matching
/// item kind so the resolver's slice-3b gate bypass applies. Slice 3c
/// uses this from `synthetic_prelude_items`; slice 3d wires the same
/// helper into `register_builtin_types` so the typechecker registers
/// from baked source instead of the hardcoded shape.
fn baked_item_for(name: &str) -> Option<Item> {
    for (_, program) in STDLIB_PROGRAMS.iter() {
        for item in &program.items {
            let matches = match item {
                Item::Function(f) => f.name == name,
                Item::StructDef(s) => s.name == name,
                Item::EnumDef(e) => e.name == name,
                Item::TraitDef(t) => t.name == name,
                _ => false,
            };
            if !matches {
                continue;
            }
            let mut cloned = item.clone();
            match &mut cloned {
                Item::Function(f) => f.stdlib_origin = true,
                Item::StructDef(s) => s.stdlib_origin = true,
                Item::EnumDef(e) => e.stdlib_origin = true,
                Item::TraitDef(t) => t.stdlib_origin = true,
                _ => {}
            }
            return Some(cloned);
        }
    }
    None
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
        stdlib_origin: true,
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
        stdlib_origin: true,
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
        stdlib_origin: true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EnumDef, Item, VariantKind};

    /// Find the `EnumDef` for `name` among the items of the parsed stdlib
    /// program at the given index in `STDLIB_PROGRAMS`. Test helper.
    fn find_enum(idx: usize, name: &str) -> &'static EnumDef {
        let (_, program) = &STDLIB_PROGRAMS[idx];
        for item in &program.items {
            if let Item::EnumDef(e) = item {
                if e.name == name {
                    return e;
                }
            }
        }
        panic!("expected enum `{}` in stdlib program at index {}", name, idx);
    }

    #[test]
    fn stdlib_sources_contains_option_kara() {
        let names: Vec<&str> = STDLIB_SOURCES.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"option.kara"),
            "STDLIB_SOURCES should contain option.kara, got: {:?}",
            names
        );
    }

    #[test]
    fn stdlib_sources_contains_result_kara() {
        // CR-202 slice 4a: `Result` joins the baked surface.
        let names: Vec<&str> = STDLIB_SOURCES.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"result.kara"),
            "STDLIB_SOURCES should contain result.kara, got: {:?}",
            names
        );
    }

    #[test]
    fn stdlib_sources_contains_vec_kara() {
        // CR-202 slice 4b: `Vec` joins the baked surface.
        let names: Vec<&str> = STDLIB_SOURCES.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"vec.kara"),
            "STDLIB_SOURCES should contain vec.kara, got: {:?}",
            names
        );
    }

    #[test]
    fn stdlib_sources_contains_partial_eq_kara() {
        // CR-202 slice 5a: first baked trait file.
        let names: Vec<&str> = STDLIB_SOURCES.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"partial_eq.kara"),
            "STDLIB_SOURCES should contain partial_eq.kara, got: {:?}",
            names
        );
    }

    #[test]
    fn stdlib_sources_contains_eq_kara() {
        // CR-202 slice 5b: `Eq` joins the baked surface with `: PartialEq`.
        let names: Vec<&str> = STDLIB_SOURCES.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"eq.kara"),
            "STDLIB_SOURCES should contain eq.kara, got: {:?}",
            names
        );
    }

    #[test]
    fn stdlib_sources_contains_partial_ord_kara() {
        // CR-202 slice 5c: `PartialOrd: PartialEq` joins the baked surface.
        let names: Vec<&str> = STDLIB_SOURCES.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"partial_ord.kara"),
            "STDLIB_SOURCES should contain partial_ord.kara, got: {:?}",
            names
        );
    }

    #[test]
    fn stdlib_sources_contains_ord_kara() {
        // CR-202 slice 5d: `Ord: PartialOrd + Eq` joins the baked surface.
        let names: Vec<&str> = STDLIB_SOURCES.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"ord.kara"),
            "STDLIB_SOURCES should contain ord.kara, got: {:?}",
            names
        );
    }

    #[test]
    fn stdlib_sources_have_nonempty_bodies() {
        for &(name, src) in STDLIB_SOURCES {
            assert!(
                !src.trim().is_empty(),
                "stdlib source `{}` should not be empty",
                name
            );
        }
    }

    #[test]
    fn stdlib_programs_parses_option_cleanly() {
        // Forces evaluation of the LazyLock; would panic with a parse-error
        // message if the bake source is malformed.
        let programs: &Vec<(&'static str, Program)> = &STDLIB_PROGRAMS;
        assert_eq!(
            programs.len(),
            STDLIB_SOURCES.len(),
            "STDLIB_PROGRAMS should have one entry per STDLIB_SOURCES entry"
        );
    }

    #[test]
    fn baked_option_has_some_and_none_variants() {
        let opt = find_enum(0, "Option");
        let variant_names: Vec<&str> = opt.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            variant_names,
            vec!["Some", "None"],
            "baked Option should declare exactly Some(T), None"
        );
    }

    #[test]
    fn baked_option_has_one_generic_param_named_t() {
        let opt = find_enum(0, "Option");
        let params = opt
            .generic_params
            .as_ref()
            .expect("baked Option should declare a generic parameter list");
        let names: Vec<&str> = params.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["T"]);
    }

    #[test]
    fn baked_option_some_variant_carries_type_param() {
        let opt = find_enum(0, "Option");
        let some = opt
            .variants
            .iter()
            .find(|v| v.name == "Some")
            .expect("Some variant should exist");
        match &some.kind {
            VariantKind::Tuple(types) => {
                assert_eq!(types.len(), 1, "Some(T) should carry exactly one type");
            }
            other => panic!("expected Some to be Tuple-shaped, got {:?}", other),
        }
    }

    #[test]
    fn baked_option_none_variant_is_unit() {
        let opt = find_enum(0, "Option");
        let none = opt
            .variants
            .iter()
            .find(|v| v.name == "None")
            .expect("None variant should exist");
        assert!(
            matches!(none.kind, VariantKind::Unit),
            "None should be a Unit variant, got {:?}",
            none.kind
        );
    }

    // ── Slice 3d verification: synthetic_prelude_items splices baked Option ──

    fn find_prelude_item<'a>(items: &'a [Item], name: &str) -> Option<&'a Item> {
        items.iter().find(|i| match i {
            Item::Function(f) => f.name == name,
            Item::StructDef(s) => s.name == name,
            Item::EnumDef(e) => e.name == name,
            Item::TraitDef(t) => t.name == name,
            _ => false,
        })
    }

    #[test]
    fn synthetic_prelude_items_returns_baked_option_as_enum_def() {
        // Pre-3c the prelude module exposed Option as `Item::StructDef`
        // (a placeholder stub from `stub_struct`). After 3c the splice
        // should be the real `Item::EnumDef` parsed from
        // `runtime/stdlib/option.kara`.
        let items = synthetic_prelude_items();
        let opt = find_prelude_item(&items, "Option")
            .expect("synthetic prelude exposes Option");
        assert!(
            matches!(opt, Item::EnumDef(_)),
            "Option should be spliced as EnumDef (baked), got {:?}",
            opt
        );
    }

    #[test]
    fn synthetic_prelude_items_returns_baked_result_as_enum_def() {
        // CR-202 slice 4a: same splice path, second file. Confirms the
        // multi-file `STDLIB_SOURCES` path resolves Result through
        // `baked_item_for` rather than falling back to the stub.
        let items = synthetic_prelude_items();
        let res = find_prelude_item(&items, "Result")
            .expect("synthetic prelude exposes Result");
        assert!(
            matches!(res, Item::EnumDef(_)),
            "Result should be spliced as EnumDef (baked), got {:?}",
            res
        );
        let Item::EnumDef(e) = res else { unreachable!() };
        assert!(
            e.span.line > 0,
            "baked Result should carry a real source span"
        );
        assert!(
            e.stdlib_origin,
            "baked Result should be tagged stdlib_origin = true"
        );
    }

    #[test]
    fn synthetic_prelude_items_returns_baked_vec_as_struct_def() {
        // CR-202 slice 4b: Vec joins the baked surface as a struct.
        // Pre-4b Vec was a `stub_struct` with synthetic span; post-4b it
        // is the real `struct Vec[T] { }` from baked source.
        let items = synthetic_prelude_items();
        let v = find_prelude_item(&items, "Vec").expect("synthetic prelude exposes Vec");
        let Item::StructDef(s) = v else {
            panic!("Vec should be spliced as StructDef (baked), got {:?}", v);
        };
        assert!(s.span.line > 0, "baked Vec should carry a real source span");
        assert!(
            s.stdlib_origin,
            "baked Vec should be tagged stdlib_origin = true"
        );
        let params = s
            .generic_params
            .as_ref()
            .expect("baked Vec should declare a generic param list");
        assert_eq!(
            params.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["T"],
        );
    }

    #[test]
    fn synthetic_prelude_items_returns_baked_partial_eq_as_trait_def() {
        // CR-202 slice 5a: first baked trait. Pre-5a `PartialEq` did not
        // appear in `PRELUDE_TRAITS` and was therefore not exposed
        // through the synthetic prelude module at all. After 5a it is a
        // real `Item::TraitDef` from `runtime/stdlib/partial_eq.kara`.
        let items = synthetic_prelude_items();
        let pe = find_prelude_item(&items, "PartialEq")
            .expect("synthetic prelude exposes PartialEq");
        let Item::TraitDef(t) = pe else {
            panic!("PartialEq should be spliced as TraitDef (baked), got {:?}", pe);
        };
        assert!(
            t.span.line > 0,
            "baked PartialEq should carry a real source span"
        );
        assert!(
            t.stdlib_origin,
            "baked PartialEq should be tagged stdlib_origin = true"
        );
        // Should declare exactly one method (`eq`), no associated types.
        let method_names: Vec<&str> = t
            .items
            .iter()
            .filter_map(|i| match i {
                crate::ast::TraitItem::Method(m) => Some(m.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(method_names, vec!["eq"]);
    }

    #[test]
    fn synthetic_prelude_items_returns_baked_eq_with_partial_eq_supertrait() {
        // CR-202 slice 5b: `Eq` is now `Eq: PartialEq` from baked source.
        // Pre-5b the hardcoded `register_stdlib_traits` array registered
        // `Eq` with no supertraits.
        let items = synthetic_prelude_items();
        let eq = find_prelude_item(&items, "Eq").expect("synthetic prelude exposes Eq");
        let Item::TraitDef(t) = eq else {
            panic!("Eq should be spliced as TraitDef (baked), got {:?}", eq);
        };
        assert!(t.span.line > 0, "baked Eq should carry a real source span");
        assert!(
            t.stdlib_origin,
            "baked Eq should be tagged stdlib_origin = true"
        );
        let supertrait_names: Vec<&str> = t
            .supertraits
            .iter()
            .map(|b| b.path.last().map(|s| s.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(
            supertrait_names,
            vec!["PartialEq"],
            "baked Eq should declare `PartialEq` as its sole supertrait"
        );
        let method_count = t
            .items
            .iter()
            .filter(|i| matches!(i, crate::ast::TraitItem::Method(_)))
            .count();
        assert_eq!(method_count, 0, "Eq should declare no methods of its own");
    }

    #[test]
    fn baked_option_has_real_source_span_not_synthetic() {
        // The placeholder stubs use `synthetic_span()` (line 0, column 0,
        // offset 0). The baked source's span is set by the parser based
        // on the actual byte offset of the `enum Option` declaration in
        // `runtime/stdlib/option.kara`, so it has a non-zero line. This
        // is what 3d's diagnostic-span improvement rests on:
        // Option-related diagnostics now point at the real source rather
        // than the synthetic origin.
        let items = synthetic_prelude_items();
        let opt = find_prelude_item(&items, "Option").unwrap();
        let Item::EnumDef(e) = opt else {
            panic!("expected EnumDef");
        };
        assert!(
            e.span.line > 0,
            "baked Option's span should point at the real source (non-zero line), got line={}",
            e.span.line
        );
    }

    #[test]
    fn baked_option_carries_stdlib_origin_tag() {
        // The slice-3b gate bypass relies on `stdlib_origin = true` on
        // baked items. `baked_item_for` flips it after cloning; verify
        // the splice preserves the flag end to end.
        let items = synthetic_prelude_items();
        let opt = find_prelude_item(&items, "Option").unwrap();
        let Item::EnumDef(e) = opt else {
            panic!("expected EnumDef");
        };
        assert!(
            e.stdlib_origin,
            "baked Option should be tagged stdlib_origin = true"
        );
    }

    #[test]
    fn placeholder_stub_for_unbaked_prelude_type_keeps_synthetic_span() {
        // CR-202 contract: only types with a baked source file get the
        // real-source treatment; everything else continues to use
        // `stub_struct` with a synthetic span. This pins the
        // partial-migration property until the remaining types migrate.
        // `Map` is explicitly deferred (its trait bounds depend on slice
        // 5's trait materialization), so it's a stable picker for this
        // test even as slice 4 widens the baked surface.
        let items = synthetic_prelude_items();
        let map_item = find_prelude_item(&items, "Map")
            .expect("Map is still a prelude name");
        match map_item {
            Item::StructDef(s) => {
                assert_eq!(
                    s.span.line, 0,
                    "non-baked prelude type should still use synthetic span"
                );
                assert!(
                    s.stdlib_origin,
                    "stubs are still tagged stdlib_origin = true \
                     (the synthetic prelude module IS stdlib origin)"
                );
            }
            other => panic!("Map should still be a stub StructDef, got {:?}", other),
        }
    }
}
