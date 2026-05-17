// tests/missing_must_use_lint.rs
//
// Slice 3 of the `#[must_use]` mandate
// (`docs/implementation_checklist/phase-5-diagnostics.md` § `#[must_use]`
// mandate, slice 3): `missing_must_use` stdlib-hygiene lint.
//
// Test surface:
//
// 1. Positive cases — programs whose `stdlib_origin`-tagged functions
//    match the iterator-shape or new-value-from-self heuristics fire
//    the lint with the expected diagnostic shape (primary + note +
//    help).
//
// 2. Negative-space cases — every documented exclusion path stays
//    silent (user-origin items, already-must_use items,
//    `#[allow(missing_must_use)]` items, Result / Option / Unit / Self
//    / impl-target returns, owned-self / mut-ref-self receivers,
//    trait-impl methods, trait method declarations).
//
// 3. Stdlib-hygiene baseline — a sentinel test that walks the actual
//    `STDLIB_PROGRAMS` constant and confirms the lint runs end-to-end
//    against real baked source. Pinned as "produces ≥ 1 diagnostic
//    today" rather than a fixed integer because the baseline is
//    expected to shrink as triage adds `#[must_use]` annotations to
//    real stdlib functions; the test guards "lint is wired and finds
//    real candidates" without making every triage commit re-flip the
//    pin.

use karac::ast::{
    AttrArg, Attribute, Block, ExprKind, Function, ImplBlock, ImplItem, Item, PathExpr, Program,
    SelfParam, TypeExpr, TypeKind, Visibility,
};
use karac::missing_must_use_lint::{check_missing_must_use, LintDiagnostic, LintLevel};
use karac::prelude::STDLIB_PROGRAMS;
use karac::token::Span;

// ── AST construction helpers ─────────────────────────────────────────
//
// The lint is purely syntactic — it consults `Function.attributes`,
// `Function.self_param`, `Function.return_type`, `Function.is_pub`,
// `Function.stdlib_origin`, and `ImplBlock.{trait_name, target_type}`.
// Constructing minimal synthetic AST nodes is cheaper and more focused
// than round-tripping through the parser, since the lint exercises
// no body-level expression machinery.

fn syn_span() -> Span {
    Span {
        offset: 0,
        length: 0,
        line: 1,
        column: 1,
    }
}

fn path_type(name: &str) -> TypeExpr {
    TypeExpr {
        kind: TypeKind::Path(PathExpr {
            segments: vec![name.to_string()],
            generic_args: None,
            span: syn_span(),
        }),
        span: syn_span(),
    }
}

fn unit_type() -> TypeExpr {
    TypeExpr {
        kind: TypeKind::Unit,
        span: syn_span(),
    }
}

fn empty_block() -> Block {
    Block {
        stmts: Vec::new(),
        final_expr: None,
        span: syn_span(),
    }
}

fn must_use_attr() -> Attribute {
    Attribute {
        span: syn_span(),
        name: "must_use".to_string(),
        args: Vec::new(),
        string_value: Some("test reason".to_string()),
    }
}

fn allow_missing_must_use_attr() -> Attribute {
    Attribute {
        span: syn_span(),
        name: "allow".to_string(),
        args: vec![AttrArg {
            name: None,
            value: Some(karac::ast::Expr {
                kind: ExprKind::Identifier("missing_must_use".to_string()),
                span: syn_span(),
            }),
            span: syn_span(),
        }],
        string_value: None,
    }
}

#[derive(Clone)]
struct FnSpec<'a> {
    name: &'a str,
    is_pub: bool,
    stdlib_origin: bool,
    self_param: Option<SelfParam>,
    return_type: Option<TypeExpr>,
    attrs: &'a [&'a Attribute],
}

impl<'a> FnSpec<'a> {
    fn build(self) -> Function {
        Function {
            span: syn_span(),
            attributes: self.attrs.iter().map(|a| (*a).clone()).collect(),
            doc_comment: None,
            is_pub: self.is_pub,
            is_private: false,
            is_unsafe: false,
            name: self.name.to_string(),
            generic_params: None,
            params: Vec::new(),
            self_param: self.self_param,
            return_type: self.return_type,
            effects: None,
            requires: Vec::new(),
            ensures: Vec::new(),
            where_clause: None,
            body: empty_block(),
            stdlib_origin: self.stdlib_origin,
            deprecation: None,
            is_track_caller: false,
            lint_overrides: Vec::new(),
        }
    }
}

fn program_with(items: Vec<Item>) -> Program {
    Program {
        items,
        ..Default::default()
    }
}

fn free_fn(spec: FnSpec<'_>) -> Item {
    Item::Function(spec.build())
}

fn impl_block_inherent(target: &str, methods: Vec<Function>) -> Item {
    Item::ImplBlock(ImplBlock {
        span: syn_span(),
        attributes: Vec::new(),
        generic_params: None,
        trait_name: None,
        target_type: path_type(target),
        where_clause: None,
        items: methods
            .into_iter()
            .map(|m| ImplItem::Method(Box::new(m)))
            .collect(),
    })
}

fn impl_block_trait(trait_name: &str, target: &str, methods: Vec<Function>) -> Item {
    Item::ImplBlock(ImplBlock {
        span: syn_span(),
        attributes: Vec::new(),
        generic_params: None,
        trait_name: Some(PathExpr {
            segments: vec![trait_name.to_string()],
            generic_args: None,
            span: syn_span(),
        }),
        target_type: path_type(target),
        where_clause: None,
        items: methods
            .into_iter()
            .map(|m| ImplItem::Method(Box::new(m)))
            .collect(),
    })
}

fn lint_one(program: Program) -> Vec<LintDiagnostic> {
    check_missing_must_use(&program)
}

fn assert_fires_with(diags: &[LintDiagnostic], expected_substring: &str) {
    assert!(
        diags.iter().any(|d| d.lint_name == "missing_must_use"
            && d.level == LintLevel::Warning
            && d.message.contains(expected_substring)),
        "expected `missing_must_use` warning containing '{expected_substring}', got: {diags:?}"
    );
}

// ── Positive cases ───────────────────────────────────────────────────

#[test]
fn test_iterator_return_no_self_fires() {
    // Stdlib free `pub fn` returning `Iterator[…]`: fires with the
    // iterator-flavored message.
    let f = FnSpec {
        name: "produce",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Iterator")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_fires_with(&diags, "iterator-shaped");
    assert_eq!(
        diags[0].message,
        "stdlib `fn produce` returns an iterator-shaped value but is not annotated `#[must_use]`"
    );
    let help = diags[0]
        .help
        .as_ref()
        .expect("iterator diagnostic should carry help");
    assert!(
        help.contains("terminal method") && help.contains("bind the result"),
        "iterator help should mention the slice 2 spec-mandated message, got: {help}"
    );
    let note = diags[0]
        .note
        .as_ref()
        .expect("iterator diagnostic should carry note");
    assert!(
        note.contains("stdlib hygiene") && note.contains("slice 4"),
        "iterator note should cross-reference stdlib hygiene + slice 4, got: {note}"
    );
}

#[test]
fn test_peekable_return_fires_under_iterator_heuristic() {
    // Pinning that `Peekable[T]` is recognised as an iterator-shaped
    // return (slice 2 annotated it as must-use on the *type*; this
    // lint catches stdlib fns returning it without `#[must_use]`).
    let f = FnSpec {
        name: "wrap",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Peekable")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert_eq!(diags.len(), 1);
    assert_fires_with(&diags, "iterator-shaped");
}

#[test]
fn test_new_value_no_self_fires() {
    // Stdlib `pub fn` returning a fresh value with no receiver:
    // matches new-value-from-self heuristic (return ≠ Unit / Self /
    // Result / Option, no `self`).
    let f = FnSpec {
        name: "make_widget",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Widget")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert_eq!(diags.len(), 1);
    assert_fires_with(&diags, "new value");
    let note = diags[0].note.as_ref().unwrap();
    assert!(
        note.contains("new-value-from-self") && note.contains("ref self"),
        "new-value note should name the heuristic + receiver shape, got: {note}"
    );
}

#[test]
fn test_inherent_impl_method_ref_self_returning_new_value_fires() {
    // Inherent method on an impl block: `ref self`, return ≠ impl
    // target → matches new-value-from-self. Mirrors the
    // `Stats.mean(xs)` / `Cli.help_text(ref self) -> String` shape.
    let method = FnSpec {
        name: "to_summary",
        is_pub: false, // Impl methods don't carry `pub` in baked stdlib convention.
        stdlib_origin: true,
        self_param: Some(SelfParam::Ref),
        return_type: Some(path_type("String")),
        attrs: &[],
    }
    .build();
    let diags = lint_one(program_with(vec![impl_block_inherent(
        "Stats",
        vec![method],
    )]));
    assert_eq!(diags.len(), 1);
    assert_fires_with(&diags, "new value");
}

#[test]
fn test_inherent_static_method_returning_other_type_fires() {
    // `Type.factory() -> OtherType` — no self, returns something
    // other than the impl target. Matches new-value-from-self.
    let method = FnSpec {
        name: "describe",
        is_pub: false,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("String")),
        attrs: &[],
    }
    .build();
    let diags = lint_one(program_with(vec![impl_block_inherent(
        "Widget",
        vec![method],
    )]));
    assert_eq!(diags.len(), 1);
    assert_fires_with(&diags, "new value");
}

// ── Negative-space: receiver-shape exclusions ────────────────────────

#[test]
fn test_owned_self_receiver_does_not_fire() {
    // `Command.arg(self, …) -> Command` — owned-self builder. Spec
    // excludes consuming-self receivers from the new-value-from-self
    // heuristic.
    let method = FnSpec {
        name: "arg",
        is_pub: false,
        stdlib_origin: true,
        self_param: Some(SelfParam::Owned),
        return_type: Some(path_type("Command")),
        attrs: &[],
    }
    .build();
    let diags = lint_one(program_with(vec![impl_block_inherent(
        "Command",
        vec![method],
    )]));
    assert!(
        diags.is_empty(),
        "owned-self receiver should not fire, got: {diags:?}"
    );
}

#[test]
fn test_mut_ref_self_receiver_does_not_fire() {
    // `Vec.push(mut ref self, val)` — mutates the receiver in place.
    // The return is often Unit (handled by another path) but even
    // when it's not, mut-ref-self is a state-changing method, not a
    // pure transformation. Spec restricts the heuristic to `ref self`
    // or no `self`.
    let method = FnSpec {
        name: "rotate",
        is_pub: false,
        stdlib_origin: true,
        self_param: Some(SelfParam::MutRef),
        return_type: Some(path_type("i64")),
        attrs: &[],
    }
    .build();
    let diags = lint_one(program_with(vec![impl_block_inherent("Vec", vec![method])]));
    assert!(
        diags.is_empty(),
        "mut-ref-self receiver should not fire, got: {diags:?}"
    );
}

// ── Negative-space: return-shape exclusions ──────────────────────────

#[test]
fn test_unit_return_does_not_fire() {
    let f = FnSpec {
        name: "side_effect",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(unit_type()),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(diags.is_empty());
}

#[test]
fn test_no_return_type_does_not_fire() {
    // Bare `fn foo()` (no `-> T`) — implicit Unit. Same exclusion as
    // explicit Unit.
    let f = FnSpec {
        name: "no_return",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: None,
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(diags.is_empty());
}

#[test]
fn test_result_return_does_not_fire() {
    // `-> Result[T, E]` is already implicitly must-use per slice 1;
    // firing here would be duplicative noise.
    let f = FnSpec {
        name: "fallible",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Result")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(diags.is_empty());
}

#[test]
fn test_option_return_does_not_fire() {
    let f = FnSpec {
        name: "maybe",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Option")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(diags.is_empty());
}

#[test]
fn test_self_return_does_not_fire() {
    // Spec's `≠ Self` clause — Self-returning shapes are excluded.
    let f = FnSpec {
        name: "identity",
        is_pub: true,
        stdlib_origin: true,
        self_param: Some(SelfParam::Ref),
        return_type: Some(path_type("Self")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(diags.is_empty());
}

#[test]
fn test_impl_target_return_does_not_fire() {
    // `impl Command { fn passthrough(ref self) -> Command }` — return
    // type literally names the impl target. The lint treats this as a
    // Self-shape and skips, matching the spec's exclusion intent.
    let method = FnSpec {
        name: "passthrough",
        is_pub: false,
        stdlib_origin: true,
        self_param: Some(SelfParam::Ref),
        return_type: Some(path_type("Command")),
        attrs: &[],
    }
    .build();
    let diags = lint_one(program_with(vec![impl_block_inherent(
        "Command",
        vec![method],
    )]));
    assert!(diags.is_empty());
}

// ── Negative-space: attribute-based exclusions ───────────────────────

#[test]
fn test_must_use_annotated_does_not_fire() {
    let attr = must_use_attr();
    let f = FnSpec {
        name: "already_annotated",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Iterator")),
        attrs: &[&attr],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(
        diags.is_empty(),
        "`#[must_use]` should suppress the lint, got: {diags:?}"
    );
}

#[test]
fn test_allow_missing_must_use_suppresses() {
    // Future-proofing the lint-level-attributes framework: today the
    // parser accepts `#[allow(...)]` and the lint reads it, even
    // though the broader allow/deny/expect plumbing for non-unsafe
    // rules hasn't shipped yet.
    let attr = allow_missing_must_use_attr();
    let f = FnSpec {
        name: "explicitly_allowed",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Iterator")),
        attrs: &[&attr],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(
        diags.is_empty(),
        "`#[allow(missing_must_use)]` should suppress, got: {diags:?}"
    );
}

// ── Negative-space: scope exclusions ─────────────────────────────────

#[test]
fn test_user_origin_does_not_fire() {
    // The spec's "allow-for-user-code" wording: user-origin items
    // (`stdlib_origin = false`) stay silent unconditionally.
    let f = FnSpec {
        name: "user_fn",
        is_pub: true,
        stdlib_origin: false,
        self_param: None,
        return_type: Some(path_type("Iterator")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(
        diags.is_empty(),
        "user-origin items should be silent, got: {diags:?}"
    );
}

#[test]
fn test_non_pub_free_function_does_not_fire() {
    // Project-internal free functions (no `pub`) are out of scope —
    // the slice talks about the *public* stdlib surface.
    let f = FnSpec {
        name: "internal",
        is_pub: false,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Iterator")),
        attrs: &[],
    };
    let diags = lint_one(program_with(vec![free_fn(f)]));
    assert!(diags.is_empty());
}

#[test]
fn test_trait_impl_method_does_not_fire() {
    // Trait-impl methods inherit `#[must_use]` from the trait
    // declaration. Linting impls would force every implementor to
    // re-annotate. Skip.
    let method = FnSpec {
        name: "fmt_debug",
        is_pub: false,
        stdlib_origin: true,
        self_param: Some(SelfParam::Ref),
        return_type: Some(path_type("String")),
        attrs: &[],
    }
    .build();
    let diags = lint_one(program_with(vec![impl_block_trait(
        "Debug",
        "MyType",
        vec![method],
    )]));
    assert!(
        diags.is_empty(),
        "trait-impl method should be silent, got: {diags:?}"
    );
}

// ── Visibility helper sanity ─────────────────────────────────────────

#[test]
fn test_visibility_helper_aligns_with_is_pub_field() {
    // Pin that the `is_pub` boolean and the `visibility()` helper
    // agree — the lint reads `is_pub` directly, so a future change to
    // the helper that diverges from the boolean would silently shift
    // the lint's scope.
    let f = FnSpec {
        name: "x",
        is_pub: true,
        stdlib_origin: true,
        self_param: None,
        return_type: Some(path_type("Iterator")),
        attrs: &[],
    }
    .build();
    assert!(f.is_pub);
    assert_eq!(f.visibility(), Visibility::Pub);
}

// ── Stdlib-hygiene baseline ──────────────────────────────────────────

#[test]
fn test_stdlib_hygiene_baseline_lint_runs_against_real_baked_source() {
    // Walks every program in `STDLIB_PROGRAMS`, builds a synthetic
    // `Program` with `stdlib_origin` flipped on each top-level item,
    // and runs the lint. The slice ships with a *non-empty* baseline
    // intentionally: existing stdlib `pub fn`s (constructors,
    // pure-transformation accessors, info-only queries) are
    // candidates the lint catches before annotation triage lands.
    // Pinning ≥ 1 here proves the lint is wired end-to-end against
    // real baked source without making every triage commit re-flip
    // the count; future triage shrinks the diagnostic list toward
    // zero.
    let mut all_items: Vec<Item> = Vec::new();
    for (_filename, program) in STDLIB_PROGRAMS.iter() {
        for item in &program.items {
            let mut cloned = item.clone();
            match &mut cloned {
                Item::Function(f) => f.stdlib_origin = true,
                Item::ImplBlock(imp) => {
                    for it in &mut imp.items {
                        if let ImplItem::Method(m) = it {
                            m.stdlib_origin = true;
                        }
                    }
                }
                _ => {}
            }
            all_items.push(cloned);
        }
    }
    let synthetic = program_with(all_items);
    let diags = check_missing_must_use(&synthetic);
    assert!(
        !diags.is_empty(),
        "slice 3 baseline: lint should find at least one stdlib pub fn missing `#[must_use]` (no annotation triage has landed yet — this asserts the lint is wired against real baked source). Got an empty list, which means either the lint regressed or every stdlib candidate has been annotated (in which case flip this assertion to `assert!(diags.is_empty())` and remove the comment)."
    );
    // Sanity: every diagnostic should be a `missing_must_use`
    // warning. A diagnostic with a different lint name would indicate
    // cross-contamination from another lint module.
    for d in &diags {
        assert_eq!(
            d.lint_name, "missing_must_use",
            "expected only missing_must_use diags, got: {d:?}"
        );
        assert_eq!(d.level, LintLevel::Warning);
    }
}
