// tests/resolver.rs

use karac::ast::TraitBound;
use karac::resolver::*;
use karac::{parse, resolve};

// ── Test Helpers ────────────────────────────────────────────────

fn resolve_ok(source: &str) -> ResolveResult {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {}",
        parsed
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let result = resolve(&parsed.program);
    assert!(
        result.errors.is_empty(),
        "Resolve errors: {}",
        result
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    result
}

fn resolve_errors(source: &str) -> Vec<ResolveError> {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {}",
        parsed
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let result = resolve(&parsed.program);
    assert!(
        !result.errors.is_empty(),
        "Expected resolve errors but got none"
    );
    result.errors
}

// ── Scope & Symbol Table ────────────────────────────────────────

#[test]
fn test_empty_program() {
    resolve_ok("");
}

#[test]
fn test_builtin_types_available() {
    // Using primitive types in function signatures should resolve
    resolve_ok("fn foo(x: i64) -> bool { true }");
}

#[test]
fn test_function_registered() {
    let result = resolve_ok("fn hello() { }");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "hello");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::Function { .. }));
}

#[test]
fn test_struct_registered() {
    let result = resolve_ok("struct Point { x: i64, y: i64 }");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "Point");
    assert!(sym.is_some());
    if let SymbolKind::Struct { ref field_names } = sym.unwrap().kind {
        assert_eq!(field_names, &["x", "y"]);
    } else {
        panic!("expected Struct symbol");
    }
}

#[test]
fn test_enum_and_variants() {
    let result = resolve_ok("enum Color { Red, Green, Blue }");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "Color");
    assert!(sym.is_some());
    // Variants should also be registered in global scope
    assert!(result
        .symbol_table
        .lookup_in_scope(ScopeId(0), "Red")
        .is_some());
    assert!(result
        .symbol_table
        .lookup_in_scope(ScopeId(0), "Green")
        .is_some());
    assert!(result
        .symbol_table
        .lookup_in_scope(ScopeId(0), "Blue")
        .is_some());
}

#[test]
fn test_trait_registered() {
    let result = resolve_ok("trait Printable { fn print(self); }");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "Printable");
    assert!(sym.is_some());
    if let SymbolKind::Trait { ref method_names } = sym.unwrap().kind {
        assert_eq!(method_names, &["print"]);
    } else {
        panic!("expected Trait symbol");
    }
}

#[test]
fn test_const_registered() {
    let result = resolve_ok("const MAX: i64 = 100;");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "MAX");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::Constant));
}

#[test]
fn test_effect_resource_registered() {
    let result = resolve_ok("effect resource UserDB;");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "UserDB");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::EffectResource));
}

#[test]
fn test_effect_group_registered() {
    let result = resolve_ok(
        "effect resource UserDB;\n\
         effect group read_all = reads(UserDB);",
    );
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "read_all");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::EffectGroup));
}

#[test]
fn test_type_alias_registered() {
    let result = resolve_ok("type UserId = u64;");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "UserId");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::TypeAlias));
}

#[test]
fn test_use_decl_imports_name() {
    let result = resolve_ok("use std.collections.HashMap;");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "HashMap");
    assert!(sym.is_some());
    if let SymbolKind::Import { ref path } = sym.unwrap().kind {
        assert_eq!(path, &["std", "collections", "HashMap"]);
    } else {
        panic!("expected Import symbol");
    }
}

#[test]
fn test_pub_use_decl_visibility() {
    let result = resolve_ok("pub use db.connection.Connection;");
    let sym = result
        .symbol_table
        .lookup_in_scope(ScopeId(0), "Connection");
    assert!(sym.is_some());
    let sym = sym.unwrap();
    assert!(sym.is_pub);
    if let SymbolKind::Import { ref path } = sym.kind {
        assert_eq!(path, &["db", "connection", "Connection"]);
    } else {
        panic!("expected Import symbol");
    }
}

#[test]
fn test_duplicate_top_level_error() {
    let errors = resolve_errors("fn foo() { }\nfn foo() { }");
    assert!(errors[0].kind == ResolveErrorKind::DuplicateDefinition);
    assert!(errors[0].message.contains("foo"));
}

#[test]
fn test_forward_reference() {
    // bar calls foo which is defined after bar
    resolve_ok(
        "fn bar() { foo(); }\n\
         fn foo() { }",
    );
}

// ── Variable Resolution ─────────────────────────────────────────

#[test]
fn test_let_binding_resolves() {
    resolve_ok("fn main() { let x = 5; x; }");
}

#[test]
fn test_undefined_variable_error() {
    let errors = resolve_errors("fn main() { y; }");
    assert!(errors[0].kind == ResolveErrorKind::UndefinedName);
    assert!(errors[0].message.contains("y"));
}

#[test]
fn test_parameter_in_body() {
    resolve_ok("fn add(a: i64, b: i64) -> i64 { a + b }");
}

#[test]
fn test_shadowing_in_block() {
    resolve_ok(
        "fn main() {\n\
             let x = 1;\n\
             let y = {\n\
                 let x = 2;\n\
                 x\n\
             };\n\
             x;\n\
         }",
    );
}

#[test]
fn test_variable_not_visible_after_block() {
    let errors = resolve_errors(
        "fn main() {\n\
             {\n\
                 let inner = 1;\n\
             }\n\
             inner;\n\
         }",
    );
    assert!(errors[0].message.contains("inner"));
}

#[test]
fn test_for_loop_binding() {
    // 'list' needs to be defined for the for loop to resolve
    resolve_ok(
        "fn process(list: i64) {\n\
             for item in list {\n\
                 item;\n\
             }\n\
         }",
    );
}

#[test]
fn test_closure_params() {
    resolve_ok(
        "fn main() {\n\
             let f = |x: i64, y: i64| x + y;\n\
         }",
    );
}

#[test]
fn test_closure_captures_outer() {
    resolve_ok(
        "fn main() {\n\
             let a = 10;\n\
             let f = |x: i64| x + a;\n\
         }",
    );
}

// ── Expression Resolution ───────────────────────────────────────

#[test]
fn test_function_call() {
    resolve_ok(
        "fn greet() { }\n\
         fn main() { greet(); }",
    );
}

#[test]
fn test_undefined_function_call() {
    let errors = resolve_errors("fn main() { unknown(); }");
    assert!(errors[0].kind == ResolveErrorKind::UndefinedName);
    assert!(errors[0].message.contains("unknown"));
}

#[test]
fn test_struct_literal() {
    resolve_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn main() { let p = Point { x: 1, y: 2 }; }",
    );
}

#[test]
fn test_if_else_resolution() {
    resolve_ok(
        "fn max(a: i64, b: i64) -> i64 {\n\
             if a > b { a } else { b }\n\
         }",
    );
}

#[test]
fn test_match_arm_bindings() {
    resolve_ok(
        "enum Option { Some(i64), None }\n\
         fn main() {\n\
             let x = Some(5);\n\
             match x {\n\
                 Some(v) => v,\n\
                 None => 0,\n\
             };\n\
         }",
    );
}

#[test]
fn test_match_arm_scope_isolation() {
    // 'v' defined in one match arm should not be visible in another
    let errors = resolve_errors(
        "enum Option { Some(i64), None }\n\
         fn main() {\n\
             let x = Some(5);\n\
             match x {\n\
                 Some(v) => v,\n\
                 None => v,\n\
             };\n\
         }",
    );
    assert!(errors.iter().any(|e| e.message.contains("v")));
}

#[test]
fn test_nested_expressions() {
    resolve_ok(
        "fn main() {\n\
             let a = 1;\n\
             let b = 2;\n\
             let c = (a + b) * a - b;\n\
         }",
    );
}

#[test]
fn test_while_loop() {
    resolve_ok(
        "fn main() {\n\
             let mut count = 0;\n\
             while count < 10 {\n\
                 count = count + 1;\n\
             }\n\
         }",
    );
}

#[test]
fn test_loop_and_break() {
    resolve_ok(
        "fn main() {\n\
             let mut x = 0;\n\
             loop {\n\
                 x = x + 1;\n\
                 break;\n\
             }\n\
         }",
    );
}

#[test]
fn test_return_expression() {
    resolve_ok(
        "fn early(x: i64) -> i64 {\n\
             if x > 0 {\n\
                 return x;\n\
             }\n\
             0\n\
         }",
    );
}

// ── Type Resolution ─────────────────────────────────────────────

#[test]
fn test_type_annotation() {
    resolve_ok("fn main() { let x: i64 = 42; }");
}

#[test]
fn test_undefined_type_error() {
    let errors = resolve_errors("fn main() { let x: Nonexistent = 42; }");
    assert!(errors[0].kind == ResolveErrorKind::UndefinedType);
    assert!(errors[0].message.contains("Nonexistent"));
}

#[test]
fn test_generic_type_param() {
    resolve_ok("fn identity[T](x: T) -> T { x }");
}

#[test]
fn test_struct_type_in_function() {
    resolve_ok(
        "struct User { name: String, age: i64 }\n\
         fn greet(user: User) { }",
    );
}

#[test]
fn test_function_return_type() {
    resolve_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn origin() -> Point { Point { x: 0, y: 0 } }",
    );
}

// ── Pattern Resolution ──────────────────────────────────────────

#[test]
fn test_struct_pattern() {
    resolve_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let p = Point { x: 1, y: 2 };\n\
             let Point { x, y } = p;\n\
             x + y;\n\
         }",
    );
}

#[test]
fn test_tuple_pattern() {
    resolve_ok(
        "fn main() {\n\
             let t = (1, 2);\n\
             let (a, b) = t;\n\
             a + b;\n\
         }",
    );
}

#[test]
fn test_enum_variant_pattern() {
    resolve_ok(
        "enum Result { Ok(i64), Err(String) }\n\
         fn handle(r: Result) -> i64 {\n\
             match r {\n\
                 Ok(val) => val,\n\
                 Err(msg) => 0,\n\
             }\n\
         }",
    );
}

// ── Effect Resolution ───────────────────────────────────────────

#[test]
fn test_effect_resource_in_annotation() {
    resolve_ok(
        "effect resource UserDB;\n\
         fn save() writes(UserDB) { }",
    );
}

#[test]
fn test_undefined_effect_resource() {
    let errors = resolve_errors("fn save() writes(Unknown) { }");
    assert!(errors.iter().any(|e| e.message.contains("Unknown")));
}

#[test]
fn test_effect_group_in_with() {
    resolve_ok(
        "effect resource UserDB;\n\
         effect group read_all = reads(UserDB);\n\
         fn load() with read_all { }",
    );
}

#[test]
fn test_multiple_effects() {
    resolve_ok(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         fn process() with reads(UserDB) writes(OrderDB) { }",
    );
}

// ── Impl Blocks ─────────────────────────────────────────────────

#[test]
fn test_impl_self_available() {
    resolve_ok(
        "struct Counter { value: i64 }\n\
         impl Counter {\n\
             fn get(self) -> i64 { self.value }\n\
         }",
    );
}

#[test]
fn test_impl_self_type() {
    resolve_ok(
        "struct Counter { value: i64 }\n\
         impl Counter {\n\
             fn new() -> Self { Counter { value: 0 } }\n\
         }",
    );
}

#[test]
fn test_trait_impl_resolves() {
    resolve_ok(
        "trait Printable { fn print(self); }\n\
         struct Foo { x: i64 }\n\
         impl Printable for Foo {\n\
             fn print(self) { }\n\
         }",
    );
}

// ── Visibility ──────────────────────────────────────────────────

#[test]
fn test_pub_tracking() {
    let result = resolve_ok(
        "pub fn public_fn() { }\n\
         fn private_fn() { }",
    );
    let pub_sym = result
        .symbol_table
        .lookup_in_scope(ScopeId(0), "public_fn")
        .unwrap();
    assert!(pub_sym.is_pub);
    let priv_sym = result
        .symbol_table
        .lookup_in_scope(ScopeId(0), "private_fn")
        .unwrap();
    assert!(!priv_sym.is_pub);
}

// ── Typo Suggestions ────────────────────────────────────────────

#[test]
fn test_typo_suggestion() {
    let errors = resolve_errors(
        "fn main() {\n\
             let count = 0;\n\
             ccount;\n\
         }",
    );
    assert!(errors[0].suggestion.is_some());
    assert_eq!(errors[0].suggestion.as_ref().unwrap(), "count");
}

#[test]
fn test_no_suggestion_for_very_different() {
    let errors = resolve_errors("fn main() { xyz123; }");
    assert!(errors[0].suggestion.is_none());
}

// ── Complex Programs ────────────────────────────────────────────

#[test]
fn test_complex_program() {
    resolve_ok(
        "effect resource FileSystem;\n\
         effect resource UserDB;\n\
         \n\
         struct User {\n\
             name: String,\n\
             age: i64,\n\
         }\n\
         \n\
         enum Status {\n\
             Active,\n\
             Inactive,\n\
         }\n\
         \n\
         trait Validate {\n\
             fn is_valid(self) -> bool;\n\
         }\n\
         \n\
         impl User {\n\
             fn new(name: String, age: i64) -> User {\n\
                 User { name: name, age: age }\n\
             }\n\
         }\n\
         \n\
         impl Validate for User {\n\
             fn is_valid(self) -> bool { self.age > 0 }\n\
         }\n\
         \n\
         fn process(user: User) -> bool reads(UserDB) {\n\
             let status = Active;\n\
             match status {\n\
                 Active => user.age > 18,\n\
                 Inactive => false,\n\
             }\n\
         }\n\
         \n\
         fn main() {\n\
             let u = User { name: \"Alice\", age: 30 };\n\
             let valid = process(u);\n\
         }",
    );
}

#[test]
fn test_multiple_errors_reported() {
    let parsed = parse("fn main() { a; b; c; }");
    assert!(parsed.errors.is_empty());
    let result = resolve(&parsed.program);
    // Should report all three undefined names, not just the first
    assert!(result.errors.len() >= 3);
}

#[test]
fn test_extern_function() {
    resolve_ok(
        "effect resource FileSystem;\n\
         unsafe extern \"C\" { fn write(fd: i32, buf: i64, count: i64) -> i64 writes(FileSystem); }",
    );
}

#[test]
fn test_const_in_expression() {
    resolve_ok(
        "const MAX: i64 = 100;\n\
         fn check(x: i64) -> bool { x < MAX }",
    );
}

#[test]
fn test_generic_function() {
    resolve_ok(
        "trait Display { fn show(self); }\n\
         fn print_item[T: Display](item: T) { item; }",
    );
}

// ── Destructuring in function/closure parameters ─────────────────

#[test]
fn test_tuple_destructuring_param_resolves() {
    resolve_ok("fn add((a, b): (i64, i64)) -> i64 { a + b }");
}

#[test]
fn test_struct_destructuring_param_resolves() {
    resolve_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn get_x(Point { x, y }: Point) -> i64 { x }",
    );
}

#[test]
fn test_wildcard_destructuring_param_resolves() {
    resolve_ok("fn y_only((_, y): (i64, i64)) -> i64 { y }");
}

#[test]
fn test_mixed_params_with_destructuring_resolves() {
    resolve_ok("fn foo(name: i64, (a, b): (i64, i64)) -> i64 { name + a + b }");
}

#[test]
fn test_closure_destructuring_param_resolves() {
    resolve_ok("fn main() { let f = |(a, b)| a; }");
}

#[test]
fn test_nested_tuple_destructuring_param_resolves() {
    resolve_ok("fn nested(((a, b), c): ((i64, i64), i64)) -> i64 { a + b + c }");
}

// ── Generic Params on Structs/Enums/Traits ─────────────────────

#[test]
fn test_generic_struct_field_resolves() {
    resolve_ok("struct Box[T] { value: T }");
}

#[test]
fn test_generic_enum_variant_resolves() {
    resolve_ok("enum Option[T] { Some(T), None }");
}

#[test]
fn test_generic_trait_method_resolves() {
    resolve_ok(
        "trait Container[T] {\n\
             fn get(self) -> T;\n\
         }",
    );
}

#[test]
fn test_generic_impl_block_resolves() {
    resolve_ok(
        "struct Wrapper[T] { inner: T }\n\
         impl[T] Wrapper[T] {\n\
             fn unwrap(self) -> T { self.inner }\n\
         }",
    );
}

// ── Self/Self Outside Impl ─────────────────────────────────────

#[test]
fn test_self_value_outside_impl_error() {
    let errors = resolve_errors("fn main() { self; }");
    assert!(errors
        .iter()
        .any(|e| e.message.contains("self") && e.message.contains("outside")));
}

#[test]
fn test_self_type_outside_impl_error() {
    // Self as a return type outside of an impl block
    let errors = resolve_errors("fn make() -> Self { }");
    assert!(errors.iter().any(|e| e.message.contains("Self")));
}

// ── Module Declaration ─────────────────────────────────────────

#[test]
fn test_process_builtin_module_registered() {
    // `mod name;` is rejected at parse time per CR-24 slice 9, so the
    // resolver no longer registers user-declared modules. The only
    // `SymbolKind::Module` left in scope-0 is the `process` builtin.
    let result = resolve_ok("");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "process");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::Module));
}

// ── Effect Verb Declaration ────────────────────────────────────

#[test]
fn test_effect_verb_registered() {
    let result = resolve_ok("effect verb logs;");
    let sym = result.symbol_table.lookup_in_scope(ScopeId(0), "logs");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::EffectVerb));
}

// ── Qualified Path ─────────────────────────────────────────────

#[test]
fn test_qualified_enum_variant_path() {
    resolve_ok(
        "enum Color { Red, Green, Blue }\n\
         fn main() { let c = Color.Red; }",
    );
}

// ── Reserved identifiers ────────────────────────────────────────

#[test]
fn test_reserved_identifier_fn_type() {
    // `Fn` triggers both a parser naming error (Type-class fn name) and a resolver
    // ReservedIdentifier error. Parse with errors allowed, then check the resolver.
    let parsed = parse("fn Fn() {}");
    let result = resolve(&parsed.program);
    assert!(result
        .errors
        .iter()
        .any(|e| e.kind == ResolveErrorKind::ReservedIdentifier && e.message.contains("Fn")));
}

#[test]
fn test_reserved_identifier_split_by_variant() {
    let errors = resolve_errors("fn main() { let split_by_variant = 1; }");
    assert!(errors
        .iter()
        .any(|e| e.kind == ResolveErrorKind::ReservedIdentifier
            && e.message.contains("split_by_variant")));
}

#[test]
fn test_non_reserved_identifiers_ok() {
    // Similar names should be fine
    resolve_ok("fn main() { let fn_ptr = 1; let splitter = 2; }");
}

// ── Labeled Loop Validation ────────────────────────────────────

#[test]
fn test_break_undefined_label() {
    // Use `continue` to test undefined labels — no value ambiguity.
    // `break` with an unknown identifier is parsed as `break <value-expr>`,
    // not as a labeled break, since the parser only recognizes known loop labels.
    let errors = resolve_errors("fn main() { loop { continue unknown_label; } }");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ResolveErrorKind::UndefinedLabel
                && e.message.contains("unknown_label"))
    );
}

#[test]
fn test_break_wrong_label() {
    // `outer:` is a known label, but `wrong` is not — resolver catches it via continue
    let errors = resolve_errors("fn main() { outer: loop { loop { continue wrong; } } }");
    assert!(errors
        .iter()
        .any(|e| e.kind == ResolveErrorKind::UndefinedLabel && e.message.contains("wrong")));
}

#[test]
fn test_continue_undefined_label() {
    let errors = resolve_errors("fn main() { loop { continue unknown_label; } }");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ResolveErrorKind::UndefinedLabel
                && e.message.contains("unknown_label"))
    );
}

#[test]
fn test_break_valid_label_ok() {
    resolve_ok("fn main() { outer: loop { loop { break outer (); } } }");
}

#[test]
fn test_continue_valid_label_ok() {
    resolve_ok("fn main() { outer: while true { while true { continue outer; } } }");
}

#[test]
fn test_break_label_with_value_undefined() {
    // With a known outer label, break with a wrong label is caught by the resolver
    let errors = resolve_errors("fn main() { outer: loop { loop { continue nope; } } }");
    assert!(errors
        .iter()
        .any(|e| e.kind == ResolveErrorKind::UndefinedLabel && e.message.contains("nope")));
}

#[test]
fn test_labeled_for_loop_ok() {
    resolve_ok("fn main() { outer: for x in [1, 2] { for y in [3, 4] { break outer (); } } }");
}

// ── Labeled Block Validation ───────────────────────────────────

#[test]
fn test_labeled_block_nested_same_name_shadows() {
    // Inner labeled block shadows outer same-name label; inner
    // `break label` resolves to inner. Outer `break label` after
    // exiting inner refers to outer. Resolver-level check: both
    // breaks resolve without an unknown-label diagnostic.
    resolve_ok("fn main() { lbl: { lbl: { break lbl; } break lbl; } }");
}

#[test]
fn test_labeled_block_unknown_label_diagnostic() {
    // `continue never_declared;` outside any labeled construct
    // produces UndefinedLabel — same diagnostic family used for
    // labeled loops. (Use `continue` rather than `break <expr>`
    // to avoid the parser swallowing the identifier as a value.)
    let errors = resolve_errors("fn main() { loop { continue never_declared; } }");
    assert!(errors.iter().any(|e| {
        e.kind == ResolveErrorKind::UndefinedLabel && e.message.contains("never_declared")
    }));
}

#[test]
fn test_continue_label_to_block_rejected() {
    // `lbl: { continue lbl; }` is rejected with the new
    // E_CONTINUE_LABEL_BLOCK diagnostic — `continue` is only valid
    // for loop labels, not for labeled blocks.
    let errors = resolve_errors("fn main() { lbl: { continue lbl; } }");
    let hit = errors.iter().find(|e| {
        e.kind == ResolveErrorKind::ContinueOnBlockLabel
            && e.message.contains("E_CONTINUE_LABEL_BLOCK")
            && e.message.contains("labeled block")
            && e.message.contains("`lbl`")
    });
    assert!(
        hit.is_some(),
        "expected ContinueOnBlockLabel diagnostic; got: {:?}",
        errors
    );
    // The diagnostic carries a fix-it suggestion (renames or
    // restructure as loop).
    let hit = hit.unwrap();
    assert!(
        hit.suggestion.is_some(),
        "expected suggestion on ContinueOnBlockLabel, got None"
    );
}

#[test]
fn test_break_inside_closure_cannot_target_enclosing_label() {
    // LB4 — closure-boundary rule. A `break label` inside a closure
    // body cannot target an enclosing labeled block; the resolver
    // surfaces this as `undefined loop label`. Same shape with a
    // labeled `for` loop also rejects (audit-finding fix: the
    // labeled-loop closure-boundary gap was missed before this slice).
    let errors_block = resolve_errors("fn main() { lbl: { let f = || { break lbl; }; f(); } }");
    assert!(
        errors_block
            .iter()
            .any(|e| e.kind == ResolveErrorKind::UndefinedLabel && e.message.contains("lbl")),
        "labeled-block closure-boundary not enforced: {:?}",
        errors_block
    );

    // Labeled-loop variant — fixes the audit-finding gap (LB4 fixes
    // the loop-side closure-boundary rule as a side-effect).
    let errors_loop =
        resolve_errors("fn main() { lbl: for x in [1, 2] { let f = || { continue lbl; }; f(); } }");
    assert!(
        errors_loop
            .iter()
            .any(|e| e.kind == ResolveErrorKind::UndefinedLabel && e.message.contains("lbl")),
        "labeled-loop closure-boundary not enforced: {:?}",
        errors_loop
    );
}

#[test]
fn test_labeled_block_label_scope_ends_at_closing_brace() {
    // The label scope ends at the closing `}` of the labeled
    // block. A subsequent `continue lbl;` at the same scope as the
    // labeled block's parent rejects with UndefinedLabel.
    let errors = resolve_errors("fn main() { loop { lbl: { } continue lbl; } }");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ResolveErrorKind::UndefinedLabel && e.message.contains("lbl")),
        "label scope should end at closing brace; errors: {:?}",
        errors
    );
}

// ── Built-in Module Resolution ────────────────────────────────

#[test]
fn test_process_module_resolves() {
    resolve_ok("fn main() { process.exit(0); }");
}

#[test]
fn test_f32_f64_total_order_types_resolve() {
    resolve_ok("fn main() { let x: F32 = F32 { value: 1.0 }; let y: F64 = F64 { value: 2.0 }; }");
}

#[test]
fn test_ordering_variants_resolve() {
    // Comparison-Ordering variants (Less / Equal / Greater)
    resolve_ok("fn main() { let lt = Ordering.Less; let eq = Ordering.Equal; }");
}

#[test]
fn test_memory_ordering_variants_resolve() {
    resolve_ok("fn main() { let r = MemoryOrdering.Relaxed; let a = MemoryOrdering.Acquire; }");
}

#[test]
fn test_atomic_resolves() {
    resolve_ok("fn main() { let a = Atomic.new(0); }");
}

// ── Layout validation ────────────────────────────────────────────

#[test]
fn test_layout_valid_all_fields_assigned() {
    resolve_ok(
        r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group physics { x, y }
    group combat { hp }
}
"#,
    );
}

#[test]
fn test_layout_unknown_field_error() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group physics { x, nonexistent }
}
"#,
    );
    assert!(
        errors.iter().any(|e| e.message.contains("nonexistent")),
        "expected error about unknown field, got: {:?}",
        errors
    );
}

#[test]
fn test_layout_duplicate_field_error() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group a { x }
    group b { x }
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("multiple sections")),
        "expected duplicate field error, got: {:?}",
        errors
    );
}

#[test]
fn test_layout_unassigned_fields_warning() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group physics { x }
}
"#,
    );
    assert!(
        errors.iter().any(|e| e.message.contains("not assigned")),
        "expected unassigned fields warning, got: {:?}",
        errors
    );
}

// ── Layout: cold section and align(N) ────────────────────────

#[test]
fn test_layout_cold_section_valid() {
    resolve_ok(
        r#"
struct Entity { x: f64, y: f64, hp: i64, name: i64 }
layout entities: Vec[Entity] {
    group physics { x, y }
    cold { hp, name }
}
"#,
    );
}

#[test]
fn test_layout_cold_duplicate_cold_section_error() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group physics { x }
    cold { y }
    cold { hp }
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("more than one cold")),
        "expected duplicate cold section error, got: {:?}",
        errors
    );
}

#[test]
fn test_layout_cold_field_also_in_group_error() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group physics { x, y }
    cold { y }
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("multiple sections")),
        "expected field-in-both-group-and-cold error, got: {:?}",
        errors
    );
}

#[test]
fn test_layout_cold_unknown_field_error() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64, y: f64 }
layout entities: Vec[Entity] {
    group physics { x }
    cold { no_such_field }
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("does not exist") && e.message.contains("cold section")),
        "expected unknown-field error in cold section, got: {:?}",
        errors
    );
}

#[test]
fn test_layout_align_valid_power_of_two() {
    resolve_ok(
        r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group a { x, y } align(64)
    group b { hp } align(128)
}
"#,
    );
}

#[test]
fn test_layout_align_not_power_of_two_error() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64, y: f64 }
layout entities: Vec[Entity] {
    group a { x, y } align(63)
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("not a power of two")),
        "expected align-not-power-of-two error, got: {:?}",
        errors
    );
}

#[test]
fn test_layout_align_zero_error() {
    let errors = resolve_errors(
        r#"
struct Entity { x: f64 }
layout entities: Vec[Entity] {
    group a { x } align(0)
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("not a power of two")),
        "expected align(0) error, got: {:?}",
        errors
    );
}

// ── Operator-trait impl restriction (Step 7) ──────────────────

#[test]
fn test_user_impl_add_for_struct_rejected() {
    let errors = resolve_errors(
        "struct Vector { x: f64, y: f64 }\n\
         impl Add for Vector {\n\
             fn add(self, rhs: Vector) -> Vector { self }\n\
         }",
    );
    assert!(
        errors.iter().any(|e| matches!(
            e.kind,
            karac::resolver::ResolveErrorKind::OperatorTraitImplRestricted
        )),
        "expected operator-trait restriction error, got: {:?}",
        errors
    );
}

#[test]
fn test_user_impl_add_for_vec_has_concat_hint() {
    let errors = resolve_errors(
        "impl Add for Vec {\n\
             fn add(self, rhs: Vec) -> Vec { self }\n\
         }",
    );
    let err = errors.iter().find(|e| {
        matches!(
            e.kind,
            karac::resolver::ResolveErrorKind::OperatorTraitImplRestricted
        )
    });
    assert!(
        err.is_some(),
        "expected operator-trait restriction error, got: {:?}",
        errors
    );
    let err = err.unwrap();
    assert!(err.message.contains("Vec"), "message: {}", err.message);
    let suggestion = err.suggestion.as_deref().unwrap_or("");
    assert!(
        suggestion.contains("concat") || suggestion.contains("extend"),
        "expected concat/extend hint in suggestion, got: {}",
        suggestion
    );
}

#[test]
fn test_user_impl_from_still_allowed() {
    // From is a conversion trait, not an operator trait — user impls remain
    // legal (required for `?` cross-error propagation).
    resolve_ok(
        "struct ParseError { msg: String }\n\
         struct AppError { msg: String }\n\
         impl From for AppError {\n\
             fn from(e: ParseError) -> AppError { AppError { msg: e.msg } }\n\
         }",
    );
}

#[test]
fn test_user_impl_eq_ord_for_struct_allowed() {
    // Eq/Ord are carved out of the operator-trait restriction — user types
    // routinely need custom equality/ordering, and the operator lowering
    // pass routes `==`/`<` through these impls. CR-202 slice 5b added
    // `Eq: PartialEq` as a supertrait edge, so `impl Eq for Point` now
    // also requires `impl PartialEq for Point` (typechecker-level
    // `MissingSupertrait`; resolver-level passes are unaffected, but the
    // impl is added here so the same source resolves cleanly through
    // typecheck too).
    resolve_ok(
        "struct Point { x: i64, y: i64 }\n\
         impl PartialEq for Point {\n\
             fn eq(ref self, other: ref Point) -> bool { self.x == other.x and self.y == other.y }\n\
         }\n\
         impl Eq for Point {\n\
             fn eq(self, other: Point) -> bool { self.x == other.x and self.y == other.y }\n\
         }\n\
         impl PartialOrd for Point {\n\
             fn partial_cmp(ref self, other: ref Point) -> Option[Ordering] { Some(self.x.cmp(other.x)) }\n\
         }\n\
         impl Ord for Point {\n\
             fn lt(self, other: Point) -> bool { self.x < other.x }\n\
             fn le(self, other: Point) -> bool { self.x <= other.x }\n\
             fn gt(self, other: Point) -> bool { self.x > other.x }\n\
             fn ge(self, other: Point) -> bool { self.x >= other.x }\n\
         }",
    );
}

#[test]
fn test_user_impl_arithmetic_still_rejected_after_relational_carveout() {
    // The relational-trait carve-out is surgical: Add/Sub/etc. on user types
    // stay rejected until the heterogeneous `Rhs`/`Output` design lands.
    let errors = resolve_errors(
        "struct Vector { x: f64 }\n\
         impl Mul for Vector {\n\
             fn mul(self, rhs: Vector) -> Vector { self }\n\
         }",
    );
    assert!(
        errors.iter().any(|e| matches!(
            e.kind,
            karac::resolver::ResolveErrorKind::OperatorTraitImplRestricted
        )),
        "expected Mul to still be rejected, got: {:?}",
        errors
    );
}

#[test]
fn test_user_impl_into_rejected() {
    // `impl Into` is derived from `From` via a blanket impl — user impls are
    // rejected with a suggestion to implement `From` instead.
    let errors = resolve_errors(
        "struct Inches { n: i64 }\n\
         struct Cm { n: i64 }\n\
         impl Into for Inches {\n\
             fn into(self) -> Cm { Cm { n: 0 } }\n\
         }",
    );
    assert!(
        errors.iter().any(|e| matches!(
            e.kind,
            karac::resolver::ResolveErrorKind::IntoTraitImplNotAllowed
        )),
        "expected Into impl to be rejected, got: {:?}",
        errors
    );
}

#[test]
fn test_user_impl_tryinto_rejected() {
    let errors = resolve_errors(
        "struct Narrow { n: i8 }\n\
         impl TryInto for i64 {\n\
             fn try_into(self) -> Result[Narrow, String] { Err(\"\") }\n\
         }",
    );
    assert!(
        errors.iter().any(|e| matches!(
            e.kind,
            karac::resolver::ResolveErrorKind::IntoTraitImplNotAllowed
        )),
        "expected TryInto impl to be rejected, got: {:?}",
        errors
    );
}

#[test]
fn test_impl_level_effect_var_rejected() {
    // `impl[..., with E] Trait for Type` is rejected: effect polymorphism is
    // expressed via `with _` on the trait method, not bound at impl level.
    // (Round 6 step 4 — design.md § Conversion Traits.)
    let errors = resolve_errors(
        "trait Sink { fn drain(ref self); }\n\
         struct LogSink { name: String }\n\
         impl[with E] Sink for LogSink {\n\
             fn drain(ref self) {}\n\
         }",
    );
    assert!(
        errors.iter().any(|e| matches!(
            e.kind,
            karac::resolver::ResolveErrorKind::ImplLevelEffectVarNotAllowed
        )),
        "expected ImplLevelEffectVarNotAllowed, got: {:?}",
        errors
    );
    let err = errors
        .iter()
        .find(|e| {
            matches!(
                e.kind,
                karac::resolver::ResolveErrorKind::ImplLevelEffectVarNotAllowed
            )
        })
        .unwrap();
    assert!(
        err.message.contains("`E`"),
        "diagnostic should name the offending effect variable, got: {}",
        err.message
    );
    assert!(
        err.suggestion
            .as_ref()
            .is_some_and(|s| s.contains("with _")),
        "suggestion should point at `with _` on the trait method, got: {:?}",
        err.suggestion
    );
}

#[test]
fn test_impl_with_type_params_only_accepted() {
    // Regression: `impl[T] Trait[T] for Wrapper { ... }` — type-only generics
    // on impl are unaffected by step 4. Only `with E` (effect-variable) impl
    // generics are rejected.
    resolve_ok(
        "trait Container[T] { fn get(ref self) -> T; }\n\
         struct Wrapper[T] { value: T }\n\
         impl[T] Container[T] for Wrapper[T] {\n\
             fn get(ref self) -> T { self.value }\n\
         }",
    );
}

// ── `providers { }` block resource-name resolution ───────────────

#[test]
fn test_providers_block_rejects_undefined_resource() {
    let errors = resolve_errors(
        "fn mk() -> i64 { 0 }\n\
         fn main() { providers { UnknownResource => mk() } in { } }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ResolveErrorKind::UndefinedName
                && e.message.contains("UnknownResource")),
        "expected UndefinedName for UnknownResource, got {:?}",
        errors
    );
}

#[test]
fn test_providers_block_rejects_non_effect_resource_name() {
    // `Point` is a struct — valid symbol, wrong kind.
    let errors = resolve_errors(
        "struct Point { x: i64 }\n\
         fn mk() -> i64 { 0 }\n\
         fn main() { providers { Point => mk() } in { } }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("not an effect resource")),
        "expected 'not an effect resource' error, got {:?}",
        errors
    );
}

#[test]
fn test_providers_block_accepts_known_effect_resource() {
    let _ = resolve_ok(
        "effect resource UserDB;\n\
         struct Db { tag: i64 }\n\
         fn main() { providers { UserDB => Db { tag: 0 } } in { } }",
    );
}

// ── Reserved effect resource names (E0228) ──────────────────────

#[test]
fn test_compile_time_env_reserved() {
    let errors = resolve_errors("effect resource CompileTimeEnv;");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ResolveErrorKind::ReservedEffectResource
                && e.message.contains("CompileTimeEnv")),
        "expected ReservedEffectResource for CompileTimeEnv, got {:?}",
        errors
    );
}

#[test]
fn test_compile_time_heap_reserved() {
    let errors = resolve_errors("effect resource CompileTimeHeap;");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == ResolveErrorKind::ReservedEffectResource
                && e.message.contains("CompileTimeHeap")),
        "expected ReservedEffectResource for CompileTimeHeap, got {:?}",
        errors
    );
}

#[test]
fn test_reserved_effect_resource_not_registered_in_scope() {
    // The reserved name must not be inserted into the symbol table,
    // so a subsequent reference to it should be an undefined-resource error.
    let parsed = karac::parse(
        "effect resource CompileTimeEnv;\n\
         effect group g = reads(CompileTimeEnv);",
    );
    assert!(parsed.errors.is_empty());
    let result = karac::resolve(&parsed.program);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == ResolveErrorKind::ReservedEffectResource),
        "expected ReservedEffectResource error, got {:?}",
        result.errors
    );
    // Must NOT be present in the symbol table.
    assert!(
        result
            .symbol_table
            .lookup_in_scope(karac::resolver::ScopeId(0), "CompileTimeEnv")
            .is_none(),
        "reserved resource should not appear in the symbol table"
    );
}

#[test]
fn test_non_reserved_effect_resource_still_works() {
    // Names that are merely similar must not be rejected.
    let result = resolve_ok("effect resource CompileTimeStorage;");
    let sym = result
        .symbol_table
        .lookup_in_scope(ScopeId(0), "CompileTimeStorage");
    assert!(sym.is_some());
    assert!(matches!(sym.unwrap().kind, SymbolKind::EffectResource));
}

// ── Trait-bound tracking on generic params ──────────────────────

/// Bound trait names recorded for the (single) generic param named `param_name`.
/// Each `TraitBound`'s last path segment is what the typechecker checks against.
fn bounds_for_param(table: &SymbolTable, param_name: &str) -> Vec<String> {
    let entry = table
        .generic_param_bounds
        .iter()
        .find(|(id, _)| table.get_symbol(**id).name == param_name);
    let bounds: &[TraitBound] = entry.map(|(_, bs)| bs.as_slice()).unwrap_or(&[]);
    bounds
        .iter()
        .map(|b| b.path.last().cloned().unwrap_or_default())
        .collect()
}

#[test]
fn test_inline_bound_recorded_on_function_param() {
    let result = resolve_ok("fn sort[T: Ord](items: T) -> T { items }");
    let bounds = bounds_for_param(&result.symbol_table, "T");
    assert_eq!(bounds, vec!["Ord".to_string()]);
}

#[test]
fn test_multiple_inline_bounds_recorded() {
    let result = resolve_ok("fn f[T: Eq + Ord](x: T) -> T { x }");
    let mut bounds = bounds_for_param(&result.symbol_table, "T");
    bounds.sort();
    assert_eq!(bounds, vec!["Eq".to_string(), "Ord".to_string()]);
}

#[test]
fn test_where_clause_bound_recorded_on_function_param() {
    let result = resolve_ok("fn sort[T](items: T) -> T where T: Ord { items }");
    let bounds = bounds_for_param(&result.symbol_table, "T");
    assert_eq!(bounds, vec!["Ord".to_string()]);
}

#[test]
fn test_inline_and_where_clause_bounds_merged() {
    let result = resolve_ok("fn f[T: Eq](x: T) -> T where T: Ord { x }");
    let mut bounds = bounds_for_param(&result.symbol_table, "T");
    bounds.sort();
    assert_eq!(bounds, vec!["Eq".to_string(), "Ord".to_string()]);
}

#[test]
fn test_inline_bound_recorded_on_struct_param() {
    let result = resolve_ok("struct Sorted[T: Ord] { items: T }");
    let bounds = bounds_for_param(&result.symbol_table, "T");
    assert_eq!(bounds, vec!["Ord".to_string()]);
}

#[test]
fn test_where_clause_bound_recorded_on_enum_param() {
    let result = resolve_ok("enum Pair[T] where T: Eq { Single(T), Double(T, T) }");
    let bounds = bounds_for_param(&result.symbol_table, "T");
    assert_eq!(bounds, vec!["Eq".to_string()]);
}

#[test]
fn test_supertrait_recorded_as_self_bound() {
    // `trait Foo: Bar` makes `Bar` a logical bound on Self inside the trait.
    let result = resolve_ok("trait Foo: Eq { fn check(self) -> bool; }");
    let bounds = bounds_for_param(&result.symbol_table, "Self");
    assert_eq!(bounds, vec!["Eq".to_string()]);
}

#[test]
fn test_no_bounds_when_param_has_none() {
    let result = resolve_ok("fn id[T](x: T) -> T { x }");
    let bounds = bounds_for_param(&result.symbol_table, "T");
    assert!(bounds.is_empty());
}

#[test]
fn test_trait_bound_path_resolution_recorded() {
    // Inline bound's trait path should be resolved against the symbol table.
    // `Ord` is a prelude trait; its SymbolId should appear in the resolution
    // map keyed by the bound's span.
    let result = resolve_ok("fn sort[T: Ord](items: T) -> T { items }");
    // Find any resolution that points to a TypeParam Ord (registered as a
    // prelude Primitive with name "Ord").
    let any_ord_resolution = result.resolutions.values().any(|&id| {
        let sym = result.symbol_table.get_symbol(id);
        sym.name == "Ord"
    });
    assert!(
        any_ord_resolution,
        "expected a resolution pointing at the prelude `Ord` trait"
    );
}

#[test]
fn test_resolver_const_param_kind() {
    // `fn f[const N: i64]` defines `N` as a `SymbolKind::ConstParam`, not
    // `SymbolKind::TypeParam` (const generics slice 1, sub-step c). The
    // declaration-site permitted-type check in the typechecker branches on
    // this distinction.
    let result = resolve_ok("fn f[const N: i64](x: i64) -> i64 { x }");
    let n_kind = result
        .symbol_table
        .all_symbols()
        .iter()
        .find(|s| s.name == "N")
        .map(|s| &s.kind)
        .expect("expected a symbol named 'N'");
    assert!(
        matches!(n_kind, SymbolKind::ConstParam),
        "expected SymbolKind::ConstParam for 'N', got {:?}",
        n_kind
    );
}

#[test]
fn test_resolver_const_type_name_collision() {
    // `fn f[T, const T: i64]` — type-param and const-param share the name
    // `T`. The resolver's existing duplicate-name detection fires; no
    // kind-aware special-casing required.
    let errs = resolve_errors("fn f[T, const T: i64](x: i64) -> i64 { x }");
    assert!(
        !errs.is_empty(),
        "expected a duplicate-name diagnostic for type/const param collision"
    );
}

#[test]
fn test_impl_block_generic_bound_recorded() {
    let result = resolve_ok(
        "struct Wrapper[T] { value: T }\n\
         impl[T: Eq] Wrapper[T] { fn check(self) -> bool { true } }",
    );
    // The impl-level T's bounds should be recorded.
    let bounds = bounds_for_param(&result.symbol_table, "T");
    assert!(
        bounds.contains(&"Eq".to_string()),
        "expected Eq among recorded bounds, got {:?}",
        bounds
    );
}

// ── #[compiler_builtin] gate (CR-202 slice 1, E0237) ─────────────
// The attribute is reserved for stdlib source baked into the compiler
// binary. User code is rejected at the resolver layer; the synthetic
// stdlib package opts in via `Resolver::with_stdlib_source(true)` (slice 3
// will wire that path through the bake step).

fn assert_compiler_builtin_rejected(source: &str) {
    let errs = resolve_errors(source);
    assert!(
        errs.iter()
            .any(|e| e.kind == ResolveErrorKind::CompilerBuiltinReserved),
        "expected CompilerBuiltinReserved error, got: {:?}",
        errs.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_compiler_builtin_rejected_on_function_in_user_code() {
    assert_compiler_builtin_rejected("#[compiler_builtin]\nfn dbg[T](v: T) -> T { v }");
}

#[test]
fn test_compiler_builtin_rejected_on_struct_in_user_code() {
    assert_compiler_builtin_rejected("#[compiler_builtin]\nstruct Vec[T] { }");
}

#[test]
fn test_compiler_builtin_rejected_on_enum_in_user_code() {
    assert_compiler_builtin_rejected("#[compiler_builtin]\nenum Option[T] { Some(T), None }");
}

#[test]
fn test_compiler_builtin_rejected_on_trait_in_user_code() {
    assert_compiler_builtin_rejected(
        "#[compiler_builtin]\ntrait Display { fn fmt(ref self) -> String; }",
    );
}

#[test]
fn test_compiler_builtin_permitted_with_stdlib_source_flag() {
    // `with_stdlib_source(true)` is the opt-in path the bake step (slice 3)
    // will use. The attribute should pass cleanly through the resolver and
    // produce no diagnostic of any kind.
    let parsed = parse(
        "#[compiler_builtin]\nfn dbg[T](v: T) -> T { v }\n\
         #[compiler_builtin]\nstruct Vec[T] { }\n\
         #[compiler_builtin]\nenum Option[T] { Some(T), None }\n\
         #[compiler_builtin]\ntrait Display { fn fmt(ref self) -> String; }",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let result = Resolver::new(&parsed.program)
        .with_stdlib_source(true)
        .resolve();
    let unrelated: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind != ResolveErrorKind::CompilerBuiltinReserved)
        .collect();
    assert!(
        result
            .errors
            .iter()
            .all(|e| e.kind != ResolveErrorKind::CompilerBuiltinReserved),
        "stdlib-source flag should suppress CompilerBuiltinReserved",
    );
    assert!(
        unrelated.is_empty(),
        "expected no other resolver errors, got: {:?}",
        unrelated.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_compiler_builtin_rejection_does_not_block_definition() {
    // The error fires alongside symbol definition — downstream references
    // to the rejected name should still resolve, so users see the gating
    // diagnostic in isolation rather than a cascade of "undefined name".
    let parsed = parse(
        "#[compiler_builtin]\nfn dbg[T](v: T) -> T { v }\n\
         fn use_it() { let _ = dbg(1); }",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let errs = resolve(&parsed.program).errors;
    let undef: Vec<_> = errs
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::UndefinedName)
        .collect();
    assert!(
        undef.is_empty(),
        "rejected #[compiler_builtin] item should still be defined; got UndefinedName: {:?}",
        undef.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == ResolveErrorKind::CompilerBuiltinReserved),
        "expected CompilerBuiltinReserved among errors"
    );
}

// ── Per-item stdlib-origin tag (CR-202 slice 3b) ────────────────
// The bake step in slice 3c flips `Item::*.stdlib_origin = true` on
// items spliced from `runtime/stdlib/*.kara`. The resolver runs in
// user-mode for the spliced program tree (`is_stdlib_source == false`),
// so the gate must consult per-item origin to let baked items use
// `#[compiler_builtin]` even when the session-wide flag is unset.

fn flip_stdlib_origin_on_first_item(prog: &mut karac::ast::Program) {
    use karac::ast::Item;
    match prog
        .items
        .first_mut()
        .expect("program has at least one item")
    {
        Item::Function(f) => f.stdlib_origin = true,
        Item::StructDef(s) => s.stdlib_origin = true,
        Item::EnumDef(e) => e.stdlib_origin = true,
        Item::TraitDef(t) => t.stdlib_origin = true,
        other => panic!("first item is not a kind we tag: {:?}", other),
    }
}

fn assert_per_item_origin_bypasses_gate(source: &str) {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let mut program = parsed.program;
    flip_stdlib_origin_on_first_item(&mut program);
    // is_stdlib_source defaults to false — per-item tag is the only thing
    // that can suppress the gate here.
    let result = Resolver::new(&program).resolve();
    let blocked: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::CompilerBuiltinReserved)
        .collect();
    assert!(
        blocked.is_empty(),
        "per-item stdlib_origin should suppress CompilerBuiltinReserved, got: {:?}",
        blocked.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_compiler_builtin_per_item_origin_bypasses_gate_on_function() {
    assert_per_item_origin_bypasses_gate("#[compiler_builtin]\nfn dbg[T](v: T) -> T { v }");
}

#[test]
fn test_compiler_builtin_per_item_origin_bypasses_gate_on_struct() {
    assert_per_item_origin_bypasses_gate("#[compiler_builtin]\nstruct Vec[T] { }");
}

#[test]
fn test_compiler_builtin_per_item_origin_bypasses_gate_on_enum() {
    assert_per_item_origin_bypasses_gate("#[compiler_builtin]\nenum Option[T] { Some(T), None }");
}

#[test]
fn test_compiler_builtin_per_item_origin_bypasses_gate_on_trait() {
    assert_per_item_origin_bypasses_gate(
        "#[compiler_builtin]\ntrait Display { fn fmt(ref self) -> String; }",
    );
}

#[test]
fn test_compiler_builtin_per_item_tag_is_per_item_not_global() {
    // Two items with `#[compiler_builtin]`. The first is tagged with
    // stdlib_origin = true (bypasses the gate). The second is left at
    // stdlib_origin = false (still rejected). Confirms the tag is
    // per-item, not implicitly applied to siblings.
    let parsed = parse(
        "#[compiler_builtin]\nfn baked[T](v: T) -> T { v }\n\
         #[compiler_builtin]\nfn user_attempt[T](v: T) -> T { v }",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let mut program = parsed.program;
    flip_stdlib_origin_on_first_item(&mut program);
    let result = Resolver::new(&program).resolve();
    let rejections: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::CompilerBuiltinReserved)
        .collect();
    assert_eq!(
        rejections.len(),
        1,
        "expected exactly one rejection (the untagged sibling), got: {:?}",
        rejections.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// ── #[compiler_builtin] gate on impl blocks ──────────────────────
// Slice 1 closed the gate on top-level items (`fn`/`struct`/`enum`/`trait`).
// Impl blocks were the documented loophole (phase-4-interpreter.md slice 6.3
// findings #2): `collect_impl` did not call `check_compiler_builtin_attr`,
// so user code could sneak `#[compiler_builtin]` past the resolver via
// `impl Foo { #[compiler_builtin] fn bar() { } }`. Closing it: the gate
// fires on both the impl block's own attributes and each method's, with
// the per-method `stdlib_origin` exemption preserved for baked source.

#[test]
fn test_compiler_builtin_rejected_on_impl_method_in_user_code() {
    assert_compiler_builtin_rejected(
        "struct Foo { }\n\
         impl Foo {\n\
             #[compiler_builtin]\n\
             fn bar(self) -> i64 { 0 }\n\
         }",
    );
}

#[test]
fn test_compiler_builtin_rejected_on_impl_block_itself_in_user_code() {
    assert_compiler_builtin_rejected(
        "struct Foo { }\n\
         #[compiler_builtin]\n\
         impl Foo {\n\
             fn bar(self) -> i64 { 0 }\n\
         }",
    );
}

#[test]
fn test_compiler_builtin_permitted_on_impl_method_with_stdlib_source_flag() {
    // The session-wide bypass (`with_stdlib_source(true)`) should suppress
    // the gate on both impl-block-level and method-level `#[compiler_builtin]`.
    let parsed = parse(
        "struct Foo { }\n\
         #[compiler_builtin]\n\
         impl Foo {\n\
             #[compiler_builtin]\n\
             fn bar(self) -> i64 { 0 }\n\
         }",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let result = Resolver::new(&parsed.program)
        .with_stdlib_source(true)
        .resolve();
    assert!(
        result
            .errors
            .iter()
            .all(|e| e.kind != ResolveErrorKind::CompilerBuiltinReserved),
        "stdlib-source flag should suppress CompilerBuiltinReserved on impl items, got: {:?}",
        result.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_compiler_builtin_per_method_origin_bypasses_gate_on_impl_method() {
    // The per-item `stdlib_origin` exemption — the bake step for slice 6.3
    // sets this on every method inside baked impl blocks. Confirms the
    // resolver picks it up: even without the session-wide flag, a method
    // tagged `stdlib_origin = true` is allowed to carry `#[compiler_builtin]`,
    // while an untagged sibling in the same block is still rejected.
    use karac::ast::{ImplItem, Item};
    let parsed = parse(
        "struct Foo { }\n\
         impl Foo {\n\
             #[compiler_builtin]\n\
             fn baked(self) -> i64 { 0 }\n\
             #[compiler_builtin]\n\
             fn user_attempt(self) -> i64 { 0 }\n\
         }",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let mut program = parsed.program;
    let imp = program
        .items
        .iter_mut()
        .find_map(|i| {
            if let Item::ImplBlock(b) = i {
                Some(b)
            } else {
                None
            }
        })
        .expect("program has an impl block");
    let baked_method = imp
        .items
        .iter_mut()
        .find_map(|i| match i {
            ImplItem::Method(m) if m.name == "baked" => Some(m),
            _ => None,
        })
        .expect("impl block has `baked` method");
    baked_method.stdlib_origin = true;

    let result = Resolver::new(&program).resolve();
    let rejections: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == ResolveErrorKind::CompilerBuiltinReserved)
        .collect();
    assert_eq!(
        rejections.len(),
        1,
        "expected exactly one rejection (the untagged sibling method), got: {:?}",
        rejections.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// ── Slice / array patterns (phase 5.2 sub-item 1) ─────────────────────────

#[test]
fn test_slice_pattern_binds_prefix_suffix_and_rest_names() {
    // The resolver should define `a`, `b`, `mid`, `c` from the slice pattern
    // so that the arm body can reference them without "undefined name" errors.
    // (The typechecker stub still rejects the program semantically — covered
    // in tests/typechecker.rs — but resolver scope binding is independent.)
    let parsed = parse(
        "fn f(xs: Vec[i64]) -> i64 { \
         match xs { \
         [a, b, ..mid, c] => a + b + c, \
         _ => 0, \
         } \
         }",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    let result = resolve(&parsed.program);
    assert!(
        result.errors.is_empty(),
        "expected no resolve errors, got: {:?}",
        result.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_slice_pattern_unbound_rest_does_not_introduce_name() {
    // `..` (ignored rest) does not introduce a binding — referencing `rest`
    // in the body should produce an unresolved-name error.
    let errs = resolve_errors(
        "fn f(xs: Vec[i64]) -> i64 { \
         match xs { \
         [.., last] => rest + last, \
         _ => 0, \
         } \
         }",
    );
    assert!(
        errs.iter().any(|e| e.message.contains("rest")),
        "expected unresolved 'rest' diagnostic, got: {:?}",
        errs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}
