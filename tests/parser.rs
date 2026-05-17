// tests/parser.rs

//! Parser integration tests for the Kāra compiler.

use karac::ast::*;
use karac::parse;

fn parse_ok(source: &str) -> Program {
    let result = parse(source);
    if !result.errors.is_empty() {
        for e in &result.errors {
            eprintln!("  Parse error: {}", e);
        }
        panic!("Expected no parse errors for:\n{}", source);
    }
    result.program
}

fn parse_with_errors(source: &str) -> (Program, Vec<karac::parser::ParseError>) {
    let result = parse(source);
    (result.program, result.errors)
}

/// Extract methods from trait items
fn trait_methods(t: &TraitDef) -> Vec<&TraitMethod> {
    t.items
        .iter()
        .filter_map(|item| match item {
            TraitItem::Method(m) => Some(m.as_ref()),
            _ => None,
        })
        .collect()
}

/// Extract methods from impl items
fn impl_methods(imp: &ImplBlock) -> Vec<&Function> {
    imp.items
        .iter()
        .filter_map(|item| match item {
            ImplItem::Method(m) => Some(m.as_ref()),
            _ => None,
        })
        .collect()
}

/// Extract type_name and bounds from a TypeBound where constraint
fn where_type_bound(c: &WhereConstraint) -> (&str, &[TraitBound]) {
    match c {
        WhereConstraint::TypeBound {
            type_name, bounds, ..
        } => (type_name, bounds),
        _ => panic!("Expected TypeBound where constraint"),
    }
}

// ── 2.1: Expressions and Statements ─────────────────────────────

#[test]
fn test_integer_literal() {
    let prog = parse_ok("fn main() { let x = 42; }");
    assert_eq!(prog.items.len(), 1);
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.name, "main");
        assert_eq!(f.body.stmts.len(), 1);
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_float_literal() {
    let prog = parse_ok("fn main() { let x = 1.5; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(value.kind, ExprKind::Float(n, _) if (n - 1.5).abs() < f64::EPSILON));
        }
    }
}

#[test]
fn test_string_literal() {
    let prog = parse_ok(r#"fn main() { let s = "hello"; }"#);
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(&value.kind, ExprKind::StringLit(s) if s == "hello"));
        }
    }
}

#[test]
fn test_boolean_literals() {
    let prog = parse_ok("fn main() { let a = true; let b = false; }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.body.stmts.len(), 2);
    }
}

#[test]
fn test_arithmetic_operators() {
    let prog = parse_ok("fn main() { let x = 1 + 2 * 3; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            // Should be Add(1, Mul(2, 3)) due to precedence
            if let ExprKind::Binary { op, left, right } = &value.kind {
                assert_eq!(*op, BinOp::Add);
                assert!(matches!(left.kind, ExprKind::Integer(1, _)));
                if let ExprKind::Binary { op: inner_op, .. } = &right.kind {
                    assert_eq!(*inner_op, BinOp::Mul);
                } else {
                    panic!("Expected Mul");
                }
            } else {
                panic!("Expected Binary");
            }
        }
    }
}

#[test]
fn test_comparison_operators() {
    parse_ok("fn main() { let x = a == b; let y = c != d; let z = e < f; }");
}

#[test]
fn test_logical_operators() {
    let prog = parse_ok("fn main() { let x = a and b or c; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            // or has lower precedence than and, so: Or(And(a, b), c)
            assert!(matches!(
                &value.kind,
                ExprKind::Binary { op: BinOp::Or, .. }
            ));
        }
    }
}

#[test]
fn test_logical_not_prefix_precedence() {
    // `not` binds tighter than comparison: `not x == y` parses as `(not x) == y`.
    let prog = parse_ok("fn main() { let r = not x == y; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(
                &value.kind,
                ExprKind::Binary { op: BinOp::Eq, left, .. }
                    if matches!(&left.kind, ExprKind::Unary { op: UnaryOp::Not, .. })
            ));
        }
    }
}

#[test]
fn test_logical_symbol_form_rejected() {
    // `&&` is not accepted; the parser must point at the keyword form.
    let (_, errors) = parse_with_errors("fn main() { let x = a && b; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("`&&`") && e.message.contains("`and`")),
        "expected migration error for `&&`, got: {:?}",
        errors
    );
}

#[test]
fn test_or_symbol_form_rejected() {
    let (_, errors) = parse_with_errors("fn main() { let x = a || b; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("`||`") && e.message.contains("`or`")),
        "expected migration error for `||`, got: {:?}",
        errors
    );
}

#[test]
fn test_bang_symbol_form_rejected() {
    let (_, errors) = parse_with_errors("fn main() { let x = !a; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("`!`") && e.message.contains("`not`")),
        "expected migration error for `!`, got: {:?}",
        errors
    );
}

#[test]
fn test_closure_no_capture_keyword_default_owned() {
    // Bare `|x| body` is the new default — captures are owned. No prefix needed.
    parse_ok("fn main() { let f = |x| x + 1; }");
}

#[test]
fn test_closure_move_keyword_rejected() {
    let (_, errors) = parse_with_errors("fn main() { let f = move |x| x + 1; }");
    assert!(
        errors.iter().any(|e| e.message.contains("`move`")),
        "expected error for `move |...|`, got: {:?}",
        errors
    );
}

#[test]
fn test_closure_own_capture_mode_prefix() {
    // `own |x| body` records `capture_mode = Some(Own)` (Rule 2½ — explicit
    // capture-by-value prefix).
    let prog = parse_ok("fn main() { let f = own |x| x + 1; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Closure {
                capture_mode,
                params,
                ..
            } = &value.kind
            {
                assert_eq!(*capture_mode, Some(CaptureMode::Own));
                assert_eq!(params.len(), 1);
                return;
            }
        }
    }
    panic!("expected closure expression");
}

#[test]
fn test_closure_ref_capture_mode_prefix() {
    // `ref |x| body` records `capture_mode = Some(Ref)`.
    let prog = parse_ok("fn main() { let f = ref |x| x + 1; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Closure {
                capture_mode,
                params,
                ..
            } = &value.kind
            {
                assert_eq!(*capture_mode, Some(CaptureMode::Ref));
                assert_eq!(params.len(), 1);
                return;
            }
        }
    }
    panic!("expected closure expression");
}

#[test]
fn test_closure_mut_ref_capture_mode_prefix() {
    // `mut ref |x| body` records `capture_mode = Some(MutRef)`.
    let prog = parse_ok("fn main() { let f = mut ref |x| x + 1; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Closure {
                capture_mode,
                params,
                ..
            } = &value.kind
            {
                assert_eq!(*capture_mode, Some(CaptureMode::MutRef));
                assert_eq!(params.len(), 1);
                return;
            }
        }
    }
    panic!("expected closure expression");
}

#[test]
fn test_closure_no_prefix_capture_mode_none() {
    // Regression: bare `|x| body` continues to parse with `capture_mode = None`.
    let prog = parse_ok("fn main() { let f = |x| x + 1; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Closure { capture_mode, .. } = &value.kind {
                assert_eq!(*capture_mode, None);
                return;
            }
        }
    }
    panic!("expected closure expression");
}

#[test]
fn test_closure_ref_prefix_with_no_params() {
    // `ref || body` — empty parameter list, `||` token form.
    let prog = parse_ok("fn main() { let f = ref || 42; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Closure {
                capture_mode,
                params,
                ..
            } = &value.kind
            {
                assert_eq!(*capture_mode, Some(CaptureMode::Ref));
                assert!(params.is_empty());
                return;
            }
        }
    }
    panic!("expected closure expression");
}

#[test]
fn test_closure_mut_ref_prefix_does_not_consume_in_type_position() {
    // Regression: `mut ref T` in parameter type position is unaffected by the
    // closure capture-mode prefix lookahead. The lookahead only fires when a
    // `|` / `||` token follows.
    parse_ok("fn add(x: mut ref i32) { *x = *x + 1; }");
}

#[test]
fn test_closure_param_tuple_pattern() {
    // Closure parameters accept irrefutable patterns — same as fn params.
    let prog = parse_ok("fn main() { let f = |(a, b)| a + b; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Closure { params, .. } = &value.kind {
                assert_eq!(params.len(), 1);
                assert!(
                    matches!(&params[0].pattern.kind, PatternKind::Tuple(elems) if elems.len() == 2),
                    "expected tuple pattern, got: {:?}",
                    params[0].pattern.kind
                );
            } else {
                panic!("expected closure expression");
            }
        }
    }
}

#[test]
fn test_closure_param_struct_pattern() {
    let prog = parse_ok("fn main() { let f = |Point { x, y }| x * y; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Closure { params, .. } = &value.kind {
                assert_eq!(params.len(), 1);
                assert!(matches!(
                    &params[0].pattern.kind,
                    PatternKind::Struct { .. }
                ));
            }
        }
    }
}

#[test]
fn test_closure_param_wildcard_pattern() {
    parse_ok("fn main() { let f = |_| 42; }");
}

#[test]
fn test_let_binding() {
    let prog = parse_ok("fn main() { let x = 5; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let {
            is_mut,
            pattern,
            ty,
            value,
        } = &f.body.stmts[0].kind
        {
            assert!(!is_mut);
            assert!(matches!(&pattern.kind, PatternKind::Binding(n) if n == "x"));
            assert!(ty.is_none());
            assert!(matches!(value.kind, ExprKind::Integer(5, _)));
        }
    }
}

#[test]
fn test_let_mut_binding() {
    let prog = parse_ok("fn main() { let mut count = 0; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { is_mut, .. } = &f.body.stmts[0].kind {
            assert!(is_mut);
        }
    }
}

#[test]
fn test_let_with_type_annotation() {
    let prog = parse_ok("fn main() { let name: String = \"Alice\"; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { ty, .. } = &f.body.stmts[0].kind {
            assert!(ty.is_some());
        }
    }
}

#[test]
fn test_assignment() {
    let prog = parse_ok("fn main() { let mut x = 0; x = 5; }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.body.stmts.len(), 2);
        assert!(matches!(&f.body.stmts[1].kind, StmtKind::Assign { .. }));
    }
}

#[test]
fn test_block_expression() {
    let prog = parse_ok("fn main() { let x = { let a = 1; a + 2 }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Block(block) = &value.kind {
                assert_eq!(block.stmts.len(), 1);
                assert!(block.final_expr.is_some());
            } else {
                panic!("Expected block expression");
            }
        }
    }
}

#[test]
fn test_if_else() {
    let prog = parse_ok("fn main() { let x = if a > b { a } else { b }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::If { else_branch, .. } = &value.kind {
                assert!(else_branch.is_some());
            } else {
                panic!("Expected if expression");
            }
        }
    }
}

#[test]
fn test_if_else_if() {
    parse_ok("fn main() { if a { x(); } else if b { y(); } else { z(); } }");
}

#[test]
fn test_if_let() {
    let prog = parse_ok("fn main() { if let Some(x) = maybe_val { x } else { 0 } }");
    if let Item::Function(f) = &prog.items[0] {
        let expr = f.body.final_expr.as_ref().expect("expected final expr");
        if let ExprKind::IfLet {
            pattern,
            else_branch,
            ..
        } = &expr.kind
        {
            assert!(matches!(&pattern.kind, PatternKind::TupleVariant { .. }));
            assert!(else_branch.is_some());
        } else {
            panic!("Expected if let expression");
        }
    }
}

#[test]
fn test_if_let_no_else() {
    parse_ok("fn main() { if let Some(x) = opt { use_x(x); } }");
}

#[test]
fn test_let_else() {
    let prog = parse_ok("fn main() { let Some(x) = maybe_val else { return; } }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::LetElse {
            pattern,
            else_block,
            ..
        } = &f.body.stmts[0].kind
        {
            assert!(matches!(&pattern.kind, PatternKind::TupleVariant { .. }));
            assert!(!else_block.stmts.is_empty() || else_block.final_expr.is_some());
        } else {
            panic!("Expected let...else statement");
        }
    }
}

#[test]
fn test_while_loop() {
    let prog = parse_ok("fn main() { while count < 10 { count = count + 1; } }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.body.stmts.len(), 1);
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            assert!(matches!(&expr.kind, ExprKind::While { .. }));
        }
    }
}

#[test]
fn test_for_loop() {
    let prog = parse_ok("fn main() { for item in items { process(item); } }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::For { pattern, .. } = &expr.kind {
                assert!(matches!(&pattern.kind, PatternKind::Binding(name) if name == "item"));
            }
        }
    }
}

#[test]
fn test_for_loop_tuple_destructure() {
    let prog = parse_ok("fn main() { for (k, v) in pairs { process(k, v); } }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::For { pattern, .. } = &expr.kind {
                if let PatternKind::Tuple(pats) = &pattern.kind {
                    assert_eq!(pats.len(), 2);
                    assert!(matches!(&pats[0].kind, PatternKind::Binding(name) if name == "k"));
                    assert!(matches!(&pats[1].kind, PatternKind::Binding(name) if name == "v"));
                } else {
                    panic!("expected tuple pattern");
                }
            }
        }
    }
}

#[test]
fn test_for_loop_wildcard() {
    parse_ok("fn main() { for _ in 0..10 { do_something(); } }");
}

#[test]
fn test_loop_break_continue() {
    parse_ok("fn main() { loop { if done() { break; } continue; } }");
}

#[test]
fn test_return() {
    parse_ok("fn main() { return; }");
    parse_ok("fn add(a: i64, b: i64) -> i64 { return a + b; }");
}

#[test]
fn test_implicit_return() {
    let prog = parse_ok("fn add(a: i64, b: i64) -> i64 { a + b }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.body.final_expr.is_some());
    }
}

#[test]
fn test_question_mark_operator() {
    parse_ok("fn main() { let x = foo()?; }");
}

#[test]
fn test_unary_operators() {
    parse_ok("fn main() { let x = -a; let y = not b; let z = ~c; }");
}

#[test]
fn test_deref_expression() {
    parse_ok("fn main() { let x = *r; }");
}

#[test]
fn test_deref_assignment() {
    parse_ok("fn main() { *r = 42; }");
}

#[test]
fn test_deref_nested() {
    parse_ok("fn main() { let x = *r + 1; }");
}

#[test]
fn test_function_call() {
    parse_ok("fn main() { foo(); bar(1, 2, 3); }");
}

#[test]
fn test_method_call() {
    parse_ok("fn main() { items.len(); items.map(|x| x + 1); }");
}

#[test]
fn test_map_entry_chain_parses() {
    // m.entry(k) is just a method call; the .or_insert / .and_modify chain
    // composes through standard method-call grammar. No new syntax.
    parse_ok("fn main() { let m = Map.new(); m.entry(k).or_insert(0); }");
    parse_ok("fn main() { let m = Map.new(); m.entry(k).or_insert_with(|| 0); }");
    parse_ok("fn main() { let m = Map.new(); m.entry(k).and_modify(|v| v + 1).or_insert(0); }");
    parse_ok("fn main() { let m = Map.new(); m.entry(k).or_insert_with(Vec.new).push(row); }");
}

#[test]
fn test_field_access() {
    parse_ok("fn main() { let x = point.x; let y = self.count; }");
}

#[test]
fn test_index_access() {
    parse_ok("fn main() { let x = items[0]; let y = map[key]; }");
}

#[test]
fn test_tuple_expression() {
    parse_ok("fn main() { let t = (1, 2, 3); }");
}

#[test]
fn test_closure_expression() {
    parse_ok("fn main() { let f = |x| x + 1; }");
    parse_ok("fn main() { let f = |x, y| { x * y }; }");
    parse_ok("fn main() { let f = || 42; }");
}

#[test]
fn test_closure_with_types() {
    parse_ok("fn main() { let f = |x: i64| x + 1; }");
}

#[test]
fn test_range_expression() {
    parse_ok("fn main() { for i in 0..10 { x(); } }");
}

#[test]
fn test_defer() {
    let prog = parse_ok("fn main() { defer { cleanup(); } }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(matches!(&f.body.stmts[0].kind, StmtKind::Defer { .. }));
    }
}

#[test]
fn test_errdefer() {
    let prog = parse_ok("fn main() { errdefer(err) { log(err); } }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::ErrDefer { binding, .. } = &f.body.stmts[0].kind {
            assert_eq!(binding.as_deref(), Some("err"));
        } else {
            panic!("Expected errdefer");
        }
    }
}

#[test]
fn test_errdefer_no_binding() {
    parse_ok("fn main() { errdefer { rollback(); } }");
}

#[test]
fn test_effect_variable_in_generics() {
    let prog =
        parse_ok("fn map[T, U, with E](f: Fn(T) -> U, items: Vec[T]) -> Vec[U] with E { todo() }");
    if let Item::Function(f) = &prog.items[0] {
        let gp = f.generic_params.as_ref().unwrap();
        assert_eq!(gp.params.len(), 2);
        assert_eq!(gp.effect_params, vec!["E"]);
        // The effect clause must produce Variable("E"), not Group("E").
        let effects = f.effects.as_ref().expect("should have effect clause");
        assert_eq!(effects.items.len(), 1);
        assert!(
            matches!(&effects.items[0], EffectItem::Variable(v) if v == "E"),
            "expected Variable(\"E\"), got {:?}",
            &effects.items[0]
        );
    }
}

#[test]
fn test_multiple_effect_variables_comma_separated() {
    // design.md line 4858 spelling: `[T, U, V, with E1, E2]` — once `with`
    // appears, every subsequent comma-separated identifier is an effect
    // variable. Both `[with E, F]` and `[with E, with F]` must work.
    let prog = parse_ok(
        "fn zip_with[T, U, V, with E1, E2](\
            f: Fn(T) -> V with E1,\
            g: Fn(U) -> V with E2,\
        ) -> Vec[V] with E1 E2 { todo() }",
    );
    if let Item::Function(f) = &prog.items[0] {
        let gp = f.generic_params.as_ref().unwrap();
        assert_eq!(gp.params.len(), 3, "expected 3 type params (T, U, V)");
        assert_eq!(gp.effect_params, vec!["E1", "E2"]);
        let effects = f.effects.as_ref().expect("should have effect clause");
        assert_eq!(effects.items.len(), 2);
        for (i, name) in ["E1", "E2"].iter().enumerate() {
            assert!(
                matches!(&effects.items[i], EffectItem::Variable(v) if v == name),
                "expected Variable({}), got {:?}",
                name,
                &effects.items[i]
            );
        }
    }
}

#[test]
fn test_multiple_effect_variables_each_with_keyword() {
    // Equivalent spelling: each effect variable gets its own `with`.
    let prog = parse_ok("fn pipe[with E, with F](a: Fn() with E, b: Fn() with F) with E F {}");
    if let Item::Function(f) = &prog.items[0] {
        let gp = f.generic_params.as_ref().unwrap();
        assert!(gp.params.is_empty());
        assert_eq!(gp.effect_params, vec!["E", "F"]);
    }
}

#[test]
fn test_struct_destructure_shorthand() {
    let prog = parse_ok("fn main() { match p { Point { x, y } => x } }");
    if let Item::Function(f) = &prog.items[0] {
        let expr = f.body.final_expr.as_ref().unwrap();
        if let ExprKind::Match { arms, .. } = &expr.kind {
            if let PatternKind::Struct { fields, .. } = &arms[0].pattern.kind {
                assert_eq!(fields.len(), 2);
                assert!(fields[0].pattern.is_none()); // shorthand: x = x
                assert!(fields[1].pattern.is_none()); // shorthand: y = y
            }
        }
    }
}

#[test]
fn test_or_pattern() {
    let prog = parse_ok("fn main() { match x { 1 | 2 | 3 => true, _ => false } }");
    if let Item::Function(f) = &prog.items[0] {
        let expr = f.body.final_expr.as_ref().unwrap();
        if let ExprKind::Match { arms, .. } = &expr.kind {
            assert!(matches!(&arms[0].pattern.kind, PatternKind::Or(alts) if alts.len() == 3));
        }
    }
}

#[test]
fn test_pipe_operator() {
    let prog = parse_ok("fn main() { let x = data |> transform |> output; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            // Should be: (data |> transform) |> output (left-associative)
            assert!(matches!(&value.kind, ExprKind::Pipe { .. }));
        } else {
            panic!("Expected let with pipe");
        }
    }
}

#[test]
fn test_struct_literal() {
    let prog = parse_ok("fn main() { let p = Point { x: 1, y: 2 }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::StructLiteral { path, fields, .. } = &value.kind {
                assert_eq!(path, &vec!["Point".to_string()]);
                assert_eq!(fields.len(), 2);
            } else {
                panic!("Expected struct literal");
            }
        }
    }
}

#[test]
fn test_cast_expression() {
    parse_ok("fn main() { let x = count as f64; }");
}

#[test]
fn test_unsafe_block() {
    parse_ok("fn main() { unsafe { let x = 5; } }");
}

// ── 2.2: Functions and Types ─────────────────────────────────────

#[test]
fn test_function_no_params() {
    let prog = parse_ok("fn greet() { }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.name, "greet");
        assert!(f.params.is_empty());
        assert!(f.return_type.is_none());
    }
}

#[test]
fn test_function_with_params_and_return() {
    let prog = parse_ok("fn add(a: i64, b: i64) -> i64 { a + b }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.name, "add");
        assert_eq!(f.params.len(), 2);
        assert!(f.return_type.is_some());
    }
}

#[test]
fn test_pub_function() {
    let prog = parse_ok("pub fn public_fn() { }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.is_pub);
    }
}

#[test]
fn test_struct_definition() {
    let prog = parse_ok("struct Point { x: f64, y: f64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.name, "Point");
        assert_eq!(s.fields.len(), 2);
        assert_eq!(s.fields[0].name, "x");
        assert_eq!(s.fields[1].name, "y");
    } else {
        panic!("Expected struct");
    }
}

#[test]
fn test_struct_field_mutability() {
    let prog =
        parse_ok("struct User { pub name: String, pub mut age: i64, mut secret: String, id: i64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.fields.len(), 4);
        // pub name
        assert!(s.fields[0].is_pub);
        assert!(!s.fields[0].is_mut);
        // pub mut age
        assert!(s.fields[1].is_pub);
        assert!(s.fields[1].is_mut);
        // mut secret
        assert!(!s.fields[2].is_pub);
        assert!(s.fields[2].is_mut);
        // id (neither)
        assert!(!s.fields[3].is_pub);
        assert!(!s.fields[3].is_mut);
    }
}

#[test]
fn test_struct_with_generics() {
    let prog = parse_ok("struct Container[T] { value: T }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert!(s.generic_params.is_some());
        assert_eq!(s.generic_params.as_ref().unwrap().params.len(), 1);
    }
}

#[test]
fn test_enum_definition() {
    let prog = parse_ok(
        r#"
        enum Shape {
            Circle { radius: f64 },
            Rectangle { width: f64, height: f64 },
            Point,
        }
    "#,
    );
    if let Item::EnumDef(e) = &prog.items[0] {
        assert_eq!(e.name, "Shape");
        assert_eq!(e.variants.len(), 3);
        assert!(matches!(&e.variants[0].kind, VariantKind::Struct(_)));
        assert!(matches!(&e.variants[2].kind, VariantKind::Unit));
    }
}

#[test]
fn test_enum_tuple_variant() {
    let prog = parse_ok("enum Option[T] { Some(T), None }");
    if let Item::EnumDef(e) = &prog.items[0] {
        assert_eq!(e.variants.len(), 2);
        assert!(matches!(&e.variants[0].kind, VariantKind::Tuple(_)));
        assert!(matches!(&e.variants[1].kind, VariantKind::Unit));
    }
}

#[test]
fn test_trait_definition() {
    let prog = parse_ok(
        r#"
        trait Processor {
            fn process(self, data: Data) -> Result[Output, Error] with _;
        }
    "#,
    );
    if let Item::TraitDef(t) = &prog.items[0] {
        assert_eq!(t.name, "Processor");
        let methods = trait_methods(t);
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].self_param, Some(SelfParam::Owned));
    }
}

#[test]
fn test_trait_pure_method() {
    let prog = parse_ok(
        r#"
        trait Comparator {
            fn compare(self, a: i64, b: i64) -> i64;
        }
    "#,
    );
    if let Item::TraitDef(t) = &prog.items[0] {
        let methods = trait_methods(t);
        assert!(methods[0].effects.is_none());
        assert!(methods[0].body.is_none());
    }
}

// ── Trait Aliases (parser/AST/resolver — v1 stub) ──────────────────
//
// `trait NAME = bound1 + bound2 + ...;` per design.md § Trait Aliases
// (v60 item 40). v1 parser/AST/resolver work — typechecker emits
// `E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET` at every use site.

#[test]
fn test_trait_alias_basic() {
    let prog = parse_ok("trait Numeric = Copy + Clone;");
    let alias = match &prog.items[0] {
        Item::TraitAlias(t) => t,
        other => panic!("expected TraitAlias, got {other:?}"),
    };
    assert_eq!(alias.name, "Numeric");
    assert_eq!(alias.bounds.len(), 2);
    assert_eq!(alias.bounds[0].path, vec!["Copy".to_string()]);
    assert_eq!(alias.bounds[1].path, vec!["Clone".to_string()]);
    assert!(alias.where_clause.is_none());
    assert!(!alias.is_pub);
}

#[test]
fn test_trait_alias_pub() {
    let prog = parse_ok("pub trait Ord2 = PartialOrd + Eq;");
    if let Item::TraitAlias(t) = &prog.items[0] {
        assert!(t.is_pub);
        assert_eq!(t.name, "Ord2");
    } else {
        panic!("expected TraitAlias");
    }
}

#[test]
fn test_trait_alias_with_generics() {
    let prog = parse_ok("trait IteratorOver[T] = Iterator;");
    if let Item::TraitAlias(t) = &prog.items[0] {
        assert!(t.generic_params.is_some());
        let gps = t.generic_params.as_ref().unwrap();
        assert_eq!(gps.params.len(), 1);
        assert_eq!(gps.params[0].name, "T");
    } else {
        panic!("expected TraitAlias");
    }
}

#[test]
fn test_trait_alias_with_where_clause() {
    let prog = parse_ok("trait OrderedFloat[T] = Ord where T: Copy;");
    if let Item::TraitAlias(t) = &prog.items[0] {
        assert!(t.where_clause.is_some());
        assert_eq!(t.bounds.len(), 1);
        assert_eq!(t.bounds[0].path, vec!["Ord".to_string()]);
    } else {
        panic!("expected TraitAlias");
    }
}

#[test]
fn test_trait_alias_empty_bound_list_rejected() {
    let (_, errors) = parse_with_errors("trait Foo = ;");
    assert!(
        !errors.is_empty(),
        "expected error for empty trait-alias bound list"
    );
    assert!(
        errors[0].message.contains("at least one trait bound"),
        "got: {:?}",
        errors[0].message
    );
}

#[test]
fn test_trait_alias_does_not_break_regular_trait() {
    // Sanity: regular `trait Foo { ... }` form still parses correctly.
    parse_ok("trait Foo { fn bar(self) -> i64; }");
    parse_ok("trait Foo: Bar { fn baz(self); }");
}

// ── Try blocks (parser/AST + v1 stub) ───────────────────────────────
//
// `try { ... }` per design.md § Error Handling > Try Blocks (v60 item
// 42). v1 parses the form; the typechecker pipeline (?-retargeting +
// error-type unification + From-chain coercion) lands in P1.

#[test]
fn test_try_block_basic() {
    let prog = parse_ok("fn main() { let r = try { 42 }; }");
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    let stmt = &f.body.stmts[0];
    let value = match &stmt.kind {
        StmtKind::Let { value, .. } => value,
        _ => panic!("expected let"),
    };
    assert!(
        matches!(value.kind, ExprKind::Try(_)),
        "expected ExprKind::Try, got {:?}",
        value.kind
    );
}

#[test]
fn test_try_block_with_inner_question_mark_parses() {
    // The inner `?` is parsed by ordinary expression machinery; the
    // parser doesn't need to know about retargeting yet.
    parse_ok(
        "fn parse(s: String) -> Result[i64, IoError] { Ok(0) } \
         fn main() { let r = try { let n = parse(\"1\")?; n }; }",
    );
}

#[test]
fn test_try_block_nested() {
    parse_ok("fn main() { let r = try { try { 1 } }; }");
}

#[test]
fn test_try_block_no_longer_reserved_keyword_error() {
    // `try` previously emitted the reserved-future-use-keyword error;
    // it now lexes as Token::Try and parses as a try block.
    parse_ok("fn main() { let _ = try { 0 }; }");
}

// ── Marker traits ───────────────────────────────────────────────────
//
// `marker trait NAME;` per design.md § Marker Traits (v60 item 55).
// Body must be empty; impl bodies must also be empty (typechecker).

#[test]
fn test_marker_trait_basic() {
    let prog = parse_ok("marker trait Pod;");
    let m = match &prog.items[0] {
        Item::MarkerTrait(t) => t,
        other => panic!("expected MarkerTrait, got {other:?}"),
    };
    assert_eq!(m.name, "Pod");
    assert!(m.supertraits.is_empty());
    assert!(!m.body_brace);
}

#[test]
fn test_marker_trait_pub() {
    let prog = parse_ok("pub marker trait Sealed;");
    if let Item::MarkerTrait(t) = &prog.items[0] {
        assert!(t.is_pub);
    } else {
        panic!("expected MarkerTrait");
    }
}

#[test]
fn test_marker_trait_with_supertrait() {
    let prog = parse_ok("marker trait Concurrent: Sized;");
    if let Item::MarkerTrait(t) = &prog.items[0] {
        assert_eq!(t.supertraits.len(), 1);
        assert_eq!(t.supertraits[0].path, vec!["Sized".to_string()]);
    } else {
        panic!("expected MarkerTrait");
    }
}

#[test]
fn test_marker_trait_with_generics() {
    let prog = parse_ok("marker trait Storeable[T];");
    if let Item::MarkerTrait(t) = &prog.items[0] {
        assert!(t.generic_params.is_some());
    } else {
        panic!("expected MarkerTrait");
    }
}

#[test]
fn test_marker_trait_empty_brace_form() {
    let prog = parse_ok("marker trait Pod { }");
    if let Item::MarkerTrait(t) = &prog.items[0] {
        assert!(t.body_brace);
    } else {
        panic!("expected MarkerTrait");
    }
}

#[test]
fn test_marker_trait_with_method_in_body_rejected() {
    let (_, errors) = parse_with_errors("marker trait Pod { fn bar(self); }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_MARKER_TRAIT_HAS_METHOD")),
        "expected E_MARKER_TRAIT_HAS_METHOD, got: {errors:?}"
    );
}

#[test]
fn test_marker_trait_with_assoc_type_rejected() {
    let (_, errors) = parse_with_errors("marker trait Pod { type Item; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_MARKER_TRAIT_HAS_ITEM")),
        "expected E_MARKER_TRAIT_HAS_ITEM, got: {errors:?}"
    );
}

#[test]
fn test_marker_trait_does_not_break_regular_trait() {
    parse_ok("trait Foo { fn bar(self); }");
    parse_ok("trait Foo: Bar { fn baz(self); }");
}

#[test]
fn test_trait_default_method() {
    let prog = parse_ok(
        r#"
        trait Counter {
            fn count(self) -> i64 {
                0
            }
        }
    "#,
    );
    if let Item::TraitDef(t) = &prog.items[0] {
        let methods = trait_methods(t);
        assert_eq!(methods.len(), 1);
        assert!(methods[0].body.is_some());
    }
}

#[test]
fn test_trait_mixed_methods() {
    let prog = parse_ok(
        r#"
        trait Iterator {
            fn next(self) -> i64;
            fn count(self) -> i64 {
                0
            }
        }
    "#,
    );
    if let Item::TraitDef(t) = &prog.items[0] {
        let methods = trait_methods(t);
        assert_eq!(methods.len(), 2);
        assert!(methods[0].body.is_none()); // required
        assert!(methods[1].body.is_some()); // default
    }
}

#[test]
fn test_impl_block() {
    let prog = parse_ok(
        r#"
        impl WordCount {
            fn total_ratio(self) -> f64 {
                self.unique as f64
            }
        }
    "#,
    );
    if let Item::ImplBlock(imp) = &prog.items[0] {
        assert!(imp.trait_name.is_none());
        assert_eq!(impl_methods(imp).len(), 1);
    }
}

#[test]
fn test_impl_trait_for_type() {
    let prog = parse_ok(
        r#"
        impl Processor for LocalProcessor {
            fn process(self, data: Data) -> Result[Output, Error] {
                compute(data)
            }
        }
    "#,
    );
    if let Item::ImplBlock(imp) = &prog.items[0] {
        assert!(imp.trait_name.is_some());
        assert_eq!(imp.trait_name.as_ref().unwrap().segments, vec!["Processor"]);
    }
}

#[test]
fn test_generic_function() {
    let prog = parse_ok("fn sort[T](list: Vec[T]) -> Vec[T] { list }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.generic_params.is_some());
        assert_eq!(f.generic_params.as_ref().unwrap().params[0].name, "T");
    }
}

#[test]
fn test_generic_with_bounds() {
    let prog = parse_ok("fn run[T: Processor](p: T) -> i64 { 0 }");
    if let Item::Function(f) = &prog.items[0] {
        let gp = f.generic_params.as_ref().unwrap();
        assert_eq!(gp.params[0].bounds.len(), 1);
        assert_eq!(gp.params[0].bounds[0].path, vec!["Processor"]);
    }
}

#[test]
fn test_self_param_variants() {
    parse_ok("impl Foo { fn a(self) { } }");
    parse_ok("impl Foo { fn b(ref self) { } }");
    parse_ok("impl Foo { fn c(mut ref self) { } }");
}

// ── 2.3: Effects Syntax ──────────────────────────────────────────

#[test]
fn test_effect_resource_decl() {
    let prog = parse_ok("effect resource UserDB: DatabaseProvider;");
    if let Item::EffectResource(e) = &prog.items[0] {
        assert_eq!(e.name, "UserDB");
        assert_eq!(e.provider_trait, Some("DatabaseProvider".to_string()));
    }
}

#[test]
fn test_effect_resource_no_provider() {
    let prog = parse_ok("effect resource UserDB;");
    if let Item::EffectResource(e) = &prog.items[0] {
        assert_eq!(e.name, "UserDB");
        assert!(e.provider_trait.is_none());
    }
}

#[test]
fn test_effect_group_decl() {
    let prog =
        parse_ok("effect group validation = reads(UserDb, InventoryDb) + sends(FraudService);");
    if let Item::EffectGroup(g) = &prog.items[0] {
        assert_eq!(g.name, "validation");
        assert_eq!(g.body.len(), 2); // reads verb and sends verb
    }
}

#[test]
fn test_effect_group_composition() {
    let prog = parse_ok("effect group order_processing = validation + fulfillment;");
    if let Item::EffectGroup(g) = &prog.items[0] {
        assert_eq!(g.body.len(), 2);
        assert!(matches!(&g.body[0], EffectGroupTerm::GroupRef(n) if n == "validation"));
        assert!(matches!(&g.body[1], EffectGroupTerm::GroupRef(n) if n == "fulfillment"));
    }
}

#[test]
fn test_transparent_effect() {
    let prog = parse_ok("transparent effect verb traces;");
    if let Item::EffectVerbDecl(t) = &prog.items[0] {
        assert_eq!(t.verb_name, "traces");
        assert!(t.is_transparent);
        assert!(!t.is_pub);
    }
}

#[test]
fn test_pub_transparent_effect_verb() {
    let prog = parse_ok("pub transparent effect verb traces;");
    if let Item::EffectVerbDecl(t) = &prog.items[0] {
        assert_eq!(t.verb_name, "traces");
        assert!(t.is_transparent);
        assert!(t.is_pub);
    } else {
        panic!("Expected EffectVerbDecl");
    }
}

#[test]
fn test_effect_verb_decl_non_transparent() {
    let prog = parse_ok("effect verb logs;");
    if let Item::EffectVerbDecl(t) = &prog.items[0] {
        assert_eq!(t.verb_name, "logs");
        assert!(!t.is_transparent);
        assert!(!t.is_pub);
    } else {
        panic!("Expected EffectVerbDecl");
    }
}

#[test]
fn test_pub_effect_verb_decl() {
    let prog = parse_ok("pub effect verb logs;");
    if let Item::EffectVerbDecl(t) = &prog.items[0] {
        assert_eq!(t.verb_name, "logs");
        assert!(!t.is_transparent);
        assert!(t.is_pub);
    } else {
        panic!("Expected EffectVerbDecl");
    }
}

#[test]
fn test_stable_effect_group() {
    let prog = parse_ok("stable effect group read_only = reads(Db);");
    if let Item::EffectGroup(g) = &prog.items[0] {
        assert_eq!(g.name, "read_only");
        assert!(g.is_stable);
        assert!(!g.is_pub);
    } else {
        panic!("Expected EffectGroup");
    }
}

#[test]
fn test_pub_stable_effect_group() {
    let prog = parse_ok("pub stable effect group read_only = reads(Db);");
    if let Item::EffectGroup(g) = &prog.items[0] {
        assert_eq!(g.name, "read_only");
        assert!(g.is_stable);
        assert!(g.is_pub);
    } else {
        panic!("Expected EffectGroup");
    }
}

#[test]
fn test_effect_group_plus_separator_only() {
    // Spec mandates + separator, not comma
    let prog = parse_ok("effect group all = reads(A) + writes(B) + sends(C);");
    if let Item::EffectGroup(g) = &prog.items[0] {
        assert_eq!(g.body.len(), 3);
    } else {
        panic!("Expected EffectGroup");
    }
}

#[test]
fn test_function_with_effects() {
    let prog = parse_ok("fn save(user: User) writes(UserDB) { }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 1);
        if let EffectItem::Verb(v) = &effects.items[0] {
            assert_eq!(v.kind, EffectVerbKind::Writes);
            assert_eq!(v.resources.len(), 1);
        }
    }
}

#[test]
fn test_function_with_multiple_effects() {
    let prog = parse_ok(
        "fn process(id: u64) -> i64 with reads(UserDB) writes(OrderDB) sends(Network) { 0 }",
    );
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 3);
    }
}

#[test]
fn test_function_with_effect_group() {
    let prog = parse_ok("fn process(order: Order) with OrderProcessing { }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert!(matches!(&effects.items[0], EffectItem::Group(n) if n == "OrderProcessing"));
    }
}

#[test]
fn test_function_with_effect_polymorphism() {
    let prog = parse_ok("fn run[T: Processor](p: T) -> i64 with _ { 0 }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert!(matches!(&effects.items[0], EffectItem::Polymorphic));
    }
}

#[test]
fn test_panics_effect() {
    let prog = parse_ok("fn crash() panics { }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        if let EffectItem::Verb(v) = &effects.items[0] {
            assert_eq!(v.kind, EffectVerbKind::Panics);
            assert!(v.resources.is_empty());
        }
    }
}

// ── 2.4: Ownership Syntax ────────────────────────────────────────

#[test]
fn test_ref_type() {
    parse_ok("fn first_word(s: ref String) -> ref String { s }");
}

#[test]
fn test_mut_ref_type() {
    parse_ok("fn modify(s: mut ref String) { }");
}

#[test]
fn test_weak_field() {
    let prog = parse_ok("struct Child { parent: weak Parent }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert!(matches!(&s.fields[0].ty.kind, TypeKind::Weak(_)));
    }
}

#[test]
fn test_shared_struct() {
    let prog = parse_ok("shared struct Node { value: i64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.name, "Node");
        assert!(s.is_shared);
        assert!(!s.is_pub);
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_pub_shared_struct() {
    let prog = parse_ok("pub shared struct Node { value: i64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.name, "Node");
        assert!(s.is_shared);
        assert!(s.is_pub);
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_shared_enum() {
    let prog = parse_ok("shared enum Tree { Leaf(i64), Branch(Tree, Tree) }");
    if let Item::EnumDef(e) = &prog.items[0] {
        assert_eq!(e.name, "Tree");
        assert!(e.is_shared);
        assert!(!e.is_pub);
        assert_eq!(e.variants.len(), 2);
    } else {
        panic!("Expected EnumDef");
    }
}

#[test]
fn test_pub_shared_enum() {
    let prog = parse_ok("pub shared enum Tree { Leaf(i64) }");
    if let Item::EnumDef(e) = &prog.items[0] {
        assert_eq!(e.name, "Tree");
        assert!(e.is_shared);
        assert!(e.is_pub);
    } else {
        panic!("Expected EnumDef");
    }
}

#[test]
fn test_shared_struct_with_mut_fields() {
    let prog = parse_ok("shared struct Counter { pub mut count: i64, name: String }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert!(s.is_shared);
        assert!(s.fields[0].is_pub);
        assert!(s.fields[0].is_mut);
        assert_eq!(s.fields[0].name, "count");
        assert!(!s.fields[1].is_pub);
        assert!(!s.fields[1].is_mut);
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_non_shared_struct_is_not_shared() {
    let prog = parse_ok("struct Point { x: i64, y: i64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert!(!s.is_shared);
    } else {
        panic!("Expected StructDef");
    }
}

// ── 2.5: Modules and Visibility ──────────────────────────────────

#[test]
fn test_mod_decl_rejected_at_parse_time() {
    // CR-24 slice 9 / brainstorming_v41.md §M1b: `mod name;` is not a Kāra
    // construct — module structure comes from the directory tree. The
    // parser rejects it with a directive-style diagnostic that points the
    // user at the `docs/design.md § Module System` rule.
    let (_, errors) = parse_with_errors("mod parser;");
    assert!(
        errors.iter().any(|e| e
            .message
            .contains("`mod` declarations are not used in Kāra")
            && e.message.contains("derived from the directory tree")),
        "expected directive-style mod-rejection diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_mod_decl_rejection_recovers_for_following_items() {
    // After rejecting `mod foo;` the parser should resync cleanly so the
    // following item still parses. We expect exactly one parse error
    // (the mod-rejection) and a fully-formed function in the program.
    let (prog, errors) = parse_with_errors("mod foo; fn bar() { }");
    assert_eq!(errors.len(), 1, "got {:?}", errors);
    assert!(matches!(prog.items.last(), Some(Item::Function(f)) if f.name == "bar"));
}

#[test]
fn test_use_decl() {
    let prog = parse_ok("use std.collections.HashMap;");
    if let Item::UseDecl(u) = &prog.items[0] {
        assert_eq!(u.path, vec!["std", "collections", "HashMap"]);
    }
}

#[test]
fn test_pub_struct() {
    let prog = parse_ok("pub struct Config { timeout: u64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert!(s.is_pub);
    }
}

#[test]
fn test_pub_use_decl() {
    let prog = parse_ok("pub use db.connection.Connection;");
    if let Item::UseDecl(u) = &prog.items[0] {
        assert!(u.is_pub);
        assert_eq!(u.path, vec!["db", "connection", "Connection"]);
    } else {
        panic!("Expected UseDecl");
    }
}

#[test]
fn test_use_decl_not_pub_by_default() {
    let prog = parse_ok("use std.io.Read;");
    if let Item::UseDecl(u) = &prog.items[0] {
        assert!(!u.is_pub);
    } else {
        panic!("Expected UseDecl");
    }
}

#[test]
fn test_pub_fn() {
    let prog = parse_ok("pub fn hello() { }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.is_pub);
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_pub_enum() {
    let prog = parse_ok("pub enum Color { Red, Green, Blue }");
    if let Item::EnumDef(e) = &prog.items[0] {
        assert!(e.is_pub);
    } else {
        panic!("Expected EnumDef");
    }
}

#[test]
fn test_pub_trait() {
    let prog = parse_ok("pub trait Display { fn display(self) -> String; }");
    if let Item::TraitDef(t) = &prog.items[0] {
        assert!(t.is_pub);
    } else {
        panic!("Expected TraitDef");
    }
}

// ── 2.6: Other Syntax ────────────────────────────────────────────

#[test]
fn test_match_expression() {
    let prog = parse_ok(
        r#"
        fn main() {
            let x = match shape {
                Circle { radius } => radius,
                Rectangle { width, height } => width,
            };
        }
    "#,
    );
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Match { arms, .. } = &value.kind {
                assert_eq!(arms.len(), 2);
            }
        }
    }
}

#[test]
fn test_match_tuple_variant() {
    parse_ok(
        r#"
        fn main() {
            match result {
                Ok(value) => value,
                Err(e) => { return; },
            };
        }
    "#,
    );
}

#[test]
fn test_tuple_destructuring() {
    parse_ok("fn main() { let (a, b) = get_pair(); }");
}

#[test]
fn test_attributes() {
    let prog = parse_ok("#[no_rc]\nfn hot_loop() { }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.attributes.len(), 1);
        assert_eq!(f.attributes[0].name, "no_rc");
    }
}

#[test]
fn test_attribute_with_args() {
    let prog = parse_ok("#[rc_budget(max: 5)]\nfn thing() { }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.attributes[0].name, "rc_budget");
        assert_eq!(f.attributes[0].args.len(), 1);
        assert_eq!(f.attributes[0].args[0].name.as_deref(), Some("max"));
    }
}

#[test]
fn test_attribute_with_equal_sign_args() {
    // Per `docs/design.md § Testing` and `§ String Interpolation`, attribute
    // args use `name = value` for new attributes. The parser also accepts
    // the legacy `name: value` form (covered by `test_attribute_with_args`),
    // but design-conformant attributes like `#[test(requires = [...])]`
    // must parse out of the box.
    let prog =
        parse_ok("#[test(requires = [db.UserDB, payment.PaymentAPI])]\nfn test_checkout() { }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.attributes[0].name, "test");
        assert_eq!(f.attributes[0].args.len(), 1);
        assert_eq!(f.attributes[0].args[0].name.as_deref(), Some("requires"));
        let value = f.attributes[0].args[0]
            .value
            .as_ref()
            .expect("requires arg should carry an array literal value");
        match &value.kind {
            ExprKind::ArrayLiteral(elems) => assert_eq!(elems.len(), 2),
            other => panic!("expected ArrayLiteral, got {other:?}"),
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_attribute_string_value() {
    let prog =
        parse_ok("#[must_use = \"connections must be explicitly disconnected\"]\nstruct Conn { }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.attributes[0].name, "must_use");
        assert_eq!(
            s.attributes[0].string_value.as_deref(),
            Some("connections must be explicitly disconnected")
        );
        assert!(s.attributes[0].args.is_empty());
    } else {
        panic!("Expected StructDef");
    }
}

// ── #[compiler_builtin] (CR-202 slice 1) ─────────────────────────
// The attribute is the marker the stdlib bake step (slice 3+) will use to
// tag intrinsic-bodied items. Slice 1 only proves parse + AST round-trip;
// resolver-level rejection in user code is exercised by tests/resolver.rs.

#[test]
fn test_compiler_builtin_on_function() {
    let prog = parse_ok("#[compiler_builtin]\nfn dbg[T](value: T) -> T { value }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.attributes.len(), 1);
        assert_eq!(f.attributes[0].name, "compiler_builtin");
        assert!(f.attributes[0].args.is_empty());
        assert!(f.attributes[0].string_value.is_none());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_compiler_builtin_on_struct() {
    let prog = parse_ok("#[compiler_builtin]\nstruct Vec[T] { }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.attributes.len(), 1);
        assert_eq!(s.attributes[0].name, "compiler_builtin");
        assert!(s.attributes[0].args.is_empty());
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_compiler_builtin_on_enum() {
    let prog = parse_ok("#[compiler_builtin]\nenum Option[T] { Some(T), None }");
    if let Item::EnumDef(e) = &prog.items[0] {
        assert_eq!(e.attributes.len(), 1);
        assert_eq!(e.attributes[0].name, "compiler_builtin");
        assert!(e.attributes[0].args.is_empty());
    } else {
        panic!("Expected EnumDef");
    }
}

#[test]
fn test_compiler_builtin_on_trait() {
    let prog = parse_ok("#[compiler_builtin]\ntrait Display { fn fmt(ref self) -> String; }");
    if let Item::TraitDef(t) = &prog.items[0] {
        assert_eq!(t.attributes.len(), 1);
        assert_eq!(t.attributes[0].name, "compiler_builtin");
        assert!(t.attributes[0].args.is_empty());
    } else {
        panic!("Expected TraitDef");
    }
}

// ── #[non_exhaustive] slice 1 ────────────────────────────────────
//
// The attribute is a bare `#[non_exhaustive]` form (no args). Parser
// captures it as an `is_non_exhaustive: bool` flag on `StructDef` and
// `EnumDef` so downstream passes can consult the flag without
// re-walking attributes. Placement validation (rejection on private
// types, traits, fns, impl blocks, etc.) lives in the resolver —
// covered by tests/resolver.rs. Per design.md § `#[non_exhaustive]`
// for Evolvable Public Types.

#[test]
fn non_exhaustive_slice1_struct_sets_flag() {
    let prog = parse_ok("#[non_exhaustive]\npub struct Config { timeout: i64, }");
    let Item::StructDef(s) = &prog.items[0] else {
        panic!("Expected StructDef");
    };
    assert!(s.is_non_exhaustive);
    assert_eq!(s.attributes.len(), 1);
    assert_eq!(s.attributes[0].name, "non_exhaustive");
    assert!(s.attributes[0].args.is_empty());
    assert!(s.attributes[0].string_value.is_none());
}

#[test]
fn non_exhaustive_slice1_enum_sets_flag() {
    let prog = parse_ok("#[non_exhaustive]\npub enum Error { NotFound, Conflict, }");
    let Item::EnumDef(e) = &prog.items[0] else {
        panic!("Expected EnumDef");
    };
    assert!(e.is_non_exhaustive);
    assert_eq!(e.attributes.len(), 1);
    assert_eq!(e.attributes[0].name, "non_exhaustive");
}

#[test]
fn non_exhaustive_slice1_struct_without_attribute_has_flag_false() {
    let prog = parse_ok("pub struct Config { timeout: i64, }");
    let Item::StructDef(s) = &prog.items[0] else {
        panic!("Expected StructDef");
    };
    assert!(!s.is_non_exhaustive);
}

#[test]
fn non_exhaustive_slice1_enum_without_attribute_has_flag_false() {
    let prog = parse_ok("pub enum Error { NotFound, }");
    let Item::EnumDef(e) = &prog.items[0] else {
        panic!("Expected EnumDef");
    };
    assert!(!e.is_non_exhaustive);
}

#[test]
fn non_exhaustive_slice1_flag_set_on_private_struct_parser_does_not_reject() {
    // Parser captures the attribute uniformly; resolver decides
    // whether the placement is legal. This is a pure parser-side
    // pin — `private struct` rejection is in tests/resolver.rs.
    let prog = parse_ok("#[non_exhaustive]\nprivate struct Internal { x: i64, }");
    let Item::StructDef(s) = &prog.items[0] else {
        panic!("Expected StructDef");
    };
    assert!(s.is_non_exhaustive);
    assert!(s.is_private);
    assert!(!s.is_pub);
}

#[test]
fn non_exhaustive_slice1_coexists_with_other_attributes_on_struct() {
    let prog = parse_ok(
        "#[non_exhaustive]\n#[must_use = \"build with .new()\"]\npub struct Config { x: i64, }",
    );
    let Item::StructDef(s) = &prog.items[0] else {
        panic!("Expected StructDef");
    };
    assert!(s.is_non_exhaustive);
    assert_eq!(s.attributes.len(), 2);
    assert!(s.attributes.iter().any(|a| a.name == "non_exhaustive"));
    assert!(s.attributes.iter().any(|a| a.name == "must_use"));
}

// ── #[track_caller] slice 1 ────────────────────────────────────────
//
// `#[track_caller]` is a bare type-level attribute on `fn` items.
// Parser captures it as `is_track_caller: bool` on `Function` so
// codegen / runtime slices can consult one bool. The attribute MUST
// take no arguments; malformed forms produce
// `E_TRACK_CALLER_ARGS_NOT_PERMITTED`. Placement validation (must be
// on `fn`, rejected on struct / enum / trait / impl / etc.) lives in
// the resolver — covered by tests/resolver.rs. Per design.md § Error
// Handling > "Stdlib panic-emitters report the caller's source
// location".

#[test]
fn track_caller_slice1_sets_flag_on_function() {
    let prog = parse_ok("#[track_caller]\nfn unwrap_inner() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert!(f.is_track_caller);
    assert_eq!(f.attributes.len(), 1);
    assert_eq!(f.attributes[0].name, "track_caller");
    assert!(f.attributes[0].args.is_empty());
    assert!(f.attributes[0].string_value.is_none());
}

#[test]
fn track_caller_slice1_function_without_attribute_has_flag_false() {
    let prog = parse_ok("fn unwrap_inner() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert!(!f.is_track_caller);
}

#[test]
fn track_caller_slice1_coexists_with_other_attributes() {
    let prog =
        parse_ok("#[track_caller]\n#[must_use = \"propagate\"]\nfn unwrap_inner() -> i64 { 0 }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert!(f.is_track_caller);
    assert_eq!(f.attributes.len(), 2);
}

#[test]
fn track_caller_slice1_rejects_paren_args() {
    let (_prog, errors) = parse_with_errors("#[track_caller(extra)]\nfn unwrap_inner() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_TRACK_CALLER_ARGS_NOT_PERMITTED")),
        "Expected E_TRACK_CALLER_ARGS_NOT_PERMITTED diagnostic; got: {errors:?}"
    );
}

#[test]
fn track_caller_slice1_rejects_string_value() {
    let (_prog, errors) = parse_with_errors("#[track_caller = \"oops\"]\nfn unwrap_inner() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_TRACK_CALLER_ARGS_NOT_PERMITTED")),
        "Expected E_TRACK_CALLER_ARGS_NOT_PERMITTED diagnostic; got: {errors:?}"
    );
}

#[test]
fn track_caller_slice1_duplicate_attribute_is_idempotent() {
    // Two copies of the bare attribute: parser is happy, flag stays
    // true. No "duplicate attribute" diagnostic at this slice — the
    // attribute is bare and idempotent.
    let prog = parse_ok("#[track_caller]\n#[track_caller]\nfn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert!(f.is_track_caller);
    assert_eq!(f.attributes.len(), 2);
}

#[test]
fn track_caller_slice1_set_on_non_fn_item_parser_does_not_reject() {
    // Parser captures attributes uniformly; the resolver rejects
    // placement on non-fn items. The struct still parses cleanly.
    let prog = parse_ok("#[track_caller]\npub struct Config { x: i64, }");
    let Item::StructDef(s) = &prog.items[0] else {
        panic!("Expected StructDef");
    };
    assert_eq!(s.attributes.len(), 1);
    assert_eq!(s.attributes[0].name, "track_caller");
}

// ── #[deprecated] slices 1+2 — parser + AST payload ───────────────
//
// Three forms per design.md § `#[deprecated]` for Item Deprecation:
//   - bare `#[deprecated]`
//   - shorthand `#[deprecated = "note"]`
//   - long form `#[deprecated(since: "...", note: "...")]`
//
// All seven attribute-bearing target kinds capture the payload:
// Function, StructDef, EnumDef, TraitDef, TraitAliasDef,
// MarkerTraitDef, DistinctTypeDef. Variant / TraitMethod /
// TypeAliasDef / ConstDecl don't carry attributes today —
// extending those is a separate enabling change.
//
// Diagnostics:
//   - E_DEPRECATED_UNKNOWN_FIELD (rejects keys other than since/note)
//   - E_DEPRECATED_FIELD_NOT_STRING (non-string value)
//   - E_DEPRECATED_POSITIONAL_ARG (positional arg in long form)
//   - E_DEPRECATED_DUPLICATE (multiple #[deprecated] on one item)

#[test]
fn deprecated_slice1_bare_form_on_function() {
    let prog = parse_ok("#[deprecated]\nfn old_api() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let d = f
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert!(d.since.is_none());
    assert!(d.note.is_none());
}

#[test]
fn deprecated_slice1_shorthand_populates_note() {
    let prog = parse_ok("#[deprecated = \"use `read_to_string` instead\"]\nfn old_api() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let d = f
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert!(d.since.is_none());
    assert_eq!(d.note.as_deref(), Some("use `read_to_string` instead"));
}

#[test]
fn deprecated_slice1_long_form_with_since_only() {
    let prog = parse_ok("#[deprecated(since: \"1.2.0\")]\nfn old_api() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let d = f
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert_eq!(d.since.as_deref(), Some("1.2.0"));
    assert!(d.note.is_none());
}

#[test]
fn deprecated_slice1_long_form_with_note_only() {
    let prog = parse_ok("#[deprecated(note: \"use foo\")]\nfn old_api() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let d = f
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert!(d.since.is_none());
    assert_eq!(d.note.as_deref(), Some("use foo"));
}

#[test]
fn deprecated_slice1_long_form_with_both_fields() {
    let prog = parse_ok(
        "#[deprecated(since: \"1.2.0\", note: \"use `Channel.unbounded()` instead\")]\n\
         fn make_channel() { }",
    );
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let d = f
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert_eq!(d.since.as_deref(), Some("1.2.0"));
    assert_eq!(d.note.as_deref(), Some("use `Channel.unbounded()` instead"),);
}

#[test]
fn deprecated_slice1_long_form_with_equals_separator() {
    // The spec example uses `name: value` but the attribute parser
    // also accepts `name = value`. Both shapes must populate the
    // same fields.
    let prog = parse_ok("#[deprecated(since = \"1.2.0\", note = \"use foo\")]\nfn old() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let d = f
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert_eq!(d.since.as_deref(), Some("1.2.0"));
    assert_eq!(d.note.as_deref(), Some("use foo"));
}

#[test]
fn deprecated_slice1_struct_captures_payload() {
    let prog = parse_ok("#[deprecated]\npub struct OldShape { x: i64, }");
    let Item::StructDef(s) = &prog.items[0] else {
        panic!("Expected StructDef");
    };
    assert!(s.deprecation.is_some());
}

#[test]
fn deprecated_slice1_enum_captures_payload() {
    let prog = parse_ok("#[deprecated = \"use NewError\"]\npub enum OldError { Bad, }");
    let Item::EnumDef(e) = &prog.items[0] else {
        panic!("Expected EnumDef");
    };
    let d = e
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert_eq!(d.note.as_deref(), Some("use NewError"));
}

#[test]
fn deprecated_slice1_trait_captures_payload() {
    let prog = parse_ok("#[deprecated]\npub trait OldFormat { fn write(ref self); }");
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    assert!(t.deprecation.is_some());
}

#[test]
fn deprecated_slice1_marker_trait_captures_payload() {
    let prog = parse_ok("#[deprecated]\npub marker trait OldMarker;");
    let Item::MarkerTrait(m) = &prog.items[0] else {
        panic!("Expected MarkerTrait");
    };
    assert!(m.deprecation.is_some());
}

#[test]
fn deprecated_slice1_trait_alias_captures_payload() {
    let prog = parse_ok("#[deprecated]\npub trait OldEq = Eq;");
    let Item::TraitAlias(t) = &prog.items[0] else {
        panic!("Expected TraitAlias");
    };
    assert!(t.deprecation.is_some());
}

#[test]
fn deprecated_slice1_distinct_type_captures_payload() {
    let prog = parse_ok("#[deprecated]\npub distinct type OldId = i64;");
    let Item::DistinctType(d) = &prog.items[0] else {
        panic!("Expected DistinctType");
    };
    assert!(d.deprecation.is_some());
}

#[test]
fn deprecated_slice1_without_attribute_leaves_field_none() {
    let prog = parse_ok("fn fresh_api() { }\nstruct Fresh { x: i64, }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert!(f.deprecation.is_none());
    let Item::StructDef(s) = &prog.items[1] else {
        panic!("Expected StructDef");
    };
    assert!(s.deprecation.is_none());
}

#[test]
fn deprecated_slice1_rejects_unknown_field() {
    let (_prog, errors) = parse_with_errors("#[deprecated(authored_by: \"alice\")]\nfn old() { }");
    assert!(
        errors.iter().any(|e| {
            e.message.contains("E_DEPRECATED_UNKNOWN_FIELD")
                && e.message.contains("authored_by")
                && e.message.contains("since")
                && e.message.contains("note")
        }),
        "Expected E_DEPRECATED_UNKNOWN_FIELD naming the offending field and accepted set; got: {errors:?}"
    );
}

#[test]
fn deprecated_slice1_rejects_non_string_value() {
    let (_prog, errors) = parse_with_errors("#[deprecated(since: 1)]\nfn old() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_DEPRECATED_FIELD_NOT_STRING")
                && e.message.contains("since")),
        "Expected E_DEPRECATED_FIELD_NOT_STRING naming the offending field; got: {errors:?}"
    );
}

#[test]
fn deprecated_slice1_rejects_positional_arg() {
    // Long form requires named args — bare strings inside parens
    // are a malformed long form (the shorthand uses `= "..."`).
    let (_prog, errors) = parse_with_errors("#[deprecated(\"oops\")]\nfn old() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_DEPRECATED_POSITIONAL_ARG")),
        "Expected E_DEPRECATED_POSITIONAL_ARG; got: {errors:?}"
    );
}

#[test]
fn deprecated_slice1_rejects_duplicate_attribute() {
    let (_prog, errors) =
        parse_with_errors("#[deprecated]\n#[deprecated = \"second\"]\nfn old() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_DEPRECATED_DUPLICATE")),
        "Expected E_DEPRECATED_DUPLICATE; got: {errors:?}"
    );
}

#[test]
fn deprecated_slice1_first_attribute_wins_when_duplicate() {
    // After the duplicate diagnostic fires, the first attribute's
    // payload survives — that's the spec's idempotency rule.
    let (prog, _errors) =
        parse_with_errors("#[deprecated = \"first\"]\n#[deprecated = \"second\"]\nfn old() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let d = f
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert_eq!(d.note.as_deref(), Some("first"));
}

#[test]
fn deprecated_slice1_set_on_impl_block_parser_does_not_reject() {
    // Parser captures attributes uniformly; resolver rejects
    // placement on impl blocks (covered in tests/resolver.rs).
    let prog = parse_ok(
        "pub struct Foo { x: i64, }\n#[deprecated]\nimpl Foo { fn x(ref self) -> i64 { 0 } }",
    );
    let Item::ImplBlock(imp) = &prog.items[1] else {
        panic!("Expected ImplBlock");
    };
    assert_eq!(imp.attributes.len(), 1);
    assert_eq!(imp.attributes[0].name, "deprecated");
}

// ── TraitMethod + Variant attributes enabling change ─────────────
//
// `TraitMethod` and `Variant` AST nodes now carry `attributes`
// (and `doc_comment`, for TraitMethod). This unblocks
// `#[deprecated]` on enum variants and trait method declarations,
// and `#[track_caller]` on trait method declarations per the
// design.md specs.
//
// Parser changes:
//   - `parse_trait_def_tail` collects attributes before each item
//     and threads them into `parse_trait_method`. Attributes on
//     associated-type declarations are rejected with
//     `E_ATTR_ON_ASSOC_TYPE_DECL` (out of v1 scope).
//   - `parse_variant` collects attributes before the variant name
//     and threads them into the AST node.

#[test]
fn enabling_change_variant_captures_attributes() {
    let prog = parse_ok(
        "pub enum Color {\n\
         #[deprecated = \"use Rgb\"]\n\
         Legacy,\n\
         Modern,\n\
         }",
    );
    let Item::EnumDef(e) = &prog.items[0] else {
        panic!("Expected EnumDef");
    };
    assert_eq!(e.variants.len(), 2);
    assert_eq!(e.variants[0].name, "Legacy");
    assert_eq!(e.variants[0].attributes.len(), 1);
    assert_eq!(e.variants[0].attributes[0].name, "deprecated");
    let d = e.variants[0]
        .deprecation
        .as_ref()
        .expect("Legacy variant should have Deprecation payload");
    assert_eq!(d.note.as_deref(), Some("use Rgb"));
    assert!(e.variants[1].attributes.is_empty());
    assert!(e.variants[1].deprecation.is_none());
}

#[test]
fn enabling_change_variant_bare_deprecated_attaches() {
    let prog = parse_ok("pub enum Color { #[deprecated] Red, Green, Blue, }");
    let Item::EnumDef(e) = &prog.items[0] else {
        panic!("Expected EnumDef");
    };
    assert!(e.variants[0].deprecation.is_some());
    assert!(e.variants[1].deprecation.is_none());
}

#[test]
fn enabling_change_trait_method_captures_attributes() {
    let prog = parse_ok(
        "pub trait Format {\n\
         #[deprecated = \"use fmt_v2\"]\n\
         fn fmt(ref self) -> String;\n\
         fn fmt_v2(ref self) -> String;\n\
         }",
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let methods = trait_methods(t);
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].name, "fmt");
    assert_eq!(methods[0].attributes.len(), 1);
    assert_eq!(methods[0].attributes[0].name, "deprecated");
    let d = methods[0]
        .deprecation
        .as_ref()
        .expect("fmt method should have Deprecation payload");
    assert_eq!(d.note.as_deref(), Some("use fmt_v2"));
    assert!(methods[1].attributes.is_empty());
    assert!(methods[1].deprecation.is_none());
}

#[test]
fn enabling_change_trait_method_track_caller_attaches() {
    // `#[track_caller]` on a trait method declaration is now legal —
    // the propagation rule (impls inherit unless explicitly dropped)
    // is the codegen slice's job; this test pins the parse surface.
    let prog = parse_ok(
        "pub trait Show {\n\
         #[track_caller]\n\
         fn show(ref self) -> String;\n\
         }",
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let methods = trait_methods(t);
    assert!(methods[0].is_track_caller);
    assert_eq!(methods[0].attributes.len(), 1);
    assert_eq!(methods[0].attributes[0].name, "track_caller");
}

#[test]
fn enabling_change_trait_method_doc_comment_captured() {
    let prog = parse_ok(
        "pub trait Format {\n\
         /// Render to a string.\n\
         fn fmt(ref self) -> String;\n\
         }",
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let methods = trait_methods(t);
    assert_eq!(
        methods[0].doc_comment.as_deref(),
        Some("Render to a string."),
    );
}

#[test]
fn enabling_change_attr_on_assoc_type_decl_rejected() {
    let (_prog, errors) = parse_with_errors(
        "pub trait Container {\n\
         #[deprecated]\n\
         type Item;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_ATTR_ON_ASSOC_TYPE_DECL")),
        "Expected E_ATTR_ON_ASSOC_TYPE_DECL diagnostic; got: {errors:?}"
    );
}

#[test]
fn enabling_change_unsafe_fn_in_trait_with_attribute() {
    // The `unsafe fn` dispatch in the trait body must thread the
    // attributes through, not lose them.
    let prog = parse_ok(
        "pub trait Raw {\n\
         #[deprecated]\n\
         unsafe fn raw_op(ref self);\n\
         }",
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let methods = trait_methods(t);
    assert_eq!(methods[0].name, "raw_op");
    assert!(methods[0].is_unsafe);
    assert!(methods[0].deprecation.is_some());
}

#[test]
fn enabling_change_attr_on_tuple_variant() {
    let prog = parse_ok("pub enum E { #[deprecated] Old(i64), New(i64), }");
    let Item::EnumDef(e) = &prog.items[0] else {
        panic!("Expected EnumDef");
    };
    assert!(e.variants[0].deprecation.is_some());
    assert!(matches!(e.variants[0].kind, VariantKind::Tuple(_)));
}

#[test]
fn enabling_change_attr_on_struct_variant() {
    let prog = parse_ok("pub enum E { #[deprecated] Old { x: i64 }, New { x: i64 }, }");
    let Item::EnumDef(e) = &prog.items[0] else {
        panic!("Expected EnumDef");
    };
    assert!(e.variants[0].deprecation.is_some());
    assert!(matches!(e.variants[0].kind, VariantKind::Struct(_)));
}

// ── Lint level attributes slice 1+2+3 ─────────────────────────────
//
// Per design.md § Lint Level Attributes, four attributes override
// a named lint's level: #[allow(NAME)], #[warn(NAME)], #[deny(NAME)],
// #[expect(NAME)]. Each accepts a comma-separated list of lint
// identifiers.
//
// This slice ships:
//   - LintLevel enum + LintLevelOverride struct + LintRegistry
//   - Parser captures lint_overrides on Function (representative
//     item; broader attachment ships with slice 4 cascade).
//   - 3 parse-time diagnostics: E_DUPLICATE_LINT_LEVEL,
//     E_LINT_LEVEL_NON_IDENT_ARG, E_LINT_LEVEL_NO_ARGS
//
// Deferred (slice 4+): scope cascade, unknown_lint warning emit,
// #[expect] semantics, lint-name carry into warning diagnostics.

use karac::lints::LintLevel;

#[test]
fn lint_attrs_slice1_allow_single_lint_captures_override() {
    let prog = parse_ok("#[allow(deprecated)]\nfn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert_eq!(f.lint_overrides.len(), 1);
    assert_eq!(f.lint_overrides[0].level, LintLevel::Allow);
    assert_eq!(f.lint_overrides[0].lint, "deprecated");
}

#[test]
fn lint_attrs_slice1_all_four_attribute_names_recognized() {
    let prog = parse_ok(
        "#[allow(deprecated)]\n\
         #[warn(rc_fallback)]\n\
         #[deny(implicit_clone)]\n\
         #[expect(unfulfilled_lint_expectation)]\n\
         fn f() { }",
    );
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert_eq!(f.lint_overrides.len(), 4);
    assert_eq!(f.lint_overrides[0].level, LintLevel::Allow);
    assert_eq!(f.lint_overrides[1].level, LintLevel::Warn);
    assert_eq!(f.lint_overrides[2].level, LintLevel::Deny);
    assert_eq!(f.lint_overrides[3].level, LintLevel::Expect);
}

#[test]
fn lint_attrs_slice1_comma_separated_list_produces_multiple_overrides() {
    let prog = parse_ok("#[allow(deprecated, rc_fallback, implicit_clone)]\nfn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert_eq!(f.lint_overrides.len(), 3);
    let names: Vec<&str> = f.lint_overrides.iter().map(|o| o.lint.as_str()).collect();
    assert_eq!(names, vec!["deprecated", "rc_fallback", "implicit_clone"]);
    for o in &f.lint_overrides {
        assert_eq!(o.level, LintLevel::Allow);
    }
}

#[test]
fn lint_attrs_slice1_function_without_attribute_has_empty_overrides() {
    let prog = parse_ok("fn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert!(f.lint_overrides.is_empty());
}

#[test]
fn lint_attrs_slice1_unknown_lint_silently_accepted() {
    // Per design.md § Lint Level Attributes "Naming" rule, an
    // unknown lint surfaces the `unknown_lint` warning once the
    // lint emission infrastructure lands. Today the parser
    // silently accepts unknown names so `#[allow(removed_lint)]`
    // from older code continues to build.
    let prog = parse_ok("#[allow(some_lint_that_does_not_exist)]\nfn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert_eq!(f.lint_overrides.len(), 1);
    assert_eq!(f.lint_overrides[0].lint, "some_lint_that_does_not_exist");
}

#[test]
fn lint_attrs_slice1_duplicate_lint_in_single_attribute_rejected() {
    let (_prog, errors) = parse_with_errors("#[allow(deprecated, deprecated)]\nfn f() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_DUPLICATE_LINT_LEVEL")
                && e.message.contains("deprecated")),
        "Expected E_DUPLICATE_LINT_LEVEL naming `deprecated`; got: {errors:?}"
    );
}

#[test]
fn lint_attrs_slice1_duplicate_in_separate_attrs_currently_allowed() {
    // Cross-attribute duplicates are accepted at this slice — the
    // scope cascade (slice 4) will define last-writer-wins
    // semantics, and de-duping here would preempt that decision.
    let prog = parse_ok("#[allow(deprecated)]\n#[allow(deprecated)]\nfn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert_eq!(f.lint_overrides.len(), 2);
}

#[test]
fn lint_attrs_slice1_empty_arg_list_rejected() {
    let (_prog, errors) = parse_with_errors("#[allow()]\nfn f() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_LINT_LEVEL_NO_ARGS")),
        "Expected E_LINT_LEVEL_NO_ARGS; got: {errors:?}"
    );
}

#[test]
fn lint_attrs_slice1_bare_form_with_no_args_rejected() {
    // `#[allow]` with no parens — same diagnostic family as
    // `#[allow()]`. Both produce zero lint names, both rejected.
    let (_prog, errors) = parse_with_errors("#[allow]\nfn f() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_LINT_LEVEL_NO_ARGS")),
        "Expected E_LINT_LEVEL_NO_ARGS for bare `#[allow]`; got: {errors:?}"
    );
}

#[test]
fn lint_attrs_slice1_string_value_rejected() {
    let (_prog, errors) = parse_with_errors("#[allow = \"deprecated\"]\nfn f() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_LINT_LEVEL_NON_IDENT_ARG")),
        "Expected E_LINT_LEVEL_NON_IDENT_ARG; got: {errors:?}"
    );
}

#[test]
fn lint_attrs_slice1_key_value_arg_rejected() {
    let (_prog, errors) = parse_with_errors("#[allow(deprecated = \"oops\")]\nfn f() { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_LINT_LEVEL_NON_IDENT_ARG")),
        "Expected E_LINT_LEVEL_NON_IDENT_ARG for key=value arg; got: {errors:?}"
    );
}

#[test]
fn lint_attrs_slice1_spans_point_at_lint_name() {
    // The override's span should locate the lint name (not the
    // whole attribute), so the scope-cascade diagnostic (slice 4)
    // can underline the precise authoring site.
    let prog = parse_ok("#[allow(deprecated)]\nfn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    // We can't easily compare spans here without exposing more,
    // but at minimum the override has *some* span associated.
    assert!(f.lint_overrides[0].span.length > 0);
}

#[test]
fn lint_attrs_slice1_attribute_coexists_with_other_attributes() {
    let prog = parse_ok("#[allow(deprecated)]\n#[track_caller]\n#[deprecated]\nfn f() { }");
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    assert_eq!(f.lint_overrides.len(), 1);
    assert!(f.is_track_caller);
    assert!(f.deprecation.is_some());
}

// ── TypeAliasDef + ConstDecl attributes enabling change ──────────
//
// `ConstDecl` and `TypeAliasDef` AST nodes now carry `attributes`
// and `deprecation`. Closes the remaining sub-open flagged in the
// #[deprecated] entry (line 381): the spec listed module-level
// consts and type aliases as legal #[deprecated] targets but the
// AST nodes had no attributes field.

#[test]
fn const_attrs_module_const_captures_deprecated() {
    let prog = parse_ok("#[deprecated = \"use NEW_VAL\"]\npub const OLD_VAL: i64 = 42;");
    let Item::ConstDecl(c) = &prog.items[0] else {
        panic!("Expected ConstDecl");
    };
    assert_eq!(c.attributes.len(), 1);
    assert_eq!(c.attributes[0].name, "deprecated");
    let d = c
        .deprecation
        .as_ref()
        .expect("expected Deprecation payload");
    assert_eq!(d.note.as_deref(), Some("use NEW_VAL"));
}

#[test]
fn const_attrs_module_const_without_attribute_has_empty_fields() {
    let prog = parse_ok("pub const VAL: i64 = 42;");
    let Item::ConstDecl(c) = &prog.items[0] else {
        panic!("Expected ConstDecl");
    };
    assert!(c.attributes.is_empty());
    assert!(c.deprecation.is_none());
}

#[test]
fn const_attrs_type_alias_captures_deprecated() {
    let prog = parse_ok("#[deprecated]\npub type OldHandle = i64;");
    let Item::TypeAlias(t) = &prog.items[0] else {
        panic!("Expected TypeAlias");
    };
    assert_eq!(t.attributes.len(), 1);
    assert!(t.deprecation.is_some());
}

#[test]
fn const_attrs_type_alias_without_attribute_has_empty_fields() {
    let prog = parse_ok("pub type Handle = i64;");
    let Item::TypeAlias(t) = &prog.items[0] else {
        panic!("Expected TypeAlias");
    };
    assert!(t.attributes.is_empty());
    assert!(t.deprecation.is_none());
}

#[test]
fn const_attrs_type_alias_long_form_deprecation() {
    let prog = parse_ok(
        "#[deprecated(since: \"1.2.0\", note: \"use NewHandle\")]\n\
         pub type OldHandle = i64;",
    );
    let Item::TypeAlias(t) = &prog.items[0] else {
        panic!("Expected TypeAlias");
    };
    let d = t.deprecation.as_ref().expect("expected Deprecation");
    assert_eq!(d.since.as_deref(), Some("1.2.0"));
    assert_eq!(d.note.as_deref(), Some("use NewHandle"));
}

#[test]
fn test_const_generic_args() {
    let prog = parse_ok("fn dot(a: Array[f64, 3], b: Array[f64, 3]) -> f64 { 0.0 }");
    if let Item::Function(f) = &prog.items[0] {
        // Should parse successfully with const generic arg 3
        assert_eq!(f.params.len(), 2);
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_const_generic_arg_in_type_annotation() {
    let prog = parse_ok("fn main() { let empty: Array[i64, 0] = []; }");
    // Should parse without error — 0 is a const generic arg
    assert!(!prog.items.is_empty());
}

#[test]
fn test_parser_const_arg_non_literal_expression() {
    // `Array[T, N + 1]` — the `N + 1` const-arg slot must carry a
    // `GenericArg::Const` of a `Binary` expression, not a `Type` arg.
    // Disambiguates by spotting `Identifier <binary op>` lookahead in
    // `parse_generic_type_args` (const generics slice 1, sub-step b).
    let prog = parse_ok("fn f[T, const N: i64](xs: Array[T, N + 1]) -> i64 { 0 }");
    if let Item::Function(f) = &prog.items[0] {
        let param_ty = &f.params[0].ty;
        // The `Array[T, N + 1]` annotation is a path with two generic args.
        let TypeKind::Path(ref path) = param_ty.kind else {
            panic!("Expected Array[...] to parse as TypeKind::Path");
        };
        let args = path.generic_args.as_ref().expect("expected generic args");
        assert_eq!(args.len(), 2);
        assert!(matches!(&args[0], GenericArg::Type(_)));
        match &args[1] {
            GenericArg::Const(expr) => match &expr.kind {
                ExprKind::Binary { op, .. } => {
                    assert!(matches!(op, BinOp::Add));
                }
                other => panic!("Expected GenericArg::Const(Binary), got Const({:?})", other),
            },
            other => panic!("Expected GenericArg::Const, got {:?}", other),
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_const_decl() {
    let prog = parse_ok("const MAX: u64 = 100;");
    if let Item::ConstDecl(c) = &prog.items[0] {
        assert_eq!(c.name, "MAX");
    }
}

#[test]
fn test_type_alias() {
    let prog = parse_ok("type UserId = u64;");
    if let Item::TypeAlias(t) = &prog.items[0] {
        assert_eq!(t.name, "UserId");
        assert!(t.refinement.is_none());
    }
}

#[test]
fn test_refinement_type_basic() {
    let prog = parse_ok("type NonZero = i32 where self != 0;");
    if let Item::TypeAlias(t) = &prog.items[0] {
        assert_eq!(t.name, "NonZero");
        assert!(t.refinement.is_some());
    } else {
        panic!("Expected TypeAlias");
    }
}

#[test]
fn test_refinement_type_numeric_range() {
    let prog = parse_ok("type Percentage = f64 where self >= 0.0 and self <= 100.0;");
    if let Item::TypeAlias(t) = &prog.items[0] {
        assert_eq!(t.name, "Percentage");
        assert!(t.refinement.is_some());
    } else {
        panic!("Expected TypeAlias");
    }
}

#[test]
fn test_refinement_type_pub() {
    let prog = parse_ok("pub type ValidPort = u16 where self >= 1 and self <= 65535;");
    if let Item::TypeAlias(t) = &prog.items[0] {
        assert_eq!(t.name, "ValidPort");
        assert!(t.is_pub);
        assert!(t.refinement.is_some());
    } else {
        panic!("Expected TypeAlias");
    }
}

#[test]
fn test_refinement_type_with_generics() {
    let prog = parse_ok("type NonEmpty[T] = Vec[T] where self.len() > 0;");
    if let Item::TypeAlias(t) = &prog.items[0] {
        assert_eq!(t.name, "NonEmpty");
        assert!(t.generic_params.is_some());
        assert!(t.refinement.is_some());
    } else {
        panic!("Expected TypeAlias");
    }
}

#[test]
fn test_refinement_type_multiple() {
    let src = r#"
        type Positive = i32 where self > 0;
        type NonZero = i32 where self != 0;
    "#;
    let prog = parse_ok(src);
    assert_eq!(prog.items.len(), 2);
    if let Item::TypeAlias(t) = &prog.items[0] {
        assert_eq!(t.name, "Positive");
        assert!(t.refinement.is_some());
    } else {
        panic!("Expected TypeAlias");
    }
    if let Item::TypeAlias(t) = &prog.items[1] {
        assert_eq!(t.name, "NonZero");
        assert!(t.refinement.is_some());
    } else {
        panic!("Expected TypeAlias");
    }
}

#[test]
fn test_extern_function() {
    let prog = parse_ok(
        r#"unsafe extern "C" {
               fn write(fd: i32, buf: i64, count: u64) -> i64 writes(FileSystem);
           }"#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock, got {:?}", &prog.items[0]);
    };
    assert_eq!(b.abi, "C");
    assert_eq!(b.items.len(), 1);
    let ExternItem::Function(f) = &b.items[0] else {
        panic!("expected ExternItem::Function");
    };
    assert_eq!(f.abi, "C");
    assert_eq!(f.name, "write");
    assert_eq!(f.params.len(), 3);
    assert!(f.effects.is_some());
}

#[test]
fn test_extern_function_accepts_pub_and_attributes() {
    let prog = parse_ok(
        r#"
        unsafe extern "C" {
            #[link_name = "puts"]
            pub fn puts(s: i64) -> i32;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock");
    };
    let ExternItem::Function(f) = &b.items[0] else {
        panic!("expected ExternItem::Function");
    };
    assert!(f.is_pub, "pub should be threaded to the inner fn");
    assert_eq!(f.attributes.len(), 1, "attribute should be captured");
    assert_eq!(f.attributes[0].name, "link_name");
    assert_eq!(f.abi, "C");
    assert_eq!(f.name, "puts");
}

#[test]
fn test_unsafe_extern_block_multiple_items() {
    let prog = parse_ok(
        r#"
        unsafe extern "C" {
            fn getpid() -> i32;
            fn write(fd: i32, buf: i64, count: u64) -> i64 writes(FileSystem);
            fn close(fd: i32) -> i32;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock");
    };
    assert_eq!(b.abi, "C");
    assert_eq!(b.items.len(), 3);
    let names: Vec<&str> = b
        .items
        .iter()
        .map(|it| {
            let ExternItem::Function(f) = it else {
                panic!("expected ExternItem::Function");
            };
            f.name.as_str()
        })
        .collect();
    assert_eq!(names, vec!["getpid", "write", "close"]);
}

#[test]
fn test_unsafe_extern_block_keeps_block_level_attribute_separate() {
    // Block-level `@noblock` lives on the `ExternBlock` itself; per-item
    // `attributes` lists hold ONLY per-item attributes (no pre-merge),
    // so the formatter can round-trip the block-header position
    // faithfully. Downstream consumers (effectchecker, codegen) take both
    // sets explicitly and union them per item.
    let prog = parse_ok(
        r#"
        @noblock
        unsafe extern "C" {
            fn abs(x: i32) -> i32;
            fn sqrt(x: f64) -> f64;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock");
    };
    assert!(
        b.attributes.iter().any(|a| a.name == "noblock"),
        "block-level @noblock should live on ExternBlock.attributes"
    );
    for it in &b.items {
        let ExternItem::Function(f) = it else {
            panic!("expected ExternItem::Function");
        };
        assert!(
            !f.attributes.iter().any(|a| a.name == "noblock"),
            "block-level @noblock must NOT be pre-merged into per-item attributes"
        );
    }
}

#[test]
fn test_unsafe_extern_block_per_item_attribute_stays_per_item() {
    // Per-item attributes inside an `unsafe extern { }` block stay on
    // the item, not on the block — symmetric to the block-level case.
    let prog = parse_ok(
        r#"
        unsafe extern "C" {
            @noblock
            fn abs(x: i32) -> i32;
            fn sqrt(x: f64) -> f64;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock");
    };
    assert!(
        !b.attributes.iter().any(|a| a.name == "noblock"),
        "per-item @noblock must NOT bubble up to ExternBlock.attributes"
    );
    let ExternItem::Function(abs) = &b.items[0] else {
        panic!("expected ExternItem::Function");
    };
    assert!(abs.attributes.iter().any(|a| a.name == "noblock"));
    let ExternItem::Function(sqrt) = &b.items[1] else {
        panic!("expected ExternItem::Function");
    };
    assert!(!sqrt.attributes.iter().any(|a| a.name == "noblock"));
}

#[test]
fn test_unsafe_extern_block_round_trips_block_level_attribute_position() {
    // Round-trip: source with a block-level attribute formats back with
    // the attribute still at the block-header position, not migrated
    // onto each child item. Closes the slice-1 round-trip non-idempotence
    // caveat (phase-5-diagnostics.md line 299). The `@noblock` sugar form
    // already normalises to `#[noblock]` via the existing attribute
    // renderer; this test pins the *position* (block-level vs per-item),
    // which is the load-bearing property.
    let src = "#[noblock]\nunsafe extern \"C\" {\n    fn abs(x: i32) -> i32;\n    fn sqrt(x: f64) -> f64;\n}\n";
    let prog = parse_ok(src);
    let formatted = karac::formatter::format_program(&prog);
    assert_eq!(formatted, src, "round-trip mismatch:\n{formatted}");
}

// ── #[unsafe(...)] wrap on linker control attributes ────────────────
//
// `#[unsafe(no_mangle)]` and `#[unsafe(link_section("..."))]` are the
// canonical authoring form (design.md § Linker Control Attributes).
// Bare `#[no_mangle]` and `#[link_section(...)]` are rejected at parse
// time; `#[used]` stays plain.

#[test]
fn test_unsafe_no_mangle_wrap_parses() {
    let prog = parse_ok("#[unsafe(no_mangle)]\nfn keep_me() -> i64 { 42 }");
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.attributes.len(), 1);
    assert_eq!(f.attributes[0].name, "no_mangle");
    assert!(f.attributes[0].args.is_empty());
    assert!(f.attributes[0].string_value.is_none());
}

#[test]
fn test_unsafe_link_section_wrap_parses() {
    let prog = parse_ok("#[unsafe(link_section(\".init_array\"))]\nfn ctor() -> i64 { 1 }");
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.attributes.len(), 1);
    assert_eq!(f.attributes[0].name, "link_section");
    assert_eq!(f.attributes[0].string_value.as_deref(), Some(".init_array"));
}

#[test]
fn test_bare_no_mangle_rejected_with_focused_diagnostic() {
    let (_, errors) = parse_with_errors("#[no_mangle]\nfn f() {}");
    assert!(
        !errors.is_empty(),
        "expected rejection of bare #[no_mangle]"
    );
    assert!(
        errors_contain(&errors, "#[unsafe(no_mangle)]"),
        "diagnostic should suggest the wrap; got {errors:?}"
    );
}

#[test]
fn test_bare_link_section_rejected_with_focused_diagnostic() {
    let (_, errors) = parse_with_errors("#[link_section(\".x\")]\nfn f() {}");
    assert!(
        !errors.is_empty(),
        "expected rejection of bare #[link_section(...)]"
    );
    assert!(
        errors_contain(&errors, "#[unsafe(link_section"),
        "diagnostic should suggest the wrap; got {errors:?}"
    );
}

#[test]
fn test_unsafe_wrap_unknown_inner_attribute_rejected() {
    // `#[unsafe(foo)]` — `foo` isn't one of the wrapped attributes;
    // emit a focused diagnostic naming the legal options. Catches a
    // user reaching for the wrap on something it doesn't apply to
    // (e.g. `#[unsafe(used)]` — `used` stays plain).
    let (_, errors) = parse_with_errors("#[unsafe(used)]\nfn f() {}");
    assert!(
        !errors.is_empty(),
        "expected rejection of unknown #[unsafe(...)] inner attribute"
    );
    assert!(
        errors_contain(&errors, "no_mangle"),
        "diagnostic should name the legal wrapped attributes; got {errors:?}"
    );
}

#[test]
fn test_used_attribute_stays_plain() {
    // `#[used]` only suppresses DCE — no soundness obligation, no wrap.
    let prog = parse_ok("#[used]\nfn keep() -> i64 { 7 }");
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.attributes.len(), 1);
    assert_eq!(f.attributes[0].name, "used");
}

#[test]
fn test_unsafe_no_mangle_round_trips_through_formatter() {
    // The wrap is the canonical authoring form, so formatter output
    // must re-emit `#[unsafe(no_mangle)]` (round-trip idempotence).
    let src = "#[unsafe(no_mangle)]\nfn keep_me() -> i64 {\n    42\n}\n";
    let prog = parse_ok(src);
    let formatted = karac::formatter::format_program(&prog);
    assert_eq!(formatted, src, "round-trip mismatch:\n{formatted}");
}

#[test]
fn test_unsafe_link_section_round_trips_through_formatter() {
    let src = "#[unsafe(link_section(\".init_array\"))]\nfn ctor() -> i64 {\n    1\n}\n";
    let prog = parse_ok(src);
    let formatted = karac::formatter::format_program(&prog);
    assert_eq!(formatted, src, "round-trip mismatch:\n{formatted}");
}

// ── Slice 5 of the `unsafe_op_in_unsafe_fn` epic ──────────────────────
//
// Kāra is greenfield — there is no migration story — so the rule is a
// hard error with no opt-out. `#[allow(unsafe_op_in_unsafe_fn)]` is
// rejected at the attribute parser with a focused diagnostic that
// redirects the author to the actual fix (wrap the offending operation
// in an `unsafe { ... }` block).
//
// Scope: only `#[allow]` is rejected on the rule's name. The companion
// `#[warn]` / `#[deny]` / `#[expect]` lint-level attributes are a
// separate future feature (`docs/implementation_checklist/phase-5-
// diagnostics.md` § "Lint level attributes"); when that feature lands
// it will reject all four uniformly on `unsafe_op_in_unsafe_fn` via the
// same hard-rule channel.

#[test]
fn test_allow_unsafe_op_in_unsafe_fn_rejected_with_focused_diagnostic() {
    let (_, errors) = parse_with_errors(
        "#[allow(unsafe_op_in_unsafe_fn)]\n\
         fn caller(p: *const i64) -> i64 { *p }",
    );
    assert!(
        !errors.is_empty(),
        "expected rejection of #[allow(unsafe_op_in_unsafe_fn)]"
    );
    assert!(
        errors_contain(&errors, "unsafe_op_in_unsafe_fn"),
        "diagnostic should name the rule; got {errors:?}"
    );
    assert!(
        errors_contain(&errors, "hard error"),
        "diagnostic should state the rule is a hard error; got {errors:?}"
    );
    assert!(
        errors_contain(&errors, "unsafe {"),
        "diagnostic should redirect to wrapping in `unsafe {{ ... }}`; got {errors:?}"
    );
    assert!(
        errors_contain(&errors, "// Safety:"),
        "diagnostic should mention the `// Safety:` comment per the \
         undocumented_unsafe lint; got {errors:?}"
    );
}

#[test]
fn test_allow_unsafe_op_in_unsafe_fn_rejected_on_impl_method() {
    // The rejection fires from the attribute parser, so the position the
    // attribute appears in (free fn, impl method, etc.) does not change
    // the outcome — this test pins that the parser surface is uniform.
    let (_, errors) = parse_with_errors(
        "struct S { x: i64 }\n\
         impl S {\n\
             #[allow(unsafe_op_in_unsafe_fn)]\n\
             unsafe fn raw_read(self) -> i64 { self.x }\n\
         }",
    );
    assert!(
        !errors.is_empty(),
        "expected rejection on impl-method attribute"
    );
    assert!(
        errors_contain(&errors, "unsafe_op_in_unsafe_fn"),
        "diagnostic should name the rule on impl-method site; got {errors:?}"
    );
}

#[test]
fn test_allow_other_lints_still_accepted() {
    // Slice 5 is scoped to `unsafe_op_in_unsafe_fn`. Other lint names
    // continue to parse cleanly — `#[allow(undocumented_unsafe)]` is the
    // documented suppression mechanism for the older lint and must
    // remain a no-op at parse time.
    let prog = parse_ok(
        "#[allow(undocumented_unsafe)]\n\
         fn f() { unsafe { } }",
    );
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.attributes.len(), 1);
    assert_eq!(f.attributes[0].name, "allow");
    assert_eq!(f.attributes[0].args.len(), 1);
}

#[test]
fn test_allow_with_multiple_lints_including_unsafe_op_rejected() {
    // The rejection is per-argument: if `unsafe_op_in_unsafe_fn` appears
    // anywhere in an `#[allow(...)]` list, the whole attribute is
    // rejected. Tests the iter-any branch of the check.
    let (_, errors) = parse_with_errors(
        "#[allow(undocumented_unsafe, unsafe_op_in_unsafe_fn)]\n\
         fn caller(p: *const i64) -> i64 { *p }",
    );
    assert!(
        !errors.is_empty(),
        "expected rejection when `unsafe_op_in_unsafe_fn` appears in a \
         multi-lint `#[allow(...)]`"
    );
    assert!(
        errors_contain(&errors, "unsafe_op_in_unsafe_fn"),
        "diagnostic should name the offending rule; got {errors:?}"
    );
}

#[test]
fn test_deny_unsafe_op_in_unsafe_fn_still_accepted() {
    // Slice 5 only rejects `#[allow]` on the rule. `#[deny]` (and the
    // not-yet-recognised `#[warn]` / `#[expect]`) parse as plain
    // attributes today; when the lint-level-attributes feature lands it
    // will reject all four uniformly via the hard-rule channel. Until
    // then, `#[deny(unsafe_op_in_unsafe_fn)]` is redundant (the rule is
    // already deny-by-default) but not parse-rejected — this pins that
    // current scoping decision.
    let prog = parse_ok(
        "#[deny(unsafe_op_in_unsafe_fn)]\n\
         fn caller(p: *const i64) -> i64 { unsafe { *p } }",
    );
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.attributes.len(), 1);
    assert_eq!(f.attributes[0].name, "deny");
}

#[test]
fn test_bare_extern_fn_at_module_scope_rejected() {
    // The pre-v1 shorthand `extern "C" fn name(...);` (without an enclosing
    // `unsafe extern { }` block) is removed from the grammar.
    let (_, errors) = parse_with_errors(r#"extern "C" fn write(fd: i32) -> i64;"#);
    assert!(
        !errors.is_empty(),
        "expected rejection of bare module-scope extern fn"
    );
    assert!(
        errors_contain(&errors, "unsafe extern"),
        "expected diagnostic to redirect to `unsafe extern \"ABI\" {{ ... }}` block form; got {errors:?}"
    );
}

#[test]
fn test_bare_extern_block_no_unsafe_rejected() {
    let (_, errors) = parse_with_errors(r#"extern "C" { fn foo(); }"#);
    assert!(
        !errors.is_empty(),
        "expected rejection of bare extern block (no `unsafe`)"
    );
    assert!(
        errors_contain(&errors, "unsafe extern"),
        "expected diagnostic to redirect to `unsafe extern`; got {errors:?}"
    );
}

#[test]
fn test_unsafe_fn_parses_and_records_is_unsafe() {
    // `unsafe fn` at module scope: parses, `is_unsafe` is set on the
    // resulting `Function` node. The `unsafe_op_in_unsafe_fn` lint
    // (slice 3 of the v2 unsafe epic) walks the body and enforces that
    // raw-ptr deref / unsafe-fn calls / volatile / etc. inside this
    // body are still wrapped in `unsafe { ... }` — slice 1 captures
    // only the surface marker.
    let prog = parse_ok("unsafe fn foo() {}");
    let Item::Function(f) = &prog.items[0] else {
        panic!("expected Function, got {:?}", &prog.items[0]);
    };
    assert!(f.is_unsafe, "is_unsafe should be set on `unsafe fn`");
    assert_eq!(f.name, "foo");
}

#[test]
fn test_plain_fn_has_is_unsafe_false() {
    // Regression-pin: bare `fn foo() {}` must not set `is_unsafe`.
    let prog = parse_ok("fn foo() {}");
    let Item::Function(f) = &prog.items[0] else {
        panic!("expected Function");
    };
    assert!(!f.is_unsafe, "plain fn should not be unsafe");
}

#[test]
fn test_unsafe_fn_pub_attributes_effects_roundtrip() {
    // `unsafe fn` composes with `pub`, doc comments, attributes, generics,
    // params, return type, and effect lists exactly like a plain `fn`.
    let prog = parse_ok(
        r#"
        /// Reads `n` bytes from a raw pointer.
        #[noblock]
        pub unsafe fn read_raw(p: i64, n: i64) -> i64 reads(Memory) {
            n
        }
        "#,
    );
    let Item::Function(f) = &prog.items[0] else {
        panic!("expected Function");
    };
    assert!(f.is_unsafe);
    assert!(f.is_pub);
    assert_eq!(f.name, "read_raw");
    assert_eq!(f.params.len(), 2);
    assert!(f.return_type.is_some());
    assert!(f.effects.is_some());
    assert!(f.doc_comment.is_some());
    assert_eq!(f.attributes.len(), 1);
}

#[test]
fn test_unsafe_followed_by_invalid_token_at_module_scope_rejected() {
    // `unsafe` at module scope is only valid as the prefix to either
    // an `unsafe extern "ABI" { ... }` block or an `unsafe fn` decl.
    // Any other shape is rejected with a focused diagnostic.
    let (_, errors) = parse_with_errors("unsafe struct Foo {}");
    assert!(!errors.is_empty());
    assert!(
        errors_contain(&errors, "expected `extern` or `fn` after `unsafe`"),
        "expected focused diagnostic; got {errors:?}"
    );
}

#[test]
fn test_impl_method_unsafe_fn_parses_and_records_is_unsafe() {
    // `unsafe fn` inside an impl body: parses, `is_unsafe` propagates
    // onto the underlying `Function` node. Mirrors the module-scope
    // dispatch in slice 1.
    let prog = parse_ok(
        r#"
        impl Foo {
            unsafe fn raw_get(self) -> i64 { 0 }
            fn safe_get(self) -> i64 { 1 }
        }
        "#,
    );
    let Item::ImplBlock(imp) = &prog.items[0] else {
        panic!("expected ImplBlock");
    };
    let methods = impl_methods(imp);
    assert_eq!(methods.len(), 2);
    assert!(methods[0].is_unsafe, "first method should be unsafe");
    assert_eq!(methods[0].name, "raw_get");
    assert!(!methods[1].is_unsafe, "second method should be plain");
    assert_eq!(methods[1].name, "safe_get");
}

#[test]
fn test_impl_method_unsafe_fn_with_pub_and_effects_roundtrips() {
    // Composition: `pub unsafe fn` with attributes / effects / params
    // inside an impl block — same precedence as module-scope `unsafe fn`.
    // (Per-method `///` doc comments inside impl bodies are a separate
    // pre-existing limitation: the impl-block body loop does not yet
    // call `collect_leading_doc_comments` — out of scope for this slice.)
    let prog = parse_ok(
        r#"
        impl Foo {
            #[noblock]
            pub unsafe fn read_raw(self, n: i64) -> i64 reads(Memory) {
                n
            }
        }
        "#,
    );
    let Item::ImplBlock(imp) = &prog.items[0] else {
        panic!("expected ImplBlock");
    };
    let methods = impl_methods(imp);
    assert_eq!(methods.len(), 1);
    let m = methods[0];
    assert!(m.is_unsafe);
    assert!(m.is_pub);
    assert_eq!(m.name, "read_raw");
    assert_eq!(m.attributes.len(), 1);
    assert!(m.effects.is_some());
}

#[test]
fn test_impl_method_unsafe_followed_by_invalid_token_rejected() {
    // `unsafe` in an impl body may only prefix `fn`. Any other shape is
    // a focused diagnostic, mirroring the module-scope rule.
    let (_, errors) = parse_with_errors(
        r#"
        impl Foo {
            unsafe type Bad = i64;
        }
        "#,
    );
    assert!(!errors.is_empty());
    assert!(
        errors_contain(&errors, "expected `fn` after `unsafe` in impl block"),
        "expected focused diagnostic; got {errors:?}"
    );
}

#[test]
fn test_trait_method_unsafe_fn_signature_parses_and_records_is_unsafe() {
    // Required `unsafe fn` signature in a trait body — no body, ends in
    // `;`. The `is_unsafe` field propagates onto `TraitMethod`.
    let prog = parse_ok(
        r#"
        trait RawAccess {
            unsafe fn raw_get(self, i: i64) -> i64;
            fn safe_get(self, i: i64) -> i64;
        }
        "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("expected TraitDef");
    };
    let methods = trait_methods(t);
    assert_eq!(methods.len(), 2);
    assert!(methods[0].is_unsafe, "first trait method should be unsafe");
    assert_eq!(methods[0].name, "raw_get");
    assert!(methods[0].body.is_none());
    assert!(!methods[1].is_unsafe, "second trait method should be plain");
    assert_eq!(methods[1].name, "safe_get");
}

#[test]
fn test_trait_method_unsafe_fn_with_default_body_parses() {
    // `unsafe fn` trait method with a provided default body composes
    // with effects and params. (Per-method `///` doc comments inside
    // trait bodies are a separate pre-existing limitation — out of
    // scope for this slice.)
    let prog = parse_ok(
        r#"
        trait RawAccess {
            unsafe fn read_raw(self, n: i64) -> i64 reads(Memory) {
                n
            }
        }
        "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("expected TraitDef");
    };
    let methods = trait_methods(t);
    assert_eq!(methods.len(), 1);
    let m = methods[0];
    assert!(m.is_unsafe);
    assert_eq!(m.name, "read_raw");
    assert!(m.body.is_some());
    assert!(m.effects.is_some());
}

#[test]
fn test_trait_method_unsafe_followed_by_invalid_token_rejected() {
    // `unsafe` in a trait body may only prefix `fn`. Any other shape is
    // a focused diagnostic mirroring the impl-block and module-scope rules.
    let (_, errors) = parse_with_errors(
        r#"
        trait Bad {
            unsafe type Assoc;
        }
        "#,
    );
    assert!(!errors.is_empty());
    assert!(
        errors_contain(&errors, "expected `fn` after `unsafe` in trait body"),
        "expected focused diagnostic; got {errors:?}"
    );
}

// ── Opaque foreign type declarations ──────────────────────────────────────

#[test]
fn test_extern_block_opaque_type_parses() {
    // `type Name;` inside an `unsafe extern "ABI" { ... }` block —
    // CN-8 forward-declaration form for opaque foreign types per
    // design.md § Opaque Foreign Types.
    let prog = parse_ok(
        r#"
        unsafe extern "C" {
            type File;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock, got {:?}", &prog.items[0]);
    };
    assert_eq!(b.abi, "C");
    assert_eq!(b.items.len(), 1);
    let ExternItem::OpaqueType(o) = &b.items[0] else {
        panic!("expected ExternItem::OpaqueType");
    };
    assert_eq!(o.name, "File");
    assert!(!o.is_pub);
    assert!(o.doc_comment.is_none());
}

#[test]
fn test_extern_block_opaque_type_pub_doc_attributes() {
    // Opaque-type declarations compose with `pub`, `///` doc comments,
    // and attributes exactly like other extern items.
    let prog = parse_ok(
        r#"
        unsafe extern "C" {
            /// Opaque handle to an open file.
            #[link_name = "_FILE"]
            pub type File;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock");
    };
    let ExternItem::OpaqueType(o) = &b.items[0] else {
        panic!("expected ExternItem::OpaqueType");
    };
    assert!(o.is_pub);
    assert_eq!(o.name, "File");
    assert_eq!(
        o.doc_comment.as_deref(),
        Some("Opaque handle to an open file.")
    );
    assert_eq!(o.attributes.len(), 1);
    assert_eq!(o.attributes[0].name, "link_name");
}

#[test]
fn test_extern_block_mixed_fn_and_opaque_type() {
    // Functions and opaque-type declarations may be interleaved within
    // the same block; the dispatcher routes each to the right parser.
    let prog = parse_ok(
        r#"
        unsafe extern "C" {
            type File;
            pub fn fopen(path: i64, mode: i64) -> i64;
            type Sqlite3;
            pub fn sqlite3_open(path: i64) -> i32;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock");
    };
    assert_eq!(b.items.len(), 4);
    let kinds: Vec<&'static str> = b
        .items
        .iter()
        .map(|it| match it {
            ExternItem::Function(_) => "fn",
            ExternItem::OpaqueType(_) => "type",
        })
        .collect();
    assert_eq!(kinds, vec!["type", "fn", "type", "fn"]);
}

#[test]
fn test_extern_block_opaque_type_kara_name_skips_class_check() {
    // CN-8: `#[kara_name = "..."]` rebinds the foreign symbol name
    // and should suppress the Kāra-side identifier-class check
    // (mirrors the existing `kara_name` escape on extern fn names).
    let prog = parse_ok(
        r#"
        unsafe extern "C" {
            #[kara_name = "FILE"]
            pub type File;
        }
        "#,
    );
    let Item::ExternBlock(b) = &prog.items[0] else {
        panic!("expected ExternBlock");
    };
    let ExternItem::OpaqueType(o) = &b.items[0] else {
        panic!("expected ExternItem::OpaqueType");
    };
    assert_eq!(o.name, "File");
    assert!(o.attributes.iter().any(|a| a.name == "kara_name"));
}

#[test]
fn test_extern_block_opaque_type_non_pascal_case_rejected() {
    // CN-1 + CN-8: the Kāra-visible name must be Type-class (PascalCase).
    // `FILE` (Const-class) is rejected with a focused diagnostic that
    // suggests the rebind path.
    let (_, errors) = parse_with_errors(
        r#"
        unsafe extern "C" {
            type FILE;
        }
        "#,
    );
    assert!(!errors.is_empty());
    assert!(
        errors_contain(&errors, "opaque foreign type"),
        "expected opaque-foreign-type CN-1 diagnostic; got {errors:?}"
    );
}

#[test]
fn test_extern_block_opaque_type_generics_rejected() {
    // C does not have generic types; `type Foo[T];` is a parse error
    // with a focused diagnostic suggesting a wrapper type.
    let (_, errors) = parse_with_errors(
        r#"
        unsafe extern "C" {
            type Container[T];
        }
        "#,
    );
    assert!(!errors.is_empty());
    assert!(
        errors_contain(&errors, "cannot be generic"),
        "expected focused 'cannot be generic' diagnostic; got {errors:?}"
    );
}

#[test]
fn test_extern_block_opaque_type_body_rejected() {
    // Opaque types have no fields; the `type Name { ... }` form is
    // rejected with a redirect to `#[repr(C)] struct`.
    let (_, errors) = parse_with_errors(
        r#"
        unsafe extern "C" {
            type Foo { x: i32 }
        }
        "#,
    );
    assert!(!errors.is_empty());
    assert!(
        errors_contain(&errors, "no body"),
        "expected focused 'no body' diagnostic; got {errors:?}"
    );
}

#[test]
fn test_extern_block_opaque_type_missing_semicolon_rejected() {
    // The form requires a trailing `;`. Missing semicolon → standard
    // expect-semicolon diagnostic.
    let (_, errors) = parse_with_errors(
        r#"
        unsafe extern "C" {
            type FILE
        }
        "#,
    );
    assert!(
        !errors.is_empty(),
        "expected rejection of missing semicolon"
    );
}

#[test]
fn test_extern_block_invalid_item_kind_rejected() {
    // Only `fn` and `type` are legal inside an extern block. Anything
    // else (e.g. `struct` or `const`) gets a focused dispatcher error.
    let (_, errors) = parse_with_errors(
        r#"
        unsafe extern "C" {
            const X: i32 = 0;
        }
        "#,
    );
    assert!(!errors.is_empty());
    assert!(
        errors_contain(&errors, "expected `fn` or `type`"),
        "expected focused dispatcher diagnostic; got {errors:?}"
    );
}

#[test]
fn test_alias_decl() {
    let prog = parse_ok("alias mylib.UserDB = theirlib.TheirDB;");
    if let Item::AliasDecl(a) = &prog.items[0] {
        assert_eq!(a.left, vec!["mylib", "UserDB"]);
        assert_eq!(a.right, vec!["theirlib", "TheirDB"]);
    }
}

#[test]
fn test_independent_decl() {
    let prog = parse_ok("independent mylib.UserDB, theirlib.TheirDB;");
    if let Item::IndependentDecl(i) = &prog.items[0] {
        assert_eq!(i.left, vec!["mylib", "UserDB"]);
        assert_eq!(i.right, vec!["theirlib", "TheirDB"]);
    }
}

#[test]
fn test_layout_def() {
    let prog = parse_ok(
        r#"
        layout entities: Collection[Entity] {
            group physics { position, velocity }
            group combat { health, armor, is_alive }
        }
    "#,
    );
    if let Item::LayoutDef(l) = &prog.items[0] {
        assert_eq!(l.name, "entities");
        assert_eq!(l.items.len(), 2);
        if let LayoutItem::Group { name, fields, .. } = &l.items[0] {
            assert_eq!(name, "physics");
            assert_eq!(
                fields,
                &vec!["position".to_string(), "velocity".to_string()]
            );
        }
    }
}

#[test]
fn test_layout_cold_section_parses() {
    let prog = parse_ok(
        r#"
        layout entities: Vec[Entity] {
            group physics { position, velocity }
            cold { id, name }
        }
        "#,
    );
    if let Item::LayoutDef(l) = &prog.items[0] {
        assert_eq!(l.items.len(), 2);
        assert!(
            matches!(&l.items[0], LayoutItem::Group { name, .. } if name == "physics"),
            "first item should be group physics"
        );
        if let LayoutItem::Cold { fields, .. } = &l.items[1] {
            assert_eq!(fields, &vec!["id".to_string(), "name".to_string()]);
        } else {
            panic!("second item should be cold section");
        }
    } else {
        panic!("expected LayoutDef");
    }
}

#[test]
fn test_layout_align_modifier_parses() {
    let prog = parse_ok(
        r#"
        layout entities: Vec[Entity] {
            group a { x, y } align(64)
            group b { hp } align(128)
        }
        "#,
    );
    if let Item::LayoutDef(l) = &prog.items[0] {
        if let LayoutItem::Group { name, align, .. } = &l.items[0] {
            assert_eq!(name, "a");
            assert_eq!(*align, Some(64u32));
        } else {
            panic!("expected group a");
        }
        if let LayoutItem::Group { align, .. } = &l.items[1] {
            assert_eq!(*align, Some(128u32));
        } else {
            panic!("expected group b");
        }
    } else {
        panic!("expected LayoutDef");
    }
}

#[test]
fn test_layout_group_no_align_is_none() {
    let prog = parse_ok(
        r#"
        layout entities: Vec[Entity] {
            group physics { position, velocity }
        }
        "#,
    );
    if let Item::LayoutDef(l) = &prog.items[0] {
        if let LayoutItem::Group { align, .. } = &l.items[0] {
            assert_eq!(*align, None, "group without align should have None");
        }
    }
}

#[test]
fn test_layout_def_accepts_pub_and_attributes() {
    let prog = parse_ok(
        r#"
        #[no_rc]
        pub layout entities: Collection[Entity] {
            group physics { position, velocity }
        }
        "#,
    );
    if let Item::LayoutDef(l) = &prog.items[0] {
        assert!(l.is_pub, "pub should be threaded to LayoutDef");
        assert_eq!(l.attributes.len(), 1, "attribute should be captured");
        assert_eq!(l.attributes[0].name, "no_rc");
        assert_eq!(l.name, "entities");
    } else {
        panic!("expected LayoutDef");
    }
}

#[test]
fn test_struct_field_accepts_attributes() {
    let prog = parse_ok(
        r#"
        struct Packet {
            #[must_use]
            pub header: u32,
            body: u64,
        }
        "#,
    );
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.fields.len(), 2);
        assert_eq!(s.fields[0].attributes.len(), 1);
        assert_eq!(s.fields[0].attributes[0].name, "must_use");
        assert_eq!(s.fields[0].name, "header");
        assert_eq!(s.fields[1].attributes.len(), 0);
        assert_eq!(s.fields[1].name, "body");
    } else {
        panic!("expected StructDef");
    }
}

// ── 2.7: AST Design ─────────────────────────────────────────────

#[test]
fn test_span_tracking() {
    let prog = parse_ok("fn main() { }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.span.line, 1);
        assert_eq!(f.span.column, 1);
    }
}

#[test]
fn test_multiline_span_tracking() {
    let prog = parse_ok("fn main() {\n    let x = 5;\n}");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.body.stmts[0].span.line, 2);
    }
}

#[test]
fn test_struct_span_tracking() {
    let prog = parse_ok("struct Foo {\n    x: i64,\n    y: bool,\n}");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.span.line, 1);
        assert_eq!(s.span.column, 1);
        assert_eq!(s.fields[0].span.line, 2);
        assert_eq!(s.fields[1].span.line, 3);
    } else {
        panic!("Expected struct");
    }
}

#[test]
fn test_expr_span_tracking() {
    let prog = parse_ok("fn f() {\n    let x = 1 + 2;\n}");
    if let Item::Function(f) = &prog.items[0] {
        let stmt = &f.body.stmts[0];
        assert_eq!(stmt.span.line, 2);
        if let StmtKind::Let { value, .. } = &stmt.kind {
            assert_eq!(value.span.line, 2);
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_enum_variant_span_tracking() {
    let prog = parse_ok("enum Color {\n    Red,\n    Green,\n    Blue,\n}");
    if let Item::EnumDef(e) = &prog.items[0] {
        assert_eq!(e.span.line, 1);
        assert_eq!(e.variants[0].span.line, 2);
        assert_eq!(e.variants[1].span.line, 3);
        assert_eq!(e.variants[2].span.line, 4);
    } else {
        panic!("Expected enum");
    }
}

// ── 2.8: Diagnostics ────────────────────────────────────────────

#[test]
fn test_error_recovery_missing_semicolon() {
    let (prog, errors) = parse_with_errors("fn main() { let x = 5 }");
    assert!(!errors.is_empty());
    // Parser should still produce partial results
    assert!(!prog.items.is_empty());
}

#[test]
fn test_error_recovery_multiple_items() {
    let (prog, errors) = parse_with_errors(
        r#"
        fn good1() { }
        fn bad( { }
        fn good2() { }
    "#,
    );
    // Should recover and parse good2 even after bad fails
    assert!(!errors.is_empty());
    // At least good1 should be parsed
    assert!(!prog.items.is_empty());
}

#[test]
fn test_error_unexpected_token() {
    let (_, errors) = parse_with_errors("fn 123() { }");
    assert!(!errors.is_empty());
    assert!(errors[0].message.contains("Expected identifier"));
}

#[test]
fn test_error_span_points_to_bad_token() {
    let (_, errors) = parse_with_errors("fn foo() {\n    let x = ;\n}");
    assert!(!errors.is_empty());
    // Error should point to line 2 where the bad token is
    assert_eq!(errors[0].span.line, 2);
}

#[test]
fn test_multiple_errors_collected() {
    let (_, errors) = parse_with_errors(
        r#"
        fn a( { }
        fn b( { }
    "#,
    );
    // Should report errors from both bad functions, not just the first
    assert!(errors.len() >= 2);
}

#[test]
fn test_error_recovery_continues_after_bad_struct() {
    let (prog, errors) = parse_with_errors(
        r#"
        struct Bad {
            x: ,
        }
        fn good() { }
    "#,
    );
    assert!(!errors.is_empty());
    // Should recover and parse the function after the bad struct
    let has_fn = prog
        .items
        .iter()
        .any(|item| matches!(item, Item::Function(_)));
    assert!(has_fn, "Should recover and parse fn after bad struct");
}

// ── Integration: word_count.kara ─────────────────────────────────

#[test]
fn test_word_count_example() {
    let source = r#"
        struct WordCount {
            total: u64,
            unique: u64,
        }

        fn count_words(content: ref String) -> WordCount {
            let words = content.split(" ");
            let unique_words = HashSet.from(words);
            WordCount {
                total: words.len(),
                unique: unique_words.len(),
            }
        }

        fn format_report(filename: ref String, count: ref WordCount) -> String {
            format("{}: {} total words, {} unique", filename, count.total, count.unique)
        }

        fn main() with reads(Env) reads(FileSystem) writes(FileSystem) {
            let args = env.args();

            let filename = match args.get(1) {
                Some(f) => f,
                None => {
                    println("Usage: word_count <filename>");
                    return;
                },
            };

            let content = read_file(filename)?;
            let count = count_words(content);
            let report = format_report(filename, count);
            println(report);
        }
    "#;

    let result = parse(source);
    if !result.errors.is_empty() {
        for e in &result.errors {
            eprintln!("  Parse error: {}", e);
        }
        panic!("word_count.kara should parse without errors");
    }

    let prog = result.program;
    assert_eq!(prog.items.len(), 4);

    // Struct
    assert!(matches!(&prog.items[0], Item::StructDef(s) if s.name == "WordCount"));

    // count_words function
    if let Item::Function(f) = &prog.items[1] {
        assert_eq!(f.name, "count_words");
        assert!(f.effects.is_none()); // pure function
    }

    // format_report function
    if let Item::Function(f) = &prog.items[2] {
        assert_eq!(f.name, "format_report");
    }

    // main function with effects
    if let Item::Function(f) = &prog.items[3] {
        assert_eq!(f.name, "main");
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 3); // reads(Env), reads(FileSystem), writes(FileSystem)
    }
}

// ── Type parsing ─────────────────────────────────────────────────

#[test]
fn test_array_type() {
    parse_ok("fn main() { let arr: Array[f32, 4] = x; }");
}

#[test]
fn test_tuple_type() {
    parse_ok("fn main() { let t: (i64, String, bool) = x; }");
}

#[test]
fn test_unit_type() {
    parse_ok("fn main() { let u: () = x; }");
}

#[test]
fn test_generic_type() {
    parse_ok("fn main() { let v: Vec[i64] = x; }");
    parse_ok("fn main() { let m: HashMap[String, i64] = x; }");
}

#[test]
fn test_nested_generic_type() {
    parse_ok("fn main() { let v: Result[Option[i64], Error] = x; }");
}

#[test]
fn test_pointer_type() {
    parse_ok("fn main() { let p: *const i64 = x; let q: *mut u8 = y; }");
}

#[test]
fn test_path_expression() {
    parse_ok("fn main() { let x = HashMap.new(); }");
    parse_ok("fn main() { let x = std.env.args(); }");
}

// ── Pattern matching ─────────────────────────────────────────────

#[test]
fn test_wildcard_pattern() {
    parse_ok("fn main() { let _ = foo(); }");
}

#[test]
fn test_match_wildcard_arm() {
    parse_ok(
        r#"
        fn main() {
            match x {
                1 => a,
                _ => b,
            };
        }
    "#,
    );
}

#[test]
fn test_struct_destructure_pattern() {
    parse_ok(
        r#"
        fn main() {
            let Point { x, y } = point;
        }
    "#,
    );
}

#[test]
fn test_char_literal() {
    let prog = parse_ok("fn main() { let c = 'a'; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(value.kind, ExprKind::CharLit('a')));
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_char_literal_escape() {
    let prog = parse_ok("fn main() { let c = '\\n'; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(value.kind, ExprKind::CharLit('\n')));
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_char_literal_in_match_pattern() {
    parse_ok(
        r#"
        fn main() {
            let x = 'a';
            match x {
                'a' => { let y = 1; },
                'b' => { let y = 2; },
                _ => { let y = 0; },
            };
        }
    "#,
    );
}

#[test]
fn test_array_literal_empty() {
    let program = parse_ok("fn main() { let x = []; }");
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(value.kind, ExprKind::ArrayLiteral(ref elems) if elems.is_empty()));
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_array_literal_integers() {
    let program = parse_ok("fn main() { let x = [1, 2, 3]; }");
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::ArrayLiteral(elems) = &value.kind {
                assert_eq!(elems.len(), 3);
                assert!(matches!(elems[0].kind, ExprKind::Integer(1, _)));
                assert!(matches!(elems[1].kind, ExprKind::Integer(2, _)));
                assert!(matches!(elems[2].kind, ExprKind::Integer(3, _)));
            } else {
                panic!("Expected array literal");
            }
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_array_literal_trailing_comma() {
    let program = parse_ok("fn main() { let x = [1, 2,]; }");
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::ArrayLiteral(elems) = &value.kind {
                assert_eq!(elems.len(), 2);
            } else {
                panic!("Expected array literal");
            }
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_array_literal_single_element() {
    let program = parse_ok(r#"fn main() { let x = ["hello"]; }"#);
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::ArrayLiteral(elems) = &value.kind {
                assert_eq!(elems.len(), 1);
            } else {
                panic!("Expected array literal");
            }
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_array_literal_nested() {
    let program = parse_ok("fn main() { let x = [[1, 2], [3, 4]]; }");
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::ArrayLiteral(elems) = &value.kind {
                assert_eq!(elems.len(), 2);
                assert!(matches!(elems[0].kind, ExprKind::ArrayLiteral(_)));
                assert!(matches!(elems[1].kind, ExprKind::ArrayLiteral(_)));
            } else {
                panic!("Expected array literal");
            }
        } else {
            panic!("Expected let statement");
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_repeat_literal_bare() {
    // `[v; n]` parses as RepeatLiteral with type_name == None.
    let program = parse_ok("fn main() { let x = [0; 8]; }");
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::RepeatLiteral {
                type_name,
                value: v,
                count: c,
            } = &value.kind
            {
                assert!(type_name.is_none());
                assert!(matches!(v.kind, ExprKind::Integer(0, _)));
                assert!(matches!(c.kind, ExprKind::Integer(8, _)));
            } else {
                panic!("Expected RepeatLiteral, got: {:?}", value.kind);
            }
        }
    }
}

#[test]
fn test_repeat_literal_array_prefix() {
    // `Array[v; n]` parses as RepeatLiteral with type_name == Some("Array").
    let program = parse_ok("fn main() { let x = Array[0; 256]; }");
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::RepeatLiteral { type_name, .. } = &value.kind {
                assert_eq!(type_name.as_deref(), Some("Array"));
            } else {
                panic!("Expected RepeatLiteral");
            }
        }
    }
}

#[test]
fn test_repeat_literal_vec_prefix() {
    let program = parse_ok("fn main() { let x = Vec[42; 100]; }");
    if let Item::Function(f) = &program.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::RepeatLiteral { type_name, .. } = &value.kind {
                assert_eq!(type_name.as_deref(), Some("Vec"));
            } else {
                panic!("Expected RepeatLiteral");
            }
        }
    }
}

// ── Where Clauses ───────────────────────────────────────────────

#[test]
fn test_where_clause_on_function() {
    let prog = parse_ok("fn merge[T, U](a: T, b: U) -> T where T: Ord + Clone, U: From[T] { a }");
    if let Item::Function(f) = &prog.items[0] {
        let wc = f.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 2);
        let (tn0, b0) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn0, "T");
        assert_eq!(b0.len(), 2);
        assert_eq!(b0[0].path, vec!["Ord"]);
        assert_eq!(b0[1].path, vec!["Clone"]);
        let (tn1, b1) = where_type_bound(&wc.constraints[1]);
        assert_eq!(tn1, "U");
        assert_eq!(b1[0].path, vec!["From"]);
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_where_clause_on_function_with_effects() {
    // Declaration order: effects → where → body
    let prog =
        parse_ok("fn search[T](h: Vec[T], n: T) -> Option[i64] reads(Index) where T: Ord { None }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.effects.is_some());
        let wc = f.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        let (tn, b) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn, "T");
        assert_eq!(b[0].path, vec!["Ord"]);
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_where_clause_on_struct() {
    let prog = parse_ok("struct SortedPair[T] where T: Ord { first: T, second: T, }");
    if let Item::StructDef(s) = &prog.items[0] {
        let wc = s.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        let (tn, b) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn, "T");
        assert_eq!(b[0].path, vec!["Ord"]);
        assert_eq!(s.fields.len(), 2);
    } else {
        panic!("Expected struct");
    }
}

#[test]
fn test_where_clause_on_enum() {
    let prog = parse_ok("enum Wrapper[T] where T: Clone { Some(T), None, }");
    if let Item::EnumDef(e) = &prog.items[0] {
        let wc = e.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        let (tn, b) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn, "T");
        assert_eq!(b[0].path, vec!["Clone"]);
        assert_eq!(e.variants.len(), 2);
    } else {
        panic!("Expected enum");
    }
}

#[test]
fn test_where_clause_on_impl_block() {
    let prog = parse_ok(
        "impl[T] Display for Vec[T] where T: Display { fn fmt(self) -> String { todo() } }",
    );
    if let Item::ImplBlock(imp) = &prog.items[0] {
        let wc = imp.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        let (tn, b) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn, "T");
        assert_eq!(b[0].path, vec!["Display"]);
    } else {
        panic!("Expected impl block");
    }
}

#[test]
fn test_where_clause_on_trait() {
    let prog = parse_ok("trait Sortable[T] where T: Ord { fn sort(self) -> Vec[T]; }");
    if let Item::TraitDef(t) = &prog.items[0] {
        let wc = t.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        let (tn, _) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn, "T");
    } else {
        panic!("Expected trait");
    }
}

#[test]
fn test_where_clause_mixed_inline_and_where() {
    // Inline bound on T, overflow bound on U in where clause
    let prog =
        parse_ok("fn process[T: Ord, U](items: Vec[T]) -> U where U: From[T] + Display { todo() }");
    if let Item::Function(f) = &prog.items[0] {
        let gp = f.generic_params.as_ref().unwrap();
        assert_eq!(gp.params[0].bounds.len(), 1); // T: Ord inline
        assert_eq!(gp.params[1].bounds.len(), 0); // U has no inline bounds
        let wc = f.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        let (tn, b) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn, "U");
        assert_eq!(b.len(), 2); // From<T> + Display
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_no_where_clause() {
    let prog = parse_ok("fn simple(x: i64) -> i64 { x }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.where_clause.is_none());
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_where_clause_multiple_bounds_on_same_param() {
    let prog = parse_ok("fn f[T]() where T: Ord + Clone + Display { }");
    if let Item::Function(f) = &prog.items[0] {
        let wc = f.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        let (_, b) = where_type_bound(&wc.constraints[0]);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].path, vec!["Ord"]);
        assert_eq!(b[1].path, vec!["Clone"]);
        assert_eq!(b[2].path, vec!["Display"]);
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_parser_where_clause_const_predicate() {
    // `where N >= 0` — const-expression predicate over a const-generic
    // param. Falls through `parse_optional_where_clause`'s `.`/`:` branches
    // to the const-predicate backtrack (const generics slice 1, sub-step b).
    // Slice 2's evaluator + slice 3's discharge engine consume; slice 1
    // only parses + resolves.
    let prog = parse_ok("fn f[const N: i64](x: i64) -> i64 where N >= 0 { x + N }");
    if let Item::Function(f) = &prog.items[0] {
        let wc = f.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        match &wc.constraints[0] {
            WhereConstraint::ConstPredicate { expr, .. } => match &expr.kind {
                ExprKind::Binary { op, .. } => {
                    assert!(matches!(op, BinOp::GtEq));
                }
                other => panic!(
                    "Expected ConstPredicate to wrap a Binary expression, got {:?}",
                    other
                ),
            },
            other => panic!("Expected WhereConstraint::ConstPredicate, got {:?}", other),
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_parser_call_site_explicit_generic_args() {
    // Const generics slice 1b: `make_arr[i64, 4]()` parses as a Call
    // whose callee is `Path { segments: ["make_arr"], generic_args:
    // Some([Type(i64), Const(Integer(4))]) }`. The bracket contents
    // have a `,` separator + the matching `]` is followed by `(`,
    // tripping the `lookahead_generic_args_call` disambiguation that
    // routes through Path rather than Index.
    let prog = parse_ok(
        "fn make_arr[T, const N: i64]() -> i64 { 42 }\n\
         fn main() { make_arr[i64, 4](); }",
    );
    let Item::Function(main_fn) = &prog.items[1] else {
        panic!("Expected `main` as the second item");
    };
    // The first statement in main is the `make_arr[i64, 4]()` call.
    let Stmt {
        kind: StmtKind::Expr(call_expr),
        ..
    } = &main_fn.body.stmts[0]
    else {
        panic!("Expected an expression statement");
    };
    let ExprKind::Call { callee, .. } = &call_expr.kind else {
        panic!("Expected a Call expression");
    };
    let ExprKind::Path {
        segments,
        generic_args,
    } = &callee.kind
    else {
        panic!("Expected a Path callee, got {:?}", callee.kind);
    };
    assert_eq!(segments, &["make_arr".to_string()]);
    let ga = generic_args.as_ref().expect("expected generic args");
    assert_eq!(ga.len(), 2);
    assert!(matches!(&ga[0], GenericArg::Type(_)));
    match &ga[1] {
        GenericArg::Const(e) => match &e.kind {
            ExprKind::Integer(4, _) => {}
            other => panic!("Expected Integer(4), got {:?}", other),
        },
        other => panic!("Expected Const arg at index 1, got {:?}", other),
    }
}

#[test]
fn test_parser_indexed_call_still_parses_as_index_then_call() {
    // Regression: the slice-1b generic-args-call disambiguation only
    // fires when bracket contents contain a top-level `,`. Single-arg
    // brackets stay as `Index` so `callbacks[0]()` keeps working
    // (`tests/interpreter.rs` exercises this shape end-to-end).
    let prog = parse_ok("fn main() { let _ = callbacks[0](); }");
    let Item::Function(main_fn) = &prog.items[0] else {
        panic!("Expected Function");
    };
    let Stmt {
        kind: StmtKind::Let { value, .. },
        ..
    } = &main_fn.body.stmts[0]
    else {
        panic!("Expected let");
    };
    let ExprKind::Call { callee, .. } = &value.kind else {
        panic!("Expected outer Call (the trailing `()`)");
    };
    assert!(
        matches!(&callee.kind, ExprKind::Index { .. }),
        "Expected the call's callee to be Index, got {:?}",
        callee.kind
    );
}

// ── Distinct Type Declarations ──────────────────────────────────

#[test]
fn test_distinct_type_basic() {
    let prog = parse_ok("distinct type UserId = i64;");
    if let Item::DistinctType(d) = &prog.items[0] {
        assert_eq!(d.name, "UserId");
        assert!(!d.is_pub);
        assert!(d.generic_params.is_none());
        assert!(d.refinement.is_none());
        assert!(d.attributes.is_empty());
    } else {
        panic!("Expected DistinctType");
    }
}

#[test]
fn test_distinct_type_pub() {
    let prog = parse_ok("pub distinct type UserId = i64;");
    if let Item::DistinctType(d) = &prog.items[0] {
        assert_eq!(d.name, "UserId");
        assert!(d.is_pub);
    } else {
        panic!("Expected DistinctType");
    }
}

#[test]
fn test_distinct_type_with_derive() {
    let prog = parse_ok("#[derive(Eq, Hash)] distinct type UserId = u64;");
    if let Item::DistinctType(d) = &prog.items[0] {
        assert_eq!(d.name, "UserId");
        assert_eq!(d.attributes.len(), 1);
        assert_eq!(d.attributes[0].name, "derive");
        assert_eq!(d.attributes[0].args.len(), 2);
        // `#[derive(Eq, Hash)]` — positional arg whose value is the bare
        // trait-name identifier. The parser records `name = None` and
        // `value = Some(Identifier)`.
        let name_of = |arg: &AttrArg| -> Option<String> {
            arg.value.as_ref().and_then(|v| match &v.kind {
                ExprKind::Identifier(n) => Some(n.clone()),
                _ => None,
            })
        };
        assert_eq!(name_of(&d.attributes[0].args[0]).as_deref(), Some("Eq"));
        assert_eq!(name_of(&d.attributes[0].args[1]).as_deref(), Some("Hash"));
    } else {
        panic!("Expected DistinctType");
    }
}

#[test]
fn test_distinct_type_with_generic_params() {
    let prog = parse_ok("distinct type Wrapper[T] = T;");
    if let Item::DistinctType(d) = &prog.items[0] {
        assert_eq!(d.name, "Wrapper");
        assert!(d.generic_params.is_some());
        let gp = d.generic_params.as_ref().unwrap();
        assert_eq!(gp.params.len(), 1);
        assert_eq!(gp.params[0].name, "T");
    } else {
        panic!("Expected DistinctType");
    }
}

#[test]
fn test_distinct_type_with_refinement() {
    let prog = parse_ok("distinct type ValidPort = u16 where self >= 1;");
    if let Item::DistinctType(d) = &prog.items[0] {
        assert_eq!(d.name, "ValidPort");
        assert!(d.refinement.is_some());
    } else {
        panic!("Expected DistinctType");
    }
}

#[test]
fn test_distinct_type_multiple() {
    let prog = parse_ok(
        "distinct type UserId = i64;\n\
         distinct type PostId = i64;",
    );
    assert_eq!(prog.items.len(), 2);
    if let Item::DistinctType(d) = &prog.items[0] {
        assert_eq!(d.name, "UserId");
    } else {
        panic!("Expected DistinctType");
    }
    if let Item::DistinctType(d) = &prog.items[1] {
        assert_eq!(d.name, "PostId");
    } else {
        panic!("Expected DistinctType");
    }
}

#[test]
fn test_distinct_type_error_missing_type_keyword() {
    let (_prog, errors) = parse_with_errors("distinct Foo = i64;");
    assert!(!errors.is_empty());
}

// ── Contracts: requires / ensures ───────────────────────────────

#[test]
fn test_requires_basic() {
    let prog = parse_ok("fn clamp(x: i32) -> i32 requires x > 0 { x }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.name, "clamp");
        assert_eq!(f.requires.len(), 1);
        assert!(f.ensures.is_empty());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_ensures_basic() {
    let prog = parse_ok("fn abs(x: i32) -> i32 ensures |result| result >= 0 { x }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.name, "abs");
        assert!(f.requires.is_empty());
        assert_eq!(f.ensures.len(), 1);
        assert_eq!(f.ensures[0].param.as_deref(), Some("result"));
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_requires_and_ensures() {
    let src = r#"
        fn process(id: i64) -> Report
            requires id > 0
            ensures |result| result.len() > 0
        { id }
    "#;
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.requires.len(), 1);
        assert_eq!(f.ensures.len(), 1);
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_requires_with_effects() {
    let src = r#"
        fn fetch(id: i64) -> Data
            reads(UserDB)
            requires id > 0
        { id }
    "#;
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.effects.is_some());
        assert_eq!(f.requires.len(), 1);
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_ensures_without_param() {
    let prog = parse_ok("fn always_true() -> bool ensures true { true }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.ensures.len(), 1);
        assert!(f.ensures[0].param.is_none());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_contracts_with_where_clause() {
    let src = r#"
        fn find[T](items: Vec[T], needle: T) -> Option[T]
            requires items.len() > 0
            ensures |result| true
            where T: Eq
        { needle }
    "#;
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.requires.len(), 1);
        assert_eq!(f.ensures.len(), 1);
        assert!(f.where_clause.is_some());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_no_contracts() {
    let prog = parse_ok("fn simple(x: i32) -> i32 { x }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.requires.is_empty());
        assert!(f.ensures.is_empty());
    } else {
        panic!("Expected Function");
    }
}

// ── Contracts: invariant on structs ─────────────────────────────

#[test]
fn test_struct_invariant_basic() {
    let src = r#"
        struct DateRange {
            start: i64,
            end: i64,
            invariant self.start <= self.end
        }
    "#;
    let prog = parse_ok(src);
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.name, "DateRange");
        assert_eq!(s.fields.len(), 2);
        assert_eq!(s.invariants.len(), 1);
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_struct_multiple_invariants() {
    let src = r#"
        struct BoundedRange {
            min: i64,
            max: i64,
            value: i64,
            invariant self.min <= self.max
            invariant self.value >= self.min and self.value <= self.max
        }
    "#;
    let prog = parse_ok(src);
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.fields.len(), 3);
        assert_eq!(s.invariants.len(), 2);
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_struct_no_invariant() {
    let prog = parse_ok("struct Point { x: f64, y: f64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.fields.len(), 2);
        assert!(s.invariants.is_empty());
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_struct_invariant_with_method_call() {
    let src = r#"
        struct SortedVec {
            data: Vec[i32],
            invariant self.data.is_sorted()
        }
    "#;
    let prog = parse_ok(src);
    if let Item::StructDef(s) = &prog.items[0] {
        assert_eq!(s.fields.len(), 1);
        assert_eq!(s.invariants.len(), 1);
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_trait_method_with_contracts() {
    let src = r#"
        trait Searchable {
            fn find(self, key: i32) -> Option[i32]
                requires key >= 0;
        }
    "#;
    let prog = parse_ok(src);
    if let Item::TraitDef(t) = &prog.items[0] {
        let methods = trait_methods(t);
        assert_eq!(methods[0].requires.len(), 1);
        assert!(methods[0].ensures.is_empty());
    } else {
        panic!("Expected TraitDef");
    }
}

// ── Default parameter values ────────────────────────────────────

#[test]
fn test_default_param_basic() {
    let prog = parse_ok("fn serve(port: u16 = 8080) { port }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 1);
        assert_eq!(f.params[0].name(), Some("port"));
        assert!(f.params[0].default_value.is_some());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_default_param_multiple() {
    let src = r#"
        fn create_server(
            host: String,
            port: u16 = 8080,
            max_connections: i64 = 1000,
            timeout_ms: i64 = 5000,
        ) -> Server { host }
    "#;
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 4);
        assert!(f.params[0].default_value.is_none());
        assert!(f.params[1].default_value.is_some());
        assert!(f.params[2].default_value.is_some());
        assert!(f.params[3].default_value.is_some());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_default_param_no_defaults() {
    let prog = parse_ok("fn add(a: i32, b: i32) -> i32 { a }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 2);
        assert!(f.params[0].default_value.is_none());
        assert!(f.params[1].default_value.is_none());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_default_param_expression() {
    let prog = parse_ok("fn timeout(ms: i64 = 60 * 1000) { ms }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 1);
        assert!(f.params[0].default_value.is_some());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_default_param_bool_literal() {
    let prog = parse_ok("fn connect(verbose: bool = false) { verbose }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 1);
        assert!(f.params[0].default_value.is_some());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_default_param_mixed_with_contracts() {
    let src = r#"
        fn fetch(url: String, retries: i32 = 3)
            requires retries >= 0
        { url }
    "#;
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert!(f.params[0].default_value.is_none());
        assert!(f.params[1].default_value.is_some());
        assert_eq!(f.requires.len(), 1);
    } else {
        panic!("Expected Function");
    }
}

// ── Destructuring in function/closure parameters ─────────────────

#[test]
fn test_tuple_destructuring_param() {
    let prog = parse_ok("fn add((a, b): (i64, i64)) -> i64 { a }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 1);
        assert!(f.params[0].name().is_none()); // not a simple binding
        match &f.params[0].pattern.kind {
            PatternKind::Tuple(pats) => {
                assert_eq!(pats.len(), 2);
                assert!(matches!(&pats[0].kind, PatternKind::Binding(n) if n == "a"));
                assert!(matches!(&pats[1].kind, PatternKind::Binding(n) if n == "b"));
            }
            _ => panic!("Expected tuple pattern"),
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_struct_destructuring_param() {
    let src = "fn get_x(Point { x, y }: Point) -> i64 { x }";
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 1);
        match &f.params[0].pattern.kind {
            PatternKind::Struct { path, fields } => {
                assert_eq!(path, &vec!["Point".to_string()]);
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "x");
                assert_eq!(fields[1].name, "y");
            }
            _ => panic!("Expected struct pattern"),
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_struct_destructuring_param_with_rename() {
    let src =
        "fn distance(Point { x: x1, y: y1 }: Point, Point { x: x2, y: y2 }: Point) -> f64 { x1 }";
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 2);
        for param in &f.params {
            match &param.pattern.kind {
                PatternKind::Struct { path, fields } => {
                    assert_eq!(path, &vec!["Point".to_string()]);
                    assert_eq!(fields.len(), 2);
                    // Each field has a sub-pattern renaming it
                    assert!(fields[0].pattern.is_some());
                    assert!(fields[1].pattern.is_some());
                }
                _ => panic!("Expected struct pattern"),
            }
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_wildcard_destructuring_param() {
    let prog = parse_ok("fn y_only((_, y): (i64, i64)) -> i64 { y }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 1);
        match &f.params[0].pattern.kind {
            PatternKind::Tuple(pats) => {
                assert_eq!(pats.len(), 2);
                assert!(matches!(&pats[0].kind, PatternKind::Wildcard));
                assert!(matches!(&pats[1].kind, PatternKind::Binding(n) if n == "y"));
            }
            _ => panic!("Expected tuple pattern"),
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_mixed_regular_and_destructuring_params() {
    let src = "fn foo(name: String, (a, b): (i64, i64)) -> i64 { a }";
    let prog = parse_ok(src);
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].name(), Some("name"));
        assert!(f.params[0].name().is_some());
        assert!(f.params[1].name().is_none());
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_closure_tuple_destructuring() {
    let prog = parse_ok("fn main() { let f = |(a, b)| a; }");
    if let Item::Function(f) = &prog.items[0] {
        // Just check it parses without error
        assert_eq!(f.name, "main");
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_closure_tuple_destructuring_with_type() {
    let prog = parse_ok("fn main() { let f = |(a, b): (i64, i64)| a; }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.name, "main");
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_nested_tuple_destructuring_param() {
    let prog = parse_ok("fn nested(((a, b), c): ((i64, i64), i64)) -> i64 { a }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.params.len(), 1);
        match &f.params[0].pattern.kind {
            PatternKind::Tuple(pats) => {
                assert_eq!(pats.len(), 2);
                assert!(matches!(&pats[0].kind, PatternKind::Tuple(_)));
                assert!(matches!(&pats[1].kind, PatternKind::Binding(n) if n == "c"));
            }
            _ => panic!("Expected tuple pattern"),
        }
    } else {
        panic!("Expected Function");
    }
}

// ── Named / Labeled Arguments ────────────────────────���─────────

#[test]
fn test_labeled_arg_basic() {
    let prog = parse_ok("fn main() { foo(x: 1, y: 2); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::Call { args, .. } = &expr.kind {
                assert_eq!(args.len(), 2);
                assert_eq!(args[0].label.as_deref(), Some("x"));
                assert_eq!(args[1].label.as_deref(), Some("y"));
            } else {
                panic!("Expected Call");
            }
        } else {
            panic!("Expected Expr statement");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_labeled_arg_mixed_positional_and_labeled() {
    let prog = parse_ok("fn main() { foo(1, y: 2, z: 3); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::Call { args, .. } = &expr.kind {
                assert_eq!(args.len(), 3);
                assert!(args[0].label.is_none());
                assert_eq!(args[1].label.as_deref(), Some("y"));
                assert_eq!(args[2].label.as_deref(), Some("z"));
            } else {
                panic!("Expected Call");
            }
        } else {
            panic!("Expected Expr statement");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_labeled_arg_all_positional_unchanged() {
    let prog = parse_ok("fn main() { foo(1, 2); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::Call { args, .. } = &expr.kind {
                assert_eq!(args.len(), 2);
                assert!(args[0].label.is_none());
                assert!(args[1].label.is_none());
            } else {
                panic!("Expected Call");
            }
        } else {
            panic!("Expected Expr statement");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_labeled_arg_with_complex_expr() {
    let prog = parse_ok("fn main() { foo(x: 1 + 2); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::Call { args, .. } = &expr.kind {
                assert_eq!(args.len(), 1);
                assert_eq!(args[0].label.as_deref(), Some("x"));
                assert!(matches!(&args[0].value.kind, ExprKind::Binary { .. }));
            } else {
                panic!("Expected Call");
            }
        } else {
            panic!("Expected Expr statement");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_labeled_arg_method_call() {
    let prog = parse_ok("fn main() { obj.method(x: 1, y: 2); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::MethodCall { args, .. } = &expr.kind {
                assert_eq!(args.len(), 2);
                assert_eq!(args[0].label.as_deref(), Some("x"));
                assert_eq!(args[1].label.as_deref(), Some("y"));
            } else {
                panic!("Expected MethodCall");
            }
        } else {
            panic!("Expected Expr statement");
        }
    } else {
        panic!("Expected Function");
    }
}

// ── Pipe Operator ──────────────────────────────────────────────

#[test]
fn test_pipe_placeholder_in_call() {
    let prog = parse_ok("fn main() { let x = data |> filter(_, pred); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Pipe { right, .. } = &value.kind {
                if let ExprKind::Call { args, .. } = &right.kind {
                    assert_eq!(args.len(), 2);
                    assert!(matches!(&args[0].value.kind, ExprKind::PipePlaceholder));
                } else {
                    panic!("Expected Call on RHS of pipe");
                }
            } else {
                panic!("Expected Pipe");
            }
        } else {
            panic!("Expected let");
        }
    }
}

#[test]
fn test_pipe_chained_with_placeholder() {
    let prog = parse_ok("fn main() { let x = data |> filter(_, pred) |> map(_, transform); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            // Should be: (data |> filter(_, pred)) |> map(_, transform)
            if let ExprKind::Pipe { left, right } = &value.kind {
                assert!(matches!(&left.kind, ExprKind::Pipe { .. }));
                assert!(matches!(&right.kind, ExprKind::Call { .. }));
            } else {
                panic!("Expected Pipe");
            }
        }
    }
}

#[test]
fn test_pipe_precedence_over_binary() {
    // x + 1 |> f should parse as (x + 1) |> f
    let prog = parse_ok("fn main() { let r = x + 1 |> f; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Pipe { left, right } = &value.kind {
                assert!(matches!(&left.kind, ExprKind::Binary { .. }));
                assert!(matches!(&right.kind, ExprKind::Identifier(_)));
            } else {
                panic!("Expected Pipe, got {:?}", value.kind);
            }
        }
    }
}

#[test]
fn test_pipe_bare_function_name() {
    let prog = parse_ok("fn main() { let x = data |> normalize; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Pipe { left, right } = &value.kind {
                assert!(matches!(&left.kind, ExprKind::Identifier(_)));
                assert!(matches!(&right.kind, ExprKind::Identifier(_)));
            } else {
                panic!("Expected Pipe");
            }
        }
    }
}

// ── Phase 2.1 Updates: New Constructs ───────────────────────────

#[test]
fn test_compound_assign_plus() {
    let prog = parse_ok("fn main() { let mut x = 0; x += 1; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::CompoundAssign { op, .. } = &f.body.stmts[1].kind {
            assert_eq!(*op, CompoundOp::Add);
        } else {
            panic!("Expected CompoundAssign");
        }
    }
}

#[test]
fn test_compound_assign_all_ops() {
    parse_ok("fn main() { let mut x = 0; x += 1; x -= 1; x *= 2; x /= 2; x %= 3; }");
    parse_ok("fn main() { let mut x = 0; x &= 1; x |= 2; x ^= 3; x <<= 1; x >>= 1; }");
}

#[test]
fn test_defer_block() {
    parse_ok("fn main() { defer { cleanup(); } }");
}

#[test]
fn test_defer_expr() {
    let prog = parse_ok("fn main() { defer cleanup(); }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(matches!(&f.body.stmts[0].kind, StmtKind::Defer { .. }));
    }
}

#[test]
fn test_errdefer_paren_binding() {
    let prog = parse_ok("fn main() { errdefer(e) { log_error(e); } }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::ErrDefer { binding, .. } = &f.body.stmts[0].kind {
            assert_eq!(binding.as_deref(), Some("e"));
        } else {
            panic!("Expected ErrDefer");
        }
    }
}

#[test]
fn test_errdefer_expr() {
    let prog = parse_ok("fn main() { errdefer conn.close(); }");
    if let Item::Function(f) = &prog.items[0] {
        assert!(matches!(
            &f.body.stmts[0].kind,
            StmtKind::ErrDefer { binding: None, .. }
        ));
    }
}

#[test]
fn test_while_let() {
    let prog = parse_ok("fn main() { while let Some(x) = iter.next() { process(x); } }");
    if let Item::Function(f) = &prog.items[0] {
        let stmt = &f.body.stmts[0];
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::WhileLet { pattern, .. } = &expr.kind {
                assert!(matches!(&pattern.kind, PatternKind::TupleVariant { .. }));
            } else {
                panic!("Expected WhileLet, got {:?}", expr.kind);
            }
        } else {
            panic!("Expected Expr stmt");
        }
    }
}

#[test]
fn test_seq_block() {
    let prog = parse_ok("fn main() { let x = seq { init(); configure(); }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(&value.kind, ExprKind::Seq(_)));
        } else {
            panic!("Expected Let with Seq");
        }
    }
}

#[test]
fn test_par_block() {
    let prog = parse_ok("fn main() { let x = par { fetch_a(); fetch_b(); }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(&value.kind, ExprKind::Par(_)));
        } else {
            panic!("Expected Let with Par");
        }
    }
}

#[test]
fn test_lock_block() {
    let prog = parse_ok("fn main() { let v = lock counter { counter.count }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Lock { mutex, alias, .. } = &value.kind {
                assert_eq!(mutex, "counter");
                assert!(alias.is_none());
            } else {
                panic!("Expected Lock");
            }
        }
    }
}

#[test]
fn test_lock_block_with_alias() {
    let prog = parse_ok("fn main() { let v = lock connection_pool mgr { mgr.recycle() }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Lock { mutex, alias, .. } = &value.kind {
                assert_eq!(mutex, "connection_pool");
                assert_eq!(alias.as_deref(), Some("mgr"));
            } else {
                panic!("Expected Lock");
            }
        }
    }
}

#[test]
fn test_labeled_loop() {
    let prog = parse_ok("fn main() { outer: loop { break outer; } }");
    if let Item::Function(f) = &prog.items[0] {
        let stmt = &f.body.stmts[0];
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::Loop { label, .. } = &expr.kind {
                assert_eq!(label.as_deref(), Some("outer"));
            } else {
                panic!("Expected labeled Loop");
            }
        }
    }
}

#[test]
fn test_labeled_while() {
    let prog = parse_ok("fn main() { outer: while true { break outer; } }");
    if let Item::Function(f) = &prog.items[0] {
        let stmt = &f.body.stmts[0];
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::While { label, .. } = &expr.kind {
                assert_eq!(label.as_deref(), Some("outer"));
            } else {
                panic!("Expected labeled While");
            }
        }
    }
}

#[test]
fn test_labeled_for() {
    let prog = parse_ok("fn main() { outer: for x in items { continue outer; } }");
    if let Item::Function(f) = &prog.items[0] {
        let stmt = &f.body.stmts[0];
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::For { label, .. } = &expr.kind {
                assert_eq!(label.as_deref(), Some("outer"));
            } else {
                panic!("Expected labeled For");
            }
        }
    }
}

#[test]
fn test_break_with_label() {
    // `break outer;` inside a loop labeled `outer:` is parsed as a labeled break.
    let prog = parse_ok("fn main() { outer: loop { break outer; } }");
    if let Item::Function(f) = &prog.items[0] {
        let stmt = &f.body.stmts[0];
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::Loop { body, .. } = &expr.kind {
                if let StmtKind::Expr(inner) = &body.stmts[0].kind {
                    if let ExprKind::Break { label, value } = &inner.kind {
                        assert_eq!(label.as_deref(), Some("outer"));
                        assert!(value.is_none());
                    } else {
                        panic!("Expected Break");
                    }
                }
            }
        }
    }
}

#[test]
fn test_continue_with_label() {
    let prog = parse_ok("fn main() { outer: loop { continue outer; } }");
    if let Item::Function(f) = &prog.items[0] {
        let stmt = &f.body.stmts[0];
        if let StmtKind::Expr(expr) = &stmt.kind {
            if let ExprKind::Loop { body, .. } = &expr.kind {
                if let StmtKind::Expr(inner) = &body.stmts[0].kind {
                    if let ExprKind::Continue { label } = &inner.kind {
                        assert_eq!(label.as_deref(), Some("outer"));
                    } else {
                        panic!("Expected Continue with label");
                    }
                }
            }
        }
    }
}

/// Helper: extract the function-body's first expression — the labeled-
/// loop / labeled-block tests above use `body.stmts[0]`, but the parser
/// is free to place the construct in `body.final_expr` when no
/// trailing semicolon is present. This helper accepts either layout.
fn first_fn_expr(prog: &Program) -> &Expr {
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("Expected first item to be a function"),
    };
    if let Some(ref expr) = f.body.final_expr {
        return expr;
    }
    if let StmtKind::Expr(ref e) = f.body.stmts[0].kind {
        return e;
    }
    panic!("function body has no expression");
}

#[test]
fn test_labeled_block_basic_parse() {
    // `label: { ... }` parses to ExprKind::LabeledBlock with the label
    // attached to the AST node and the body parsed as a normal block.
    // Inner `break label;` is recognized as labeled break (the label is
    // active in `loop_labels` during body parse).
    let prog = parse_ok("fn main() { outer: { break outer; } }");
    let expr = first_fn_expr(&prog);
    if let ExprKind::LabeledBlock { label, body, .. } = &expr.kind {
        assert_eq!(label, "outer");
        // Inner stmt is `break outer;`
        if let StmtKind::Expr(inner) = &body.stmts[0].kind {
            if let ExprKind::Break { label, value } = &inner.kind {
                assert_eq!(label.as_deref(), Some("outer"));
                assert!(value.is_none());
            } else {
                panic!("Expected Break inside labeled block, got {:?}", inner.kind);
            }
        }
    } else {
        panic!("Expected LabeledBlock, got {:?}", expr.kind);
    }
}

#[test]
fn test_labeled_block_nested_parse() {
    // Two nested labeled blocks parse with the inner block as a
    // distinct LabeledBlock node within the outer's body.
    let prog = parse_ok("fn main() { outer: { inner: { break outer; } } }");
    let expr = first_fn_expr(&prog);
    if let ExprKind::LabeledBlock { label, body, .. } = &expr.kind {
        assert_eq!(label, "outer");
        // Outer body contains the inner labeled block as its tail or
        // first stmt.
        let inner_expr = body
            .final_expr
            .as_deref()
            .or_else(|| {
                body.stmts.first().and_then(|s| match &s.kind {
                    StmtKind::Expr(e) => Some(e),
                    _ => None,
                })
            })
            .expect("expected inner expr in outer body");
        if let ExprKind::LabeledBlock {
            label: inner_label, ..
        } = &inner_expr.kind
        {
            assert_eq!(inner_label, "inner");
        } else {
            panic!("Expected nested LabeledBlock, got {:?}", inner_expr.kind);
        }
    } else {
        panic!("Expected outer LabeledBlock, got {:?}", expr.kind);
    }
}

#[test]
fn test_range_pattern_integer() {
    let prog = parse_ok("fn main() { match x { 1..=10 => a, _ => b, } }");
    if let Item::Function(f) = &prog.items[0] {
        if let Some(expr) = &f.body.final_expr {
            if let ExprKind::Match { arms, .. } = &expr.kind {
                assert!(matches!(
                    &arms[0].pattern.kind,
                    PatternKind::RangePattern { .. }
                ));
            }
        }
    }
}

#[test]
fn test_range_pattern_char() {
    let prog = parse_ok("fn main() { match c { 'a'..='z' => lower, _ => other, } }");
    if let Item::Function(f) = &prog.items[0] {
        if let Some(expr) = &f.body.final_expr {
            if let ExprKind::Match { arms, .. } = &expr.kind {
                if let PatternKind::RangePattern {
                    start,
                    end,
                    inclusive,
                } = &arms[0].pattern.kind
                {
                    assert!(matches!(start, Some(LiteralPattern::Char('a'))));
                    assert!(matches!(end, Some(LiteralPattern::Char('z'))));
                    assert!(*inclusive);
                } else {
                    panic!("Expected RangePattern");
                }
            }
        }
    }
}

#[test]
fn test_at_binding_pattern() {
    let prog = parse_ok("fn main() { match val { x @ Some(_) => x, None => y, } }");
    if let Item::Function(f) = &prog.items[0] {
        if let Some(expr) = &f.body.final_expr {
            if let ExprKind::Match { arms, .. } = &expr.kind {
                if let PatternKind::AtBinding { name, pattern } = &arms[0].pattern.kind {
                    assert_eq!(name, "x");
                    assert!(matches!(&pattern.kind, PatternKind::TupleVariant { .. }));
                } else {
                    panic!("Expected AtBinding");
                }
            }
        }
    }
}

// ── @ binding — additional positions per design.md § @ Bindings ─────

#[test]
fn test_at_binding_with_range_pattern() {
    parse_ok(
        "fn classify(n: i32) -> i32 { \
         match n { code @ 500..=599 => code, _ => 0 } \
         }",
    );
}

#[test]
fn test_at_binding_inside_struct_pattern() {
    // `Response { status: code @ 500..=599, body }` — nested @ inside
    // a struct destructure.
    parse_ok(
        "struct Response { status: i32, body: i32 } \
         fn handle(r: Response) -> i32 { \
         match r { Response { status: code @ 500..=599, body: _ } => code, _ => 0 } \
         }",
    );
}

#[test]
fn test_at_binding_with_or_pattern_same_name() {
    // Or-pattern with the same `@` binding name in each alternative is
    // legal — all alternatives bind `x` to the scrutinee value.
    parse_ok(
        "fn f(o: Option[i32]) -> i32 { \
         match o { x @ Option.Some(_) | x @ Option.None => 0 } \
         }",
    );
}

#[test]
fn test_at_binding_in_let_pattern_irrefutable() {
    // `let outer @ Foo { a } = foo` — irrefutable, accepted.
    parse_ok(
        "struct Foo { a: i32 } \
         fn main() { let foo = Foo { a: 1 }; let outer @ Foo { a } = foo; }",
    );
}

#[test]
fn test_at_binding_let_with_refutable_inner_rejected() {
    // `let x @ Some(y) = opt;` — inner pattern is refutable so the
    // whole `let` is refutable. Existing irrefutable-pattern check
    // still applies through the @ binding.
    let (_, errors) =
        parse_with_errors("fn main() { let opt = Option.None; let x @ Option.Some(y) = opt; }");
    let _has_typecheck_error_path = errors.is_empty();
    // The refutability check fires at typecheck, not parse — verify
    // separately via typechecker (see `tests/typechecker.rs`).
}

#[test]
fn test_at_binding_nested_at() {
    // `outer @ Foo { field: inner @ Bar(value) }` — three names bound:
    // outer (whole), field (struct field via shorthand-equivalent),
    // inner (the nested @ binding).
    parse_ok(
        "enum Bar { B(i32) } \
         struct Foo { field: Bar } \
         fn f(foo: Foo) -> i32 { \
         match foo { outer @ Foo { field: inner @ Bar.B(value) } => value, _ => 0 } \
         }",
    );
}

#[test]
fn test_struct_literal_shorthand() {
    let prog = parse_ok("fn main() { let p = Point { x, y }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::StructLiteral { fields, .. } = &value.kind {
                assert_eq!(fields.len(), 2);
                assert!(fields[0].shorthand);
                assert_eq!(fields[0].name, "x");
                assert!(fields[1].shorthand);
                assert_eq!(fields[1].name, "y");
            } else {
                panic!("Expected StructLiteral");
            }
        }
    }
}

#[test]
fn test_struct_literal_mixed_shorthand_and_explicit() {
    let prog = parse_ok("fn main() { let p = Point { x, y: 2 }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::StructLiteral { fields, .. } = &value.kind {
                assert_eq!(fields.len(), 2);
                assert!(fields[0].shorthand);
                assert!(!fields[1].shorthand);
            } else {
                panic!("Expected StructLiteral");
            }
        }
    }
}

#[test]
fn test_struct_literal_spread() {
    let prog = parse_ok("fn main() { let u = User { name: n, ..existing }; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            if let ExprKind::StructLiteral { fields, spread, .. } = &value.kind {
                assert_eq!(fields.len(), 1);
                assert!(spread.is_some());
            } else {
                panic!("Expected StructLiteral");
            }
        }
    }
}

#[test]
fn test_own_self_param_rejected() {
    let (_prog, errors) = parse_with_errors("struct Foo {} impl Foo { fn consume(own self) { } }");
    assert!(
        !errors.is_empty(),
        "expected parse error on `own self`, got none"
    );
    assert!(
        errors.iter().any(|e| e.message.contains("own self")),
        "expected diagnostic mentioning `own self`, got: {:?}",
        errors
    );
}

#[test]
fn test_bare_self_is_owned() {
    let prog = parse_ok("struct Foo {} impl Foo { fn consume(self) { } }");
    if let Item::ImplBlock(imp) = &prog.items[1] {
        assert_eq!(impl_methods(imp)[0].self_param, Some(SelfParam::Owned));
    }
}

#[test]
fn test_call_site_mut_marker() {
    // Fresh binding passed to a `mut ref` / `mut Slice` parameter carries
    // a `mut` marker at the call site (design.md Feature 4 Part 1½ Rule 1).
    let prog = parse_ok("fn main() { sort_in_place(mut v); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(e) = &f.body.stmts[0].kind {
            if let ExprKind::Call { args, .. } = &e.kind {
                assert_eq!(args.len(), 1);
                assert!(
                    args[0].mut_marker,
                    "expected mut_marker=true on fresh-binding argument"
                );
            } else {
                panic!("Expected Call expression");
            }
        }
    }
}

#[test]
fn test_call_site_bare_arg_no_marker() {
    let prog = parse_ok("fn main() { greet(name); }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(e) = &f.body.stmts[0].kind {
            if let ExprKind::Call { args, .. } = &e.kind {
                assert!(!args[0].mut_marker);
            }
        }
    }
}

#[test]
fn test_call_site_ref_rejected() {
    // `ref` is never legal in argument position (design.md Part 1½ Rule 4).
    let (_prog, errors) = parse_with_errors("fn main() { load(ref cfg); }");
    assert!(
        errors.iter().any(|e| e.message.contains("ref")),
        "expected diagnostic rejecting `ref` at call site, got: {:?}",
        errors
    );
}

#[test]
fn test_call_site_mut_ref_rejected() {
    // `mut ref` at a call site is rejected with a hint to drop the `ref`.
    let (_prog, errors) = parse_with_errors("fn main() { sort(mut ref v); }");
    assert!(
        errors.iter().any(|e| e.message.contains("mut ref")),
        "expected diagnostic rejecting `mut ref` at call site, got: {:?}",
        errors
    );
}

#[test]
fn test_nil_coalesce_operator() {
    let prog = parse_ok("fn main() { let x = a ?? b; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(&value.kind, ExprKind::NilCoalesce { .. }));
        } else {
            panic!("Expected Let with NilCoalesce");
        }
    }
}

// ── Phase 2.2 updates: Associated types, const generics, etc. ──

#[test]
fn test_trait_associated_type() {
    let prog = parse_ok(
        r#"
        trait Iterator {
            type Item;
            fn next(mut ref self) -> Option[Self.Item];
        }
    "#,
    );
    if let Item::TraitDef(t) = &prog.items[0] {
        assert_eq!(t.items.len(), 2);
        // First item: associated type
        if let TraitItem::AssocType(assoc) = &t.items[0] {
            assert_eq!(assoc.name, "Item");
            assert!(assoc.bounds.is_empty());
        } else {
            panic!("Expected AssocType");
        }
        // Second item: method
        let methods = trait_methods(t);
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "next");
    } else {
        panic!("Expected TraitDef");
    }
}

#[test]
fn test_trait_associated_type_with_bounds() {
    let prog = parse_ok(
        r#"
        trait Container {
            type Item: Display + Clone;
            fn get(ref self) -> Self.Item;
        }
    "#,
    );
    if let Item::TraitDef(t) = &prog.items[0] {
        if let TraitItem::AssocType(assoc) = &t.items[0] {
            assert_eq!(assoc.name, "Item");
            assert_eq!(assoc.bounds.len(), 2);
            assert_eq!(assoc.bounds[0].path, vec!["Display"]);
            assert_eq!(assoc.bounds[1].path, vec!["Clone"]);
        } else {
            panic!("Expected AssocType");
        }
    } else {
        panic!("Expected TraitDef");
    }
}

#[test]
fn test_impl_associated_type_binding() {
    let prog = parse_ok(
        r#"
        impl Iterator for Counter {
            type Item = i64;
            fn next(mut ref self) -> Option[i64] { None }
        }
    "#,
    );
    if let Item::ImplBlock(imp) = &prog.items[0] {
        assert_eq!(imp.items.len(), 2);
        // First item: associated type binding
        if let ImplItem::AssocType(binding) = &imp.items[0] {
            assert_eq!(binding.name, "Item");
            assert!(matches!(&binding.ty.kind, TypeKind::Path(p) if p.segments == vec!["i64"]));
        } else {
            panic!("Expected AssocType binding");
        }
        // Second item: method
        let methods = impl_methods(imp);
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "next");
    } else {
        panic!("Expected ImplBlock");
    }
}

// ── GAT slice 1 — generic-parameter list on associated types ──────
//
// Per design.md § Generic associated types (GATs), an associated type
// may itself take type parameters (`type Mapped[U]`). Slice 1 lands
// the parser surface: both the trait-side declaration grammar and the
// impl-side binding grammar accept an optional `[P1, P2, ...]` after
// the associated-type name, with optional trait bounds and an optional
// `where` clause. Effect-polymorphic GATs (`type Mapped[U, with E]`)
// are out of v1 scope and rejected with the focused diagnostic
// `E_GAT_EFFECT_PARAM`, which steers the author at the carrying-method
// form. Slices 2+ wire the AST through the resolver / typechecker.

#[test]
fn gat_slice1_trait_assoc_type_with_single_generic_param() {
    let prog = parse_ok(
        r#"
        trait Functor {
            type Mapped[U];
        }
    "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let TraitItem::AssocType(assoc) = &t.items[0] else {
        panic!("Expected AssocType");
    };
    assert_eq!(assoc.name, "Mapped");
    let gp = assoc
        .generic_params
        .as_ref()
        .expect("expected generic params on GAT");
    assert_eq!(gp.params.len(), 1);
    assert_eq!(gp.params[0].name, "U");
    assert!(gp.params[0].bounds.is_empty());
    assert!(!gp.params[0].is_const);
    assert!(gp.effect_params.is_empty());
    assert!(assoc.bounds.is_empty());
    assert!(assoc.where_clause.is_none());
}

#[test]
fn gat_slice1_trait_assoc_type_with_multiple_generic_params() {
    let prog = parse_ok(
        r#"
        trait BiFunctor {
            type Mapped[L, R];
        }
    "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let TraitItem::AssocType(assoc) = &t.items[0] else {
        panic!("Expected AssocType");
    };
    assert_eq!(assoc.name, "Mapped");
    let gp = assoc
        .generic_params
        .as_ref()
        .expect("expected generic params");
    assert_eq!(gp.params.len(), 2);
    assert_eq!(gp.params[0].name, "L");
    assert_eq!(gp.params[1].name, "R");
}

#[test]
fn gat_slice1_trait_assoc_type_with_bound_on_param() {
    let prog = parse_ok(
        r#"
        trait Functor {
            type Mapped[U: Clone];
        }
    "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let TraitItem::AssocType(assoc) = &t.items[0] else {
        panic!("Expected AssocType");
    };
    let gp = assoc
        .generic_params
        .as_ref()
        .expect("expected generic params");
    assert_eq!(gp.params.len(), 1);
    assert_eq!(gp.params[0].name, "U");
    assert_eq!(gp.params[0].bounds.len(), 1);
    assert_eq!(gp.params[0].bounds[0].path, vec!["Clone"]);
}

#[test]
fn gat_slice1_trait_assoc_type_with_outer_bound() {
    // Bound attached to the GAT declaration itself —
    // `type Mapped[U]: Trait` — applies to every legal instantiation
    // of `U` at every impl site (slice 7 enforcement).
    let prog = parse_ok(
        r#"
        trait Functor {
            type Mapped[U]: Display + Clone;
        }
    "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let TraitItem::AssocType(assoc) = &t.items[0] else {
        panic!("Expected AssocType");
    };
    let gp = assoc
        .generic_params
        .as_ref()
        .expect("expected generic params");
    assert_eq!(gp.params.len(), 1);
    assert_eq!(assoc.bounds.len(), 2);
    assert_eq!(assoc.bounds[0].path, vec!["Display"]);
    assert_eq!(assoc.bounds[1].path, vec!["Clone"]);
}

#[test]
fn gat_slice1_trait_assoc_type_with_where_clause() {
    let prog = parse_ok(
        r#"
        trait Functor {
            type Mapped[U] where U: Clone;
        }
    "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let TraitItem::AssocType(assoc) = &t.items[0] else {
        panic!("Expected AssocType");
    };
    let wc = assoc.where_clause.as_ref().expect("expected where clause");
    assert_eq!(wc.constraints.len(), 1);
    let (name, bounds) = where_type_bound(&wc.constraints[0]);
    assert_eq!(name, "U");
    assert_eq!(bounds.len(), 1);
    assert_eq!(bounds[0].path, vec!["Clone"]);
}

#[test]
fn gat_slice1_trait_non_generic_assoc_type_unchanged() {
    // Negative pin: the non-generic shape `type Item;` still parses
    // with `generic_params = None` so existing consumers of the
    // legacy field set see no surprise.
    let prog = parse_ok(
        r#"
        trait Iterator {
            type Item;
        }
    "#,
    );
    let Item::TraitDef(t) = &prog.items[0] else {
        panic!("Expected TraitDef");
    };
    let TraitItem::AssocType(assoc) = &t.items[0] else {
        panic!("Expected AssocType");
    };
    assert_eq!(assoc.name, "Item");
    assert!(assoc.generic_params.is_none());
    assert!(assoc.where_clause.is_none());
}

#[test]
fn gat_slice1_impl_assoc_type_binding_with_generic_param() {
    // Mirrors the design.md example:
    //   impl Functor for Vec[T] {
    //       type Mapped[U] = Vec[U];
    //       ...
    //   }
    let prog = parse_ok(
        r#"
        impl Functor for Vec[T] {
            type Mapped[U] = Vec[U];
        }
    "#,
    );
    let Item::ImplBlock(imp) = &prog.items[0] else {
        panic!("Expected ImplBlock");
    };
    let ImplItem::AssocType(binding) = &imp.items[0] else {
        panic!("Expected AssocType binding");
    };
    assert_eq!(binding.name, "Mapped");
    let gp = binding
        .generic_params
        .as_ref()
        .expect("expected generic params on GAT binding");
    assert_eq!(gp.params.len(), 1);
    assert_eq!(gp.params[0].name, "U");
    // RHS is `Vec[U]`.
    let TypeKind::Path(p) = &binding.ty.kind else {
        panic!("Expected Path type");
    };
    assert_eq!(p.segments, vec!["Vec"]);
}

#[test]
fn gat_slice1_impl_assoc_type_binding_with_where_clause() {
    let prog = parse_ok(
        r#"
        impl Functor for Vec[T] {
            type Mapped[U] = Vec[U] where U: Clone;
        }
    "#,
    );
    let Item::ImplBlock(imp) = &prog.items[0] else {
        panic!("Expected ImplBlock");
    };
    let ImplItem::AssocType(binding) = &imp.items[0] else {
        panic!("Expected AssocType binding");
    };
    let wc = binding
        .where_clause
        .as_ref()
        .expect("expected where clause on GAT binding");
    assert_eq!(wc.constraints.len(), 1);
    let (name, bounds) = where_type_bound(&wc.constraints[0]);
    assert_eq!(name, "U");
    assert_eq!(bounds.len(), 1);
    assert_eq!(bounds[0].path, vec!["Clone"]);
}

#[test]
fn gat_slice1_impl_non_generic_assoc_type_binding_unchanged() {
    let prog = parse_ok(
        r#"
        impl Iterator for Counter {
            type Item = i64;
        }
    "#,
    );
    let Item::ImplBlock(imp) = &prog.items[0] else {
        panic!("Expected ImplBlock");
    };
    let ImplItem::AssocType(binding) = &imp.items[0] else {
        panic!("Expected AssocType binding");
    };
    assert_eq!(binding.name, "Item");
    assert!(binding.generic_params.is_none());
    assert!(binding.where_clause.is_none());
}

#[test]
fn gat_slice1_trait_assoc_type_rejects_effect_param() {
    // Effect-polymorphic GATs (`type Mapped[U, with E]`) are out of v1
    // scope per design.md § GATs "Out of v1 scope" bullet. The carrying
    // method takes the `with E` parameter, not the associated type.
    // Parser emits `E_GAT_EFFECT_PARAM` and recovers.
    let (_prog, errors) = parse_with_errors(
        r#"
        trait Functor {
            type Mapped[U, with E];
        }
    "#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_GAT_EFFECT_PARAM")
                && e.message.contains("with E")
                && e.message.contains("declaration")),
        "Expected E_GAT_EFFECT_PARAM diagnostic mentioning `with E` on \
         the declaration; got: {errors:?}"
    );
    // The diagnostic must steer the author at the carrying-method form.
    assert!(
        errors.iter().any(|e| e.message.contains("carrying method")),
        "Expected diagnostic to suggest the carrying-method form; got: {errors:?}"
    );
}

#[test]
fn gat_slice1_impl_assoc_type_binding_rejects_effect_param() {
    // Symmetry pin: the binding-side rejection matches the
    // declaration-side rejection.
    let (_prog, errors) = parse_with_errors(
        r#"
        impl Functor for Vec[T] {
            type Mapped[U, with E] = Vec[U];
        }
    "#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_GAT_EFFECT_PARAM")
                && e.message.contains("with E")
                && e.message.contains("binding")),
        "Expected E_GAT_EFFECT_PARAM diagnostic mentioning `with E` on \
         the binding; got: {errors:?}"
    );
}

#[test]
fn gat_slice1_multiple_effect_params_all_named_in_diagnostic() {
    // When the user writes `[U, with E, F]`, the diagnostic names the
    // full effect list so the suggested rewrite is unambiguous.
    let (_prog, errors) = parse_with_errors(
        r#"
        trait Functor {
            type Mapped[U, with E, F];
        }
    "#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_GAT_EFFECT_PARAM") && e.message.contains("with E, F")),
        "Expected the diagnostic to name the full effect-param list; \
         got: {errors:?}"
    );
}

#[test]
fn test_trait_method_with_generic_params() {
    let prog = parse_ok(
        r#"
        trait Iterator {
            type Item;
            fn map[U](self, f: Fn(Self.Item) -> U) -> Vec[U] {
                Vec.new()
            }
        }
    "#,
    );
    if let Item::TraitDef(t) = &prog.items[0] {
        let methods = trait_methods(t);
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "map");
        let gp = methods[0]
            .generic_params
            .as_ref()
            .expect("expected generic params");
        assert_eq!(gp.params.len(), 1);
        assert_eq!(gp.params[0].name, "U");
    } else {
        panic!("Expected TraitDef");
    }
}

#[test]
fn test_const_generic_parameter() {
    let prog = parse_ok("struct Array[T, const N: usize] { data: T, }");
    if let Item::StructDef(s) = &prog.items[0] {
        let gp = s.generic_params.as_ref().expect("expected generic params");
        assert_eq!(gp.params.len(), 2);
        assert_eq!(gp.params[0].name, "T");
        assert!(!gp.params[0].is_const);
        assert_eq!(gp.params[1].name, "N");
        assert!(gp.params[1].is_const);
        assert!(gp.params[1].const_type.is_some());
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_const_generic_on_function() {
    let prog = parse_ok("fn zeros[const N: usize]() -> Array[f64, N] { Array.new() }");
    if let Item::Function(f) = &prog.items[0] {
        let gp = f.generic_params.as_ref().expect("expected generic params");
        assert_eq!(gp.params.len(), 1);
        assert_eq!(gp.params[0].name, "N");
        assert!(gp.params[0].is_const);
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_where_clause_assoc_type_equality() {
    let prog = parse_ok("fn process[I: Iterator](iter: I) -> i64 where I.Item = i64 { 0 }");
    if let Item::Function(f) = &prog.items[0] {
        let wc = f.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 1);
        if let WhereConstraint::AssocTypeEq {
            type_name,
            assoc_name,
            ty,
            ..
        } = &wc.constraints[0]
        {
            assert_eq!(type_name, "I");
            assert_eq!(assoc_name, "Item");
            assert!(matches!(&ty.kind, TypeKind::Path(p) if p.segments == vec!["i64"]));
        } else {
            panic!("Expected AssocTypeEq constraint");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_where_clause_mixed_bounds_and_assoc_type() {
    let prog = parse_ok("fn f[I, T](iter: I) where I: Iterator, I.Item = T, T: Display { }");
    if let Item::Function(f) = &prog.items[0] {
        let wc = f.where_clause.as_ref().expect("expected where clause");
        assert_eq!(wc.constraints.len(), 3);
        // First: I: Iterator
        let (tn0, b0) = where_type_bound(&wc.constraints[0]);
        assert_eq!(tn0, "I");
        assert_eq!(b0[0].path, vec!["Iterator"]);
        // Second: I::Item = T
        assert!(
            matches!(&wc.constraints[1], WhereConstraint::AssocTypeEq { type_name, assoc_name, .. }
            if type_name == "I" && assoc_name == "Item")
        );
        // Third: T: Display
        let (tn2, b2) = where_type_bound(&wc.constraints[2]);
        assert_eq!(tn2, "T");
        assert_eq!(b2[0].path, vec!["Display"]);
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_effect_blocks_verb() {
    let prog = parse_ok("fn wait() with blocks { }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 1);
        if let EffectItem::Verb(v) = &effects.items[0] {
            assert_eq!(v.kind, EffectVerbKind::Blocks);
            assert!(v.resources.is_empty());
        } else {
            panic!("Expected Verb");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_effect_suspends_verb() {
    let prog = parse_ok("fn yield_control() with suspends { }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 1);
        if let EffectItem::Verb(v) = &effects.items[0] {
            assert_eq!(v.kind, EffectVerbKind::Suspends);
        } else {
            panic!("Expected Verb");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_effect_user_defined_verb() {
    let prog = parse_ok("effect verb logs;\nfn record() with logs(AppLog) { }");
    if let Item::Function(f) = &prog.items[1] {
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 1);
        if let EffectItem::Verb(v) = &effects.items[0] {
            assert_eq!(v.kind, EffectVerbKind::UserDefined("logs".to_string()));
            assert_eq!(v.resources.len(), 1);
        } else {
            panic!("Expected Verb");
        }
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_effect_mixed_verbs_space_separated() {
    let prog = parse_ok("fn process() with reads(DB) writes(Log) blocks { }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 3);
        assert!(
            matches!(&effects.items[0], EffectItem::Verb(v) if v.kind == EffectVerbKind::Reads)
        );
        assert!(
            matches!(&effects.items[1], EffectItem::Verb(v) if v.kind == EffectVerbKind::Writes)
        );
        assert!(
            matches!(&effects.items[2], EffectItem::Verb(v) if v.kind == EffectVerbKind::Blocks)
        );
    } else {
        panic!("Expected Function");
    }
}

#[test]
fn test_effect_group_with_verbs() {
    let prog = parse_ok("fn process() with order_processing reads(FileSystem) { }");
    if let Item::Function(f) = &prog.items[0] {
        let effects = f.effects.as_ref().unwrap();
        assert_eq!(effects.items.len(), 2);
        assert!(matches!(&effects.items[0], EffectItem::Group(name) if name == "order_processing"));
        assert!(
            matches!(&effects.items[1], EffectItem::Verb(v) if v.kind == EffectVerbKind::Reads)
        );
    } else {
        panic!("Expected Function");
    }
}

// ── @ Attribute Syntax ─────────────────────────────────────────

#[test]
fn test_at_attribute_on_struct() {
    let prog = parse_ok("@no_rc\nstruct Particle { x: f64, y: f64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert!(s.no_rc, "@no_rc should set no_rc = true");
        assert_eq!(s.name, "Particle");
        assert!(s.attributes.iter().any(|a| a.name == "no_rc"));
    } else {
        panic!("Expected StructDef");
    }
}

#[test]
fn test_at_attribute_with_pound_attribute() {
    // Both @ and #[] attributes can coexist
    let prog = parse_ok("@no_rc\n#[derive(Copy)]\nstruct Particle { x: f64 }");
    if let Item::StructDef(s) = &prog.items[0] {
        assert!(s.no_rc);
        assert!(s.attributes.iter().any(|a| a.name == "no_rc"));
        assert!(s.attributes.iter().any(|a| a.name == "derive"));
    } else {
        panic!("Expected StructDef");
    }
}

// ── `providers { R => p, ... } in { body }` block ───────────────

fn providers_expr(prog: &Program) -> (&Vec<ProviderBinding>, &Block) {
    // Find the first `providers` block expression in `fn main`'s body.
    let Item::Function(f) = &prog.items[0] else {
        panic!("Expected Function");
    };
    fn walk(block: &Block) -> Option<(&Vec<ProviderBinding>, &Block)> {
        for s in &block.stmts {
            if let StmtKind::Expr(e) = &s.kind {
                if let ExprKind::Providers { bindings, body } = &e.kind {
                    return Some((bindings, body));
                }
            }
        }
        if let Some(e) = &block.final_expr {
            if let ExprKind::Providers { bindings, body } = &e.kind {
                return Some((bindings, body));
            }
        }
        None
    }
    walk(&f.body).expect("expected a providers block in fn main")
}

// ── `#[with_provider(resource, constructor_fn)]` attribute ──────

#[test]
fn test_with_provider_attribute_parses_positional_args() {
    let prog = parse_ok(
        "#[with_provider(Clock, FakeClock.new)]\n\
         fn test_timestamp() { }",
    );
    if let Item::Function(f) = &prog.items[0] {
        let attr = &f.attributes[0];
        assert_eq!(attr.name, "with_provider");
        assert_eq!(attr.args.len(), 2);
        // Both args are positional — `name` is None.
        assert!(attr.args[0].name.is_none());
        assert!(attr.args[1].name.is_none());
        // First arg is a bare identifier `Clock`.
        match &attr.args[0].value.as_ref().unwrap().kind {
            ExprKind::Identifier(n) => assert_eq!(n, "Clock"),
            other => panic!("expected Identifier, got {other:?}"),
        }
        // Second arg is a path / field access `FakeClock.new`. Parser
        // leaves it as either `FieldAccess(Path(["FakeClock"]), "new")`
        // or similar — we just require it parses without error.
        assert!(attr.args[1].value.is_some());
    } else {
        panic!("expected Function");
    }
}

#[test]
fn test_with_provider_attribute_parses_constructor_call() {
    // Constructor arg can itself be a call expression.
    let prog = parse_ok(
        "#[with_provider(Clock, FakeClock.at(0))]\n\
         fn test_fixed_time() { }",
    );
    if let Item::Function(f) = &prog.items[0] {
        let attr = &f.attributes[0];
        assert_eq!(attr.args.len(), 2);
        assert!(attr.args[1].value.is_some());
    } else {
        panic!("expected Function");
    }
}

#[test]
fn test_with_provider_attribute_allows_dotted_resource_path() {
    // `db.UserDB` resource path — field-access expression chain.
    let prog = parse_ok(
        "#[with_provider(db.UserDB, FakeDB.new)]\n\
         fn test_fixture() { }",
    );
    if let Item::Function(f) = &prog.items[0] {
        let attr = &f.attributes[0];
        assert_eq!(attr.args.len(), 2);
        // First arg is a FieldAccess expression — the typechecker can
        // decide whether it resolves to a valid resource path later.
        assert!(attr.args[0].value.is_some());
    } else {
        panic!("expected Function");
    }
}

#[test]
fn test_multiple_with_provider_attributes_on_one_fn() {
    // Multi-attribute form — source order is outer-to-inner.
    let prog = parse_ok(
        "#[with_provider(Clock, FakeClock.new)]\n\
         #[with_provider(UserDB, FakeDB.new)]\n\
         fn test_two_providers() { }",
    );
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.attributes.len(), 2);
        assert_eq!(f.attributes[0].name, "with_provider");
        assert_eq!(f.attributes[1].name, "with_provider");
    } else {
        panic!("expected Function");
    }
}

#[test]
fn test_providers_block_parses_single_binding() {
    let prog = parse_ok(
        "fn main() {
             providers {
                 UserDB => make_db(),
             } in {
                 run()
             }
         }",
    );
    let (bindings, _body) = providers_expr(&prog);
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].resource, "UserDB");
}

#[test]
fn test_providers_block_parses_multiple_bindings() {
    let prog = parse_ok(
        "fn main() {
             providers {
                 UserDB   => make_db(),
                 Cache    => make_cache(),
                 AuditLog => make_log(),
             } in {
                 run()
             }
         }",
    );
    let (bindings, _body) = providers_expr(&prog);
    let names: Vec<_> = bindings.iter().map(|b| b.resource.as_str()).collect();
    assert_eq!(names, vec!["UserDB", "Cache", "AuditLog"]);
}

#[test]
fn test_providers_block_rejects_empty_binding_list() {
    let (_prog, errors) = parse_with_errors(
        "fn main() {
             providers { } in {
                 run()
             }
         }",
    );
    assert!(
        errors.iter().any(|e| e.message.contains("at least one")),
        "expected 'at least one binding' error, got {:?}",
        errors
    );
}

#[test]
fn test_providers_block_without_trailing_comma_ok() {
    let prog = parse_ok(
        "fn main() {
             providers {
                 UserDB => make_db(),
                 Cache  => make_cache()
             } in {
                 run()
             }
         }",
    );
    let (bindings, _body) = providers_expr(&prog);
    assert_eq!(bindings.len(), 2);
}

#[test]
fn test_providers_is_contextual_keyword_usable_as_identifier() {
    // Theme 4 follow-up (2026-05-10): the lexer no longer reserves
    // `providers` as a keyword token. The bareword is usable as a
    // module name (`src/providers.kara`), function name, parameter
    // name, and variable binding. The parser dispatches to the
    // `providers { R => e } in { body }` block shape contextually —
    // only when an identifier expression named `providers` is
    // immediately followed by `{`.
    parse_ok(
        "fn providers() -> i64 { 7 }\n\
         fn main() {\n\
             let providers: i64 = 1;\n\
             let _ = providers + providers;\n\
         }",
    );
}

// ── Half-open range expressions ─────────────────────────────────────────────

#[test]
fn test_range_from() {
    // `a..` — start only
    let prog = parse_ok("fn main() { a..; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::Range {
                start,
                end,
                inclusive,
            } = &expr.kind
            {
                assert!(start.is_some());
                assert!(end.is_none());
                assert!(!inclusive);
            } else {
                panic!("Expected Range");
            }
        }
    }
}

#[test]
fn test_range_to_exclusive() {
    // `..b` — end only, non-inclusive
    let prog = parse_ok("fn main() { let r = ..10; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value: expr, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Range {
                start,
                end,
                inclusive,
            } = &expr.kind
            {
                assert!(start.is_none());
                assert!(end.is_some());
                assert!(!inclusive);
            } else {
                panic!("Expected Range");
            }
        }
    }
}

#[test]
fn test_range_to_inclusive() {
    // `..=b` — end only, inclusive
    let prog = parse_ok("fn main() { let r = ..=10; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value: expr, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Range {
                start,
                end,
                inclusive,
            } = &expr.kind
            {
                assert!(start.is_none());
                assert!(end.is_some());
                assert!(inclusive);
            } else {
                panic!("Expected Range");
            }
        }
    }
}

#[test]
fn test_range_full() {
    // `..` — neither start nor end
    let prog = parse_ok("fn main() { let r = ..; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value: expr, .. } = &f.body.stmts[0].kind {
            if let ExprKind::Range {
                start,
                end,
                inclusive,
            } = &expr.kind
            {
                assert!(start.is_none());
                assert!(end.is_none());
                assert!(!inclusive);
            } else {
                panic!("Expected Range");
            }
        }
    }
}

#[test]
fn test_range_full_as_slice_index() {
    // `v[..]` — full-range slice
    parse_ok("fn main() { let s = v[..]; }");
}

#[test]
fn test_range_from_as_slice_index() {
    // `v[i..]` — slice from index
    parse_ok("fn main() { let s = v[i..]; }");
}

#[test]
fn test_range_to_as_slice_index() {
    // `v[..n]` — slice to index
    parse_ok("fn main() { let s = v[..n]; }");
}

#[test]
fn test_range_to_inclusive_as_slice_index() {
    // `v[..=n]` — inclusive slice to index
    parse_ok("fn main() { let s = v[..=n]; }");
}

#[test]
fn test_range_both_bounds_still_parses() {
    // `a..b` — still works with both bounds
    let prog = parse_ok("fn main() { for i in 0..10 { x(); } }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::For { iterable, .. } = &expr.kind {
                if let ExprKind::Range {
                    start,
                    end,
                    inclusive,
                } = &iterable.kind
                {
                    assert!(start.is_some());
                    assert!(end.is_some());
                    assert!(!inclusive);
                } else {
                    panic!("Expected Range");
                }
            }
        }
    }
}

#[test]
fn test_range_both_bounds_inclusive_still_parses() {
    // `a..=b` — still works with both bounds, inclusive
    let prog = parse_ok("fn main() { for i in 1..=5 { x(); } }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Expr(expr) = &f.body.stmts[0].kind {
            if let ExprKind::For { iterable, .. } = &expr.kind {
                if let ExprKind::Range {
                    start,
                    end,
                    inclusive,
                } = &iterable.kind
                {
                    assert!(start.is_some());
                    assert!(end.is_some());
                    assert!(inclusive);
                } else {
                    panic!("Expected Range");
                }
            }
        }
    }
}

// ── Half-open range patterns ─────────────────────────────────────────────────

#[test]
fn test_range_pattern_value_class_scrutinee_with_dotdot_arm() {
    // Regression: `match n { ..0 => -1, _ => 0 }` previously misparsed
    // because `looks_like_struct_literal` matched `n { ..0 ... }` as a
    // struct-update literal. Struct literals only fire on Type-class
    // identifiers; this test pins that.
    parse_ok("fn main() { let n: i32 = 1; let _ = match n { ..0 => 0, _ => 1 }; }");
}

#[test]
fn test_range_pattern_bounded_exclusive() {
    // `lo..hi` (bounded exclusive) — five-form coverage from
    // design.md § Range Patterns.
    let prog = parse_ok("fn main() { match n { 10..100 => big, _ => other, } }");
    if let Item::Function(f) = &prog.items[0] {
        if let Some(expr) = &f.body.final_expr {
            if let ExprKind::Match { arms, .. } = &expr.kind {
                if let PatternKind::RangePattern {
                    start,
                    end,
                    inclusive,
                } = &arms[0].pattern.kind
                {
                    assert!(start.is_some(), "expected bounded start");
                    assert!(end.is_some(), "expected bounded end");
                    assert!(!*inclusive, "lo..hi is exclusive");
                } else {
                    panic!("expected RangePattern, got {:?}", arms[0].pattern.kind);
                }
            }
        }
    }
}

#[test]
fn test_range_pattern_bounded_exclusive_char() {
    parse_ok("fn main() { match c { 'a'..'z' => 0, _ => 1, } }");
}

#[test]
fn test_range_pattern_to_inclusive_only_end() {
    // `..=100` — pattern matching up to 100 inclusive
    let prog = parse_ok("fn main() { match n { ..=100 => small, _ => big, } }");
    if let Item::Function(f) = &prog.items[0] {
        if let Some(expr) = &f.body.final_expr {
            if let ExprKind::Match { arms, .. } = &expr.kind {
                if let PatternKind::RangePattern {
                    start,
                    end,
                    inclusive,
                } = &arms[0].pattern.kind
                {
                    assert!(start.is_none());
                    assert!(end.is_some());
                    assert!(*inclusive);
                } else {
                    panic!("Expected RangePattern, got {:?}", arms[0].pattern.kind);
                }
            }
        }
    }
}

#[test]
fn test_range_pattern_from_only_start() {
    // `100..` — pattern matching from 100 upward
    let prog = parse_ok("fn main() { match n { 100.. => big, _ => other, } }");
    if let Item::Function(f) = &prog.items[0] {
        if let Some(expr) = &f.body.final_expr {
            if let ExprKind::Match { arms, .. } = &expr.kind {
                if let PatternKind::RangePattern {
                    start,
                    end,
                    inclusive,
                } = &arms[0].pattern.kind
                {
                    assert!(start.is_some());
                    assert!(end.is_none());
                    assert!(!inclusive);
                } else {
                    panic!("Expected RangePattern, got {:?}", arms[0].pattern.kind);
                }
            }
        }
    }
}

#[test]
fn test_range_pattern_both_bounds_inclusive_still_works() {
    // `'a'..='z'` — fully-bounded inclusive pattern (regression)
    let prog = parse_ok("fn main() { match c { 'a'..='z' => lower, _ => other, } }");
    if let Item::Function(f) = &prog.items[0] {
        if let Some(expr) = &f.body.final_expr {
            if let ExprKind::Match { arms, .. } = &expr.kind {
                if let PatternKind::RangePattern {
                    start,
                    end,
                    inclusive,
                } = &arms[0].pattern.kind
                {
                    assert!(matches!(start, Some(LiteralPattern::Char('a'))));
                    assert!(matches!(end, Some(LiteralPattern::Char('z'))));
                    assert!(*inclusive);
                } else {
                    panic!("Expected RangePattern");
                }
            }
        }
    }
}

// ── Identifier case-class enforcement ────────────────────────────

fn errors_contain(errors: &[karac::parser::ParseError], fragment: &str) -> bool {
    errors.iter().any(|e| e.to_string().contains(fragment))
}

#[test]
fn test_ident_class_struct_must_be_pascal() {
    let (_, errors) = parse_with_errors("struct my_struct {}");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case struct"
    );
    assert!(errors_contain(&errors, "my_struct"), "{errors:?}");
    assert!(errors_contain(&errors, "MyStruct"), "{errors:?}");
}

#[test]
fn test_ident_class_struct_pascal_ok() {
    parse_ok("struct MyStruct {}");
}

#[test]
fn test_ident_class_enum_must_be_pascal() {
    let (_, errors) = parse_with_errors("enum my_color { Red, }");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case enum"
    );
    assert!(errors_contain(&errors, "my_color"), "{errors:?}");
}

#[test]
fn test_ident_class_enum_variant_must_be_pascal() {
    let (_, errors) = parse_with_errors("enum Color { red, }");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case variant"
    );
    assert!(errors_contain(&errors, "red"), "{errors:?}");
}

#[test]
fn test_ident_class_trait_must_be_pascal() {
    let (_, errors) = parse_with_errors("trait my_trait {}");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case trait"
    );
    assert!(errors_contain(&errors, "my_trait"), "{errors:?}");
}

#[test]
fn test_ident_class_type_alias_must_be_pascal() {
    let (_, errors) = parse_with_errors("type my_alias = i32;");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case type alias"
    );
    assert!(errors_contain(&errors, "my_alias"), "{errors:?}");
}

#[test]
fn test_ident_class_const_must_be_screaming_snake() {
    let (_, errors) = parse_with_errors("const myConst: i32 = 1;");
    assert!(
        !errors.is_empty(),
        "expected naming error for non-SCREAMING_SNAKE const"
    );
    assert!(errors_contain(&errors, "myConst"), "{errors:?}");
    assert!(errors_contain(&errors, "MY_CONST"), "{errors:?}");
}

#[test]
fn test_ident_class_const_screaming_ok() {
    parse_ok("const MAX_SIZE: i32 = 100;");
}

#[test]
fn test_ident_class_fn_must_be_snake() {
    let (_, errors) = parse_with_errors("fn MyFunction() {}");
    assert!(
        !errors.is_empty(),
        "expected naming error for PascalCase function"
    );
    assert!(errors_contain(&errors, "MyFunction"), "{errors:?}");
}

#[test]
fn test_ident_class_fn_snake_ok() {
    parse_ok("fn my_function() {}");
}

#[test]
fn test_ident_class_param_must_be_snake() {
    let (_, errors) = parse_with_errors("fn f(MyParam: i32) {}");
    assert!(
        !errors.is_empty(),
        "expected naming error for PascalCase param"
    );
    assert!(errors_contain(&errors, "MyParam"), "{errors:?}");
}

#[test]
fn test_ident_class_param_snake_ok() {
    parse_ok("fn f(my_param: i32) {}");
}

#[test]
fn test_ident_class_generic_type_param_must_be_type_class() {
    let (_, errors) = parse_with_errors("fn f[my_t]() {}");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case generic param"
    );
    assert!(errors_contain(&errors, "my_t"), "{errors:?}");
}

#[test]
fn test_ident_class_generic_single_upper_ok() {
    // CN-7: single uppercase letter is valid Type-class
    parse_ok("fn f[T]() {}");
    parse_ok("fn f[K, V]() {}");
}

#[test]
fn test_ident_class_let_binding_snake_ok() {
    // Simple bindings in let/match are not checked at parse time: the parser
    // cannot distinguish a fresh binding from a unit-variant reference (e.g.
    // `None` in a match arm). Naming enforcement for let bindings is deferred
    // to the resolver, which has enough context to tell the two cases apart.
    parse_ok("fn main() { let my_var = 1; }");
    parse_ok("fn main() { let MyVar = 1; }"); // resolver will flag this
}

#[test]
fn test_ident_class_leading_underscore_value_ok() {
    // CN-5: _prefixed names are valid Value-class
    parse_ok("fn f(_unused: i32) {}");
    parse_ok("fn main() { let _x = 1; }");
}

#[test]
fn test_ident_class_extern_fn_value_class_enforced() {
    let (_, errors) = parse_with_errors(r#"unsafe extern "C" { fn BadName(); }"#);
    assert!(
        !errors.is_empty(),
        "expected naming error for PascalCase extern fn"
    );
    assert!(errors_contain(&errors, "BadName"), "{errors:?}");
}

#[test]
fn test_ident_class_extern_fn_kara_name_skips_check() {
    // With #[kara_name], the Kara-side name bypasses the naming check.
    parse_ok(
        r#"unsafe extern "C" {
               #[kara_name = "malloc"]
               fn BadName(size: i32) -> i32;
           }"#,
    );
}

#[test]
fn test_ident_class_struct_field_must_be_snake() {
    let (_, errors) = parse_with_errors("struct Foo { MyField: i32, }");
    assert!(
        !errors.is_empty(),
        "expected naming error for PascalCase struct field"
    );
    assert!(errors_contain(&errors, "MyField"), "{errors:?}");
}

#[test]
fn test_ident_class_struct_field_snake_ok() {
    parse_ok("struct Foo { my_field: i32, }");
}

#[test]
fn test_ident_class_const_generic_param_must_be_type_class() {
    // Const generic params follow the Type-class convention (single
    // uppercase letter or PascalCase). `n` is Value-class — rejected.
    let (_, errors) =
        parse_with_errors("fn zeros[const n: usize]() -> Array[f64, n] { Array.new() }");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case const generic param"
    );
    assert!(errors_contain(&errors, "n"), "{errors:?}");
}

#[test]
fn test_ident_class_const_generic_param_single_upper_ok() {
    // Single uppercase letter is Type-class — `N` accepted.
    parse_ok("fn zeros[const N: usize]() -> Array[f64, N] { Array.new() }");
}

#[test]
fn test_ident_class_assoc_type_must_be_type_class() {
    let (_, errors) = parse_with_errors("trait It { type item; }");
    assert!(
        !errors.is_empty(),
        "expected naming error for snake_case associated type"
    );
    assert!(errors_contain(&errors, "item"), "{errors:?}");
}

#[test]
fn test_ident_class_assoc_type_pascal_ok() {
    parse_ok("trait It { type Item; }");
    parse_ok("trait It { type Item: Iterator; }");
}

#[test]
fn test_ident_class_layout_name_must_be_value_class() {
    let (_, errors) =
        parse_with_errors("struct Entity { id: i64 } layout MyEntities: Vec[Entity] { id }");
    assert!(
        !errors.is_empty(),
        "expected naming error for PascalCase layout name"
    );
    assert!(errors_contain(&errors, "MyEntities"), "{errors:?}");
}

// ── Doc comment attachment (List 1: karac doc) ─────────────────────────

#[test]
fn test_doc_comment_attaches_to_function() {
    let p = parse_ok("/// Doubles its argument.\nfn double(n: i64) -> i64 { n * 2 }");
    let f = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.doc_comment.as_deref(), Some("Doubles its argument."));
}

#[test]
fn test_multiple_doc_comment_lines_join_with_newlines() {
    let p = parse_ok("/// Line one.\n/// Line two.\nfn f() {}");
    let f = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.doc_comment.as_deref(), Some("Line one.\nLine two."));
}

#[test]
fn test_doc_comment_attaches_to_struct() {
    let p = parse_ok("/// A point.\nstruct Point { x: i64, y: i64 }");
    let s = match &p.items[0] {
        Item::StructDef(s) => s,
        _ => panic!("expected struct"),
    };
    assert_eq!(s.doc_comment.as_deref(), Some("A point."));
}

#[test]
fn test_doc_comment_attaches_to_enum() {
    let p = parse_ok("/// A direction.\nenum Direction { Up, Down }");
    let e = match &p.items[0] {
        Item::EnumDef(e) => e,
        _ => panic!("expected enum"),
    };
    assert_eq!(e.doc_comment.as_deref(), Some("A direction."));
}

#[test]
fn test_doc_comment_before_attribute_works() {
    // /// comment then #[attr] then item — the doc must still attach.
    let p = parse_ok("/// Documented.\n#[unsafe(no_mangle)]\nfn f() {}");
    let f = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.doc_comment.as_deref(), Some("Documented."));
    assert_eq!(f.attributes.len(), 1);
}

#[test]
fn test_no_doc_comment_yields_none() {
    let p = parse_ok("fn f() {}");
    let f = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.doc_comment, None);
}

#[test]
fn test_doc_comment_does_not_leak_to_next_item() {
    let p = parse_ok("/// First.\nfn first() {}\nfn second() {}");
    let first = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    let second = match &p.items[1] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(first.doc_comment.as_deref(), Some("First."));
    assert_eq!(second.doc_comment, None);
}

#[test]
fn test_doc_comment_attaches_to_struct_field() {
    let p = parse_ok(
        "/// A point.\n\
         struct Point {\n\
           /// Horizontal.\n\
           x: i64,\n\
           /// Vertical.\n\
           y: i64,\n\
         }",
    );
    let s = match &p.items[0] {
        Item::StructDef(s) => s,
        _ => panic!("expected struct"),
    };
    assert_eq!(s.doc_comment.as_deref(), Some("A point."));
    assert_eq!(s.fields.len(), 2);
    assert_eq!(s.fields[0].name, "x");
    assert_eq!(s.fields[0].doc_comment.as_deref(), Some("Horizontal."));
    assert_eq!(s.fields[1].name, "y");
    assert_eq!(s.fields[1].doc_comment.as_deref(), Some("Vertical."));
}

#[test]
fn test_doc_comment_attaches_to_enum_variant() {
    let p = parse_ok(
        "/// A direction.\n\
         enum Direction {\n\
           /// Toward larger y.\n\
           Up,\n\
           /// Toward smaller y.\n\
           Down,\n\
         }",
    );
    let e = match &p.items[0] {
        Item::EnumDef(e) => e,
        _ => panic!("expected enum"),
    };
    assert_eq!(e.doc_comment.as_deref(), Some("A direction."));
    assert_eq!(e.variants.len(), 2);
    assert_eq!(e.variants[0].name, "Up");
    assert_eq!(
        e.variants[0].doc_comment.as_deref(),
        Some("Toward larger y.")
    );
    assert_eq!(e.variants[1].name, "Down");
    assert_eq!(
        e.variants[1].doc_comment.as_deref(),
        Some("Toward smaller y.")
    );
}

#[test]
fn test_module_doc_comment_attaches_to_program() {
    // Run of `//!` lines at the top of the file joins into
    // `Program.module_doc_comment`. Items below parse normally.
    let p = parse_ok(
        "//! A crate-level summary.\n\
         //! With a second sentence.\n\
         fn main() {}",
    );
    assert_eq!(
        p.module_doc_comment.as_deref(),
        Some("A crate-level summary.\nWith a second sentence.")
    );
    assert_eq!(p.items.len(), 1);
}

#[test]
fn test_no_module_doc_comment_yields_none() {
    let p = parse_ok("fn main() {}");
    assert_eq!(p.module_doc_comment, None);
}

#[test]
fn test_module_doc_does_not_clobber_first_item_doc() {
    // `//!` at the top, then a `///`-documented item: each should
    // attach to the right target.
    let p = parse_ok(
        "//! Module summary.\n\
         /// Doubles its argument.\n\
         fn double(n: i64) -> i64 { n * 2 }",
    );
    assert_eq!(p.module_doc_comment.as_deref(), Some("Module summary."));
    let f = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.doc_comment.as_deref(), Some("Doubles its argument."));
}

#[test]
fn test_doc_comment_attaches_to_function_param() {
    let p = parse_ok(
        "/// Doubles its argument.\n\
         fn double(\n\
           /// The number to double.\n\
           n: i64,\n\
         ) -> i64 { n * 2 }",
    );
    let f = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.doc_comment.as_deref(), Some("Doubles its argument."));
    assert_eq!(f.params.len(), 1);
    assert_eq!(f.params[0].name(), Some("n"));
    assert_eq!(
        f.params[0].doc_comment.as_deref(),
        Some("The number to double.")
    );
}

#[test]
fn test_doc_comment_attaches_to_some_function_params_only() {
    // Mix: documented, undocumented, documented. Item-level fn doc must
    // survive the per-param collection (mirrors the struct/enum fix).
    let p = parse_ok(
        "/// Sums three numbers.\n\
         fn sum3(\n\
           /// The base term.\n\
           a: i64,\n\
           b: i64,\n\
           /// The third term, optional in callers.\n\
           c: i64,\n\
         ) -> i64 { a + b + c }",
    );
    let f = match &p.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.doc_comment.as_deref(), Some("Sums three numbers."));
    assert_eq!(f.params.len(), 3);
    assert_eq!(f.params[0].name(), Some("a"));
    assert_eq!(f.params[0].doc_comment.as_deref(), Some("The base term."));
    assert_eq!(f.params[1].name(), Some("b"));
    assert_eq!(f.params[1].doc_comment, None);
    assert_eq!(f.params[2].name(), Some("c"));
    assert_eq!(
        f.params[2].doc_comment.as_deref(),
        Some("The third term, optional in callers.")
    );
}

// ── Block-like prefix in statement context ───────────────────────────
//
// A block-like expression (`if`, `while`, `for`, `loop`, `match`, `{...}`,
// `unsafe`, `seq`, `par`, `lock`, `providers`) at statement start must end
// the statement at its closing `}`. The next token, even one normally
// accepted as a postfix operator (`[`, `(`, `.`, `?`, `?.`), starts a
// fresh statement. Without this rule, `while cond { ... }\n[1, 2]` would
// be misparsed as `(while cond {...})[1, 2]` (subscript on the loop).

#[test]
fn test_while_then_array_tail_expr_parses() {
    // The original bug: tail expression starting with `[` after a block-like.
    parse_ok(
        "fn f() -> Array[i64, 2] {\n\
            let mut i = 0;\n\
            while i < 3 { i = i + 1; }\n\
            [-1, -1]\n\
         }",
    );
}

#[test]
fn test_if_then_array_tail_expr_parses() {
    parse_ok(
        "fn f() -> Array[i64, 2] {\n\
            if true { let _ = 1; }\n\
            [-1, -1]\n\
         }",
    );
}

#[test]
fn test_for_then_array_stmt_parses() {
    parse_ok(
        "fn f() {\n\
            for x in 0..3 { let _ = x; }\n\
            [1, 2];\n\
         }",
    );
}

#[test]
fn test_match_then_array_tail_expr_parses() {
    parse_ok(
        "fn f(x: i64) -> Array[i64, 2] {\n\
            match x { _ => {} }\n\
            [-1, -1]\n\
         }",
    );
}

#[test]
fn test_block_then_array_tail_expr_parses() {
    parse_ok(
        "fn f() -> Array[i64, 2] {\n\
            { let _ = 1; }\n\
            [-1, -1]\n\
         }",
    );
}

#[test]
fn test_block_like_postfix_still_works_in_value_context() {
    // In non-statement context (e.g., the RHS of a `let`), postfix
    // continuation after a block-like expression remains legal — the
    // statement-context rule applies only at statement start.
    parse_ok(
        "fn f(cond: bool) -> i64 {\n\
            let v = if cond { 1 } else { 2 } + 3;\n\
            v\n\
         }",
    );
}

// ── Uninitialized `let pat: T;` (round 12.1 — DA plumbing) ──────

#[test]
fn test_let_uninit_scalar() {
    let prog = parse_ok("fn main() { let x: i64; }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.body.stmts.len(), 1);
        match &f.body.stmts[0].kind {
            StmtKind::LetUninit {
                is_mut,
                name,
                ty: _,
                ..
            } => {
                assert!(!is_mut);
                assert_eq!(name, "x");
            }
            other => panic!("Expected StmtKind::LetUninit, got {:?}", other),
        }
    } else {
        panic!("Expected function");
    }
}

#[test]
fn test_let_uninit_mut() {
    let prog = parse_ok("fn main() { let mut buf: Array[u8, 16]; }");
    if let Item::Function(f) = &prog.items[0] {
        match &f.body.stmts[0].kind {
            StmtKind::LetUninit { is_mut, name, .. } => {
                assert!(*is_mut);
                assert_eq!(name, "buf");
            }
            other => panic!("Expected StmtKind::LetUninit, got {:?}", other),
        }
    }
}

#[test]
fn test_let_uninit_requires_type() {
    // `let x;` with no type and no initializer must be rejected — type
    // can't be inferred without an RHS.
    let (_, errors) = parse_with_errors("fn main() { let x; }");
    assert!(
        !errors.is_empty(),
        "expected parse error for `let x;` with no type or initializer"
    );
    let msg = errors[0].message.to_lowercase();
    assert!(
        msg.contains("type annotation") || msg.contains("uninitialized"),
        "diagnostic should mention type annotation: {}",
        errors[0].message
    );
}

#[test]
fn test_let_uninit_rejects_destructuring() {
    // Tuple destructure can't appear without an RHS — no value to destructure.
    let (_, errors) = parse_with_errors("fn main() { let (a, b): (i64, i64); }");
    assert!(
        !errors.is_empty(),
        "expected parse error for destructuring uninit let"
    );
    let msg = errors[0].message.to_lowercase();
    assert!(
        msg.contains("single name") || msg.contains("destructur"),
        "diagnostic should mention destructuring restriction: {}",
        errors[0].message
    );
}

#[test]
fn test_let_uninit_then_initializer_still_works() {
    // The initializer form must keep working after the uninit branch lands.
    parse_ok("fn main() { let x: i64 = 5; let y = 10; }");
}

#[test]
fn test_let_uninit_followed_by_assignment_parses() {
    // The follow-on assignment is just a regular `Assign` statement;
    // parser must not choke on the sequence.
    let prog = parse_ok("fn main() { let mut x: i64; x = 5; }");
    if let Item::Function(f) = &prog.items[0] {
        assert_eq!(f.body.stmts.len(), 2);
        assert!(matches!(f.body.stmts[0].kind, StmtKind::LetUninit { .. }));
        assert!(matches!(f.body.stmts[1].kind, StmtKind::Assign { .. }));
    }
}

// ── Raw-identifier escape r#NAME ────────────────────────────────────

#[test]
fn raw_ident_in_let_binding_strips_prefix_in_ast() {
    // The AST stores the bare identifier ("async"), not "r#async". The
    // raw-flag lives on the lexer token; the formatter re-emits the prefix.
    let prog = parse_ok("fn main() { let r#async = 1; }");
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    let stmt0 = &f.body.stmts[0];
    let pat = match &stmt0.kind {
        StmtKind::Let { pattern, .. } => pattern,
        _ => panic!("expected let stmt"),
    };
    match &pat.kind {
        PatternKind::Binding(name) => assert_eq!(name, "async"),
        other => panic!("expected binding pattern, got {:?}", other),
    }
}

#[test]
fn raw_ident_in_function_name_strips_prefix_in_ast() {
    let prog = parse_ok("fn r#try() -> i32 { 0 }");
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    assert_eq!(f.name, "try");
}

#[test]
fn raw_ident_in_field_access_strips_prefix() {
    let prog = parse_ok("fn main() { obj.r#await; }");
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!(),
    };
    let expr = match &f.body.stmts[0].kind {
        StmtKind::Expr(e) => e,
        _ => panic!(),
    };
    match &expr.kind {
        ExprKind::FieldAccess { field, .. } => assert_eq!(field, "await"),
        other => panic!("expected field access, got {:?}", other),
    }
}

#[test]
fn raw_ident_in_struct_field_strips_prefix() {
    let prog = parse_ok("struct S { r#move: i32, }");
    let s = match &prog.items[0] {
        Item::StructDef(s) => s,
        _ => panic!(),
    };
    assert_eq!(s.fields[0].name, "move");
}

// ── Round-trip: lex → parse → format → lex again ─────────────────────

fn lex_ident_tokens(source: &str) -> Vec<karac::token::Token> {
    karac::tokenize(source)
        .into_iter()
        .map(|st| st.token)
        .collect()
}

#[test]
fn raw_ident_roundtrip_through_formatter() {
    // `r#async` written by user, lexed, parsed, formatted, lexed again must
    // produce the same token sequence (specifically, `async` must come back
    // as Identifier { raw: true }).
    let src = "fn main() { let r#async = 1; }";
    let prog = parse_ok(src);
    let formatted = karac::formatter::format_program(&prog);
    assert!(
        formatted.contains("r#async"),
        "formatter must re-emit r# escape, got:\n{formatted}"
    );
    let original_tokens = lex_ident_tokens(src);
    let reformatted_tokens = lex_ident_tokens(&formatted);
    // Pull out the raw-identifier tokens from each sequence and compare.
    let raw_in_original: Vec<_> = original_tokens
        .iter()
        .filter(|t| matches!(t, karac::token::Token::Identifier { raw: true, .. }))
        .collect();
    let raw_in_reformatted: Vec<_> = reformatted_tokens
        .iter()
        .filter(|t| matches!(t, karac::token::Token::Identifier { raw: true, .. }))
        .collect();
    assert_eq!(
        raw_in_original, raw_in_reformatted,
        "round-trip must preserve raw-identifier tokens"
    );
}

#[test]
fn raw_ident_roundtrip_in_function_name_and_field() {
    let src = "fn r#try() { obj.r#await; }";
    let prog = parse_ok(src);
    let formatted = karac::formatter::format_program(&prog);
    assert!(formatted.contains("fn r#try"), "formatted:\n{formatted}");
    assert!(formatted.contains(".r#await"), "formatted:\n{formatted}");
}

// ── Anonymous-parameter focused diagnostic ──────────────────────────
//
// `fn f(Type)` and trait `fn f(Type)` are rejected with a focused
// diagnostic that names the type and offers `_: Type` / `arg: Type`
// fix-it forms. See design.md § Trait method parameter names — required
// (v60 item 53) and the matching phase-8 checklist entry.

fn assert_one_error_containing(errors: &[karac::parser::ParseError], substrings: &[&str]) {
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one parse error, got {errors:#?}"
    );
    let msg = &errors[0].message;
    for s in substrings {
        assert!(
            msg.contains(s),
            "expected error message to contain `{s}`; got `{msg}`"
        );
    }
}

#[test]
fn anon_param_in_free_fn_emits_focused_diagnostic() {
    let (_prog, errors) = parse_with_errors("fn free(i32) { 0 }");
    assert_one_error_containing(
        &errors,
        &[
            "E_FN_ANONYMOUS_PARAM",
            "function parameters require a name",
            "_: i32",
            "arg: i32",
        ],
    );
}

#[test]
fn anon_param_in_trait_method_emits_focused_diagnostic() {
    let (_prog, errors) = parse_with_errors("trait V { fn visit(ref self, Node); }");
    assert_one_error_containing(
        &errors,
        &[
            "E_TRAIT_METHOD_ANONYMOUS_PARAM",
            "trait method parameters require a name",
            "_: Node",
            "arg: Node",
        ],
    );
}

#[test]
fn anon_param_recovers_so_remaining_params_keep_parsing() {
    // After the focused diagnostic fires on the second param, the third
    // param's name+colon shape is still recognized — we get one error
    // for the anonymous param, not a cascade.
    let (_prog, errors) = parse_with_errors("fn f(a: i32, Node, c: bool) { }");
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one error from the anonymous param; got {errors:#?}"
    );
    assert!(errors[0].message.contains("E_FN_ANONYMOUS_PARAM"));
}

#[test]
fn anon_param_with_generics_renders_full_type() {
    let (_prog, errors) = parse_with_errors("fn f(Vec[i32]) { }");
    assert_one_error_containing(
        &errors,
        &["E_FN_ANONYMOUS_PARAM", "_: Vec[i32]", "arg: Vec[i32]"],
    );
}

#[test]
fn anon_param_with_ref_and_path() {
    let (_prog, errors) = parse_with_errors("fn f(ref Foo) { }");
    assert_one_error_containing(
        &errors,
        &["E_FN_ANONYMOUS_PARAM", "_: ref Foo", "arg: ref Foo"],
    );
}

#[test]
fn underscore_pattern_is_not_an_anon_param() {
    // `_: T` is the canonical "unused parameter" form — must keep parsing
    // without emitting the focused diagnostic.
    parse_ok("fn free(_: i32) { }");
    parse_ok("trait V { fn visit(ref self, _: Node); }");
}

#[test]
fn named_param_with_primitive_type_is_not_an_anon_param() {
    // `i32: i32` — a value-class binding that happens to share a name
    // with the primitive type. The peek-ahead guard sees `:` and skips
    // the anonymous-param probe entirely.
    parse_ok("fn f(i32: i32) -> i32 { i32 }");
}

#[test]
fn destructure_pattern_in_param_is_not_an_anon_param() {
    // Tuple destructure `(a, b): T` — the speculative parse_type won't
    // land on `,` / `)` (it'd require `T` after the tuple), so we fall
    // through to the existing pattern path.
    parse_ok("fn f((a, b): (i32, i32)) -> i32 { a + b }");
    // Struct destructure `Foo { a }: Foo` — same reasoning.
    parse_ok("struct Foo { a: i32 } fn f(Foo { a }: Foo) -> i32 { a }");
}

#[test]
fn multiple_anon_params_each_get_their_own_diagnostic() {
    let (_prog, errors) = parse_with_errors("fn f(i32, bool) { }");
    assert_eq!(errors.len(), 2, "expected one error per anonymous param");
    assert!(errors[0].message.contains("E_FN_ANONYMOUS_PARAM"));
    assert!(errors[1].message.contains("E_FN_ANONYMOUS_PARAM"));
    assert!(errors[0].message.contains("_: i32"));
    assert!(errors[1].message.contains("_: bool"));
}

// ── Concrete-type UFCS — `TypeName[T1, ...].method(...)` ────────
//
// Slice B of the parser CR (phase-2-parser-ast.md § "Path expression
// with generic args — concrete-type UFCS support"). The parser emits a
// single-segment `Path { generic_args: Some(...) }` when an uppercase
// identifier with `[…]` is followed by `.method(`, leaving collection-
// literal shapes (`[1, 2]`, `Vec[1, 2]`, etc.) untouched.

/// Extract the body's terminal expression from a single function program.
fn fn_terminal_expr(program: &Program, fn_name: &str) -> Expr {
    let Item::Function(f) = program
        .items
        .iter()
        .find(|i| matches!(i, Item::Function(f) if f.name == fn_name))
        .unwrap_or_else(|| panic!("function `{}` not found", fn_name))
    else {
        unreachable!()
    };
    (**f.body
        .final_expr
        .as_ref()
        .expect("expected tail expression"))
    .clone()
}

#[test]
fn concrete_type_ufcs_single_arg_path_with_generic_args() {
    // `Vec[i64].new()` parses as
    // `MethodCall { object: Path { segments: [Vec], generic_args: Some([i64]) }, method: "new", … }`.
    let prog = parse_ok("fn f() { Vec[i64].new() }");
    let expr = fn_terminal_expr(&prog, "f");
    let ExprKind::MethodCall {
        object,
        method,
        args,
        ..
    } = &expr.kind
    else {
        panic!("expected MethodCall, got {:?}", expr.kind);
    };
    assert_eq!(method, "new");
    assert!(args.is_empty());
    let ExprKind::Path {
        segments,
        generic_args,
    } = &object.kind
    else {
        panic!("expected Path object, got {:?}", object.kind);
    };
    assert_eq!(segments, &["Vec".to_string()]);
    let ga = generic_args.as_ref().expect("expected Some generic_args");
    assert_eq!(ga.len(), 1);
    let GenericArg::Type(te) = &ga[0] else {
        panic!("expected GenericArg::Type, got {:?}", ga[0]);
    };
    assert!(matches!(&te.kind, TypeKind::Path(p) if p.segments == ["i64"]));
}

#[test]
fn concrete_type_ufcs_multi_arg_path() {
    // Multi-arg generic args `HashMap[String, i32].default()`. The parser
    // doesn't recognize HashMap as a collection literal, so this exercises
    // the disambiguation path on a non-`Vec | Array | Set | Map` type name.
    let prog = parse_ok("fn f() { HashMap[String, i32].default() }");
    let expr = fn_terminal_expr(&prog, "f");
    let ExprKind::MethodCall { object, method, .. } = &expr.kind else {
        panic!("expected MethodCall");
    };
    assert_eq!(method, "default");
    let ExprKind::Path {
        segments,
        generic_args,
    } = &object.kind
    else {
        panic!("expected Path");
    };
    assert_eq!(segments, &["HashMap".to_string()]);
    let ga = generic_args.as_ref().expect("expected Some generic_args");
    assert_eq!(ga.len(), 2);
    let GenericArg::Type(te0) = &ga[0] else {
        panic!("expected GenericArg::Type at index 0");
    };
    let GenericArg::Type(te1) = &ga[1] else {
        panic!("expected GenericArg::Type at index 1");
    };
    assert!(matches!(&te0.kind, TypeKind::Path(p) if p.segments == ["String"]));
    assert!(matches!(&te1.kind, TypeKind::Path(p) if p.segments == ["i32"]));
}

#[test]
fn concrete_type_ufcs_nested_generics() {
    // `Vec[Map[K, V]].new()` — nested generics inside the type arg.
    // Balanced-bracket scan must skip over the inner `[K, V]` to reach the
    // outer `]` before validating the trailing `.method(`.
    let prog = parse_ok("fn f() { Vec[Map[K, V]].new() }");
    let expr = fn_terminal_expr(&prog, "f");
    let ExprKind::MethodCall { object, method, .. } = &expr.kind else {
        panic!("expected MethodCall");
    };
    assert_eq!(method, "new");
    let ExprKind::Path {
        segments,
        generic_args,
    } = &object.kind
    else {
        panic!("expected Path");
    };
    assert_eq!(segments, &["Vec".to_string()]);
    let ga = generic_args.as_ref().expect("expected Some generic_args");
    assert_eq!(ga.len(), 1);
    let GenericArg::Type(te) = &ga[0] else {
        panic!("expected GenericArg::Type");
    };
    let TypeKind::Path(inner_path) = &te.kind else {
        panic!("expected nested Path type");
    };
    assert_eq!(inner_path.segments, vec!["Map".to_string()]);
    let inner_ga = inner_path
        .generic_args
        .as_ref()
        .expect("nested generic_args");
    assert_eq!(inner_ga.len(), 2);
}

#[test]
fn concrete_type_ufcs_chained_method_calls() {
    // `Vec[i64].new().push(1)` — UFCS dispatch followed by another method
    // call. The Pratt loop's postfix `.` handler should attach `.push(1)`
    // to the `MethodCall { object: Path … new }` node naturally.
    let prog = parse_ok("fn f() { Vec[i64].new().push(1) }");
    let expr = fn_terminal_expr(&prog, "f");
    let ExprKind::MethodCall {
        object,
        method,
        args,
        ..
    } = &expr.kind
    else {
        panic!("expected outer MethodCall");
    };
    assert_eq!(method, "push");
    assert_eq!(args.len(), 1);
    let ExprKind::MethodCall {
        method: inner_method,
        object: inner_object,
        ..
    } = &object.kind
    else {
        panic!("expected inner MethodCall");
    };
    assert_eq!(inner_method, "new");
    assert!(matches!(
        &inner_object.kind,
        ExprKind::Path { segments, generic_args: Some(_) } if segments == &["Vec".to_string()]
    ));
}

#[test]
fn collection_literal_with_int_first_element_still_parses_as_literal() {
    // Regression guard: `Vec[1, 2].push(3)` must continue to parse as
    // `MethodCall { object: PrefixCollectionLiteral { Vec, [1, 2] }, … }`.
    // The disambiguation rule's first-token heuristic rejects integer-
    // literal start, so the UFCS branch falls through to the existing
    // PrefixCollectionLiteral path.
    let prog = parse_ok("fn f() { Vec[1, 2].push(3) }");
    let expr = fn_terminal_expr(&prog, "f");
    let ExprKind::MethodCall { object, .. } = &expr.kind else {
        panic!("expected MethodCall");
    };
    assert!(matches!(
        &object.kind,
        ExprKind::PrefixCollectionLiteral { type_name, .. } if type_name == "Vec"
    ));
}

#[test]
fn bare_array_literal_one_element_unchanged() {
    // Regression guard: `[1].len()` parses as a method call on a one-
    // element array literal, not as UFCS (no leading uppercase identifier
    // means the disambiguation branch never fires).
    let prog = parse_ok("fn f() { [1].len() }");
    let expr = fn_terminal_expr(&prog, "f");
    let ExprKind::MethodCall { object, .. } = &expr.kind else {
        panic!("expected MethodCall");
    };
    assert!(matches!(&object.kind, ExprKind::ArrayLiteral(_)));
}

#[test]
fn vec_repeat_literal_unchanged_under_ufcs_rule() {
    // `Vec[42; 100]` must continue to parse as RepeatLiteral, not UFCS.
    // The disambiguation lookahead bails on `[42` (first token `42` is an
    // integer literal — not a type-start), and the existing RepeatLiteral
    // branch consumes the bracket pair.
    let prog = parse_ok("fn main() { let x = Vec[42; 100]; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(value.kind, ExprKind::RepeatLiteral { .. }));
        } else {
            panic!("expected Let");
        }
    } else {
        panic!("expected Function");
    }
}

#[test]
fn standalone_ufcs_path_without_method_call_is_not_promoted() {
    // `let v: Vec[i64] = Vec[]` — the type annotation is a type position
    // (handled by parse_type) and the RHS `Vec[]` is an empty collection
    // literal. Neither triggers UFCS because no `.method(` follows the `]`.
    let prog = parse_ok("fn f() { let v: Vec[i64] = Vec[]; }");
    if let Item::Function(f) = &prog.items[0] {
        if let StmtKind::Let { value, .. } = &f.body.stmts[0].kind {
            assert!(matches!(
                &value.kind,
                ExprKind::PrefixCollectionLiteral { type_name, items, .. }
                    if type_name == "Vec" && items.is_empty()
            ));
        } else {
            panic!("expected Let");
        }
    } else {
        panic!("expected Function");
    }
}

// ── Slice / array patterns (phase 5.2 sub-item 1) ─────────────────────────
//
// Parser-side coverage for the new `PatternKind::Slice` variant. Sub-item 1
// lands AST + parser + resolver; the typechecker stub diagnostic is asserted
// separately in tests/typechecker.rs.

fn first_match_arm_pattern(prog: &Program) -> &Pattern {
    let f = match &prog.items[0] {
        Item::Function(f) => f,
        _ => panic!("expected function"),
    };
    let expr = f.body.final_expr.as_ref().expect("expected final expr");
    let arms = match &expr.kind {
        ExprKind::Match { arms, .. } => arms,
        _ => panic!("expected match expression"),
    };
    &arms[0].pattern
}

#[test]
fn test_slice_pattern_empty() {
    let prog = parse_ok("fn main() { match xs { [] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert!(prefix.is_empty());
            assert!(rest.is_none());
            assert!(suffix.is_empty());
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_single() {
    let prog = parse_ok("fn main() { match xs { [a] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert_eq!(prefix.len(), 1);
            assert!(matches!(&prefix[0].kind, PatternKind::Binding(n) if n == "a"));
            assert!(rest.is_none());
            assert!(suffix.is_empty());
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_fixed_arity() {
    let prog = parse_ok("fn main() { match xs { [a, b, c] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert_eq!(prefix.len(), 3);
            assert!(rest.is_none());
            assert!(suffix.is_empty());
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_head_only() {
    let prog = parse_ok("fn main() { match xs { [a, ..] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert_eq!(prefix.len(), 1);
            assert!(matches!(rest, Some(RestPattern::Ignored)));
            assert!(suffix.is_empty());
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_tail_only() {
    let prog = parse_ok("fn main() { match xs { [.., a] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert!(prefix.is_empty());
            assert!(matches!(rest, Some(RestPattern::Ignored)));
            assert_eq!(suffix.len(), 1);
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_both_ends_ignored_rest() {
    let prog = parse_ok("fn main() { match xs { [a, .., b] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert_eq!(prefix.len(), 1);
            assert!(matches!(rest, Some(RestPattern::Ignored)));
            assert_eq!(suffix.len(), 1);
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_single_bound_rest() {
    let prog = parse_ok("fn main() { match xs { [..rest] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert!(prefix.is_empty());
            assert!(matches!(rest, Some(RestPattern::Bound(n)) if n == "rest"));
            assert!(suffix.is_empty());
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_two_bound_rest() {
    let prog = parse_ok("fn main() { match xs { [head, ..tail] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert_eq!(prefix.len(), 1);
            assert!(matches!(&prefix[0].kind, PatternKind::Binding(n) if n == "head"));
            assert!(matches!(rest, Some(RestPattern::Bound(n)) if n == "tail"));
            assert!(suffix.is_empty());
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_multi_prefix_and_suffix() {
    let prog = parse_ok("fn main() { match xs { [a, b, .., c, d] => 0, _ => 1 } }");
    match &first_match_arm_pattern(&prog).kind {
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            assert_eq!(prefix.len(), 2);
            assert!(matches!(rest, Some(RestPattern::Ignored)));
            assert_eq!(suffix.len(), 2);
        }
        other => panic!("expected Slice, got {other:?}"),
    }
}

#[test]
fn test_slice_pattern_in_let() {
    // Let-pattern context also accepts slice syntax; the typechecker stub
    // will still reject semantically, but parsing should succeed.
    parse_ok("fn main() { let [a, b] = arr; }");
}

#[test]
fn test_slice_pattern_multiple_rest_rejected() {
    let (_prog, errors) =
        parse_with_errors("fn main() { match xs { [a, .., b, .., c] => 0, _ => 1 } }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("at most one `..` marker")),
        "expected multiple-`..` diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// ── GAT slice 9 — negative-space coverage (parser pins) ────────────
//
// Two pins documenting v1 parser-level rejections that belong to the
// GAT negative-space surface: higher-ranked-trait-bound `for<X>`
// syntax is not in v1, and effect-polymorphic GAT params
// (`type Mapped[U, with E]`) are rejected at parse time per slice 1.
// The latter is a re-verification — slice 1 already pins the
// diagnostic, slice 9 re-asserts it from the negative-space framing
// so a regression in the parser's effect-param rejection trips both
// the slice 1 and slice 9 surfaces.

#[test]
fn gat_slice9_b_higher_ranked_for_in_where_clause_rejected() {
    // `for<X> F.Mapped[X]: Send` would let an author quantify a where
    // clause over an unknown-at-callsite `X`. Kāra v1 has no
    // higher-ranked-trait-bound surface — `for` is a loop keyword,
    // not a binder — so the parser fails on the `for` token in
    // where-clause position. Slice 9 pins this rejection so a future
    // parser change that adds a binder-position `for` (e.g., for
    // existentials) cannot silently accept this surface and quietly
    // ship HRTBs.
    let (_prog, errors) = parse_with_errors(
        r#"
        trait Functor { type Mapped[U]; }
        fn f[F: Functor]() where for<X> F.Mapped[X]: Send {}
    "#,
    );
    assert!(
        !errors.is_empty(),
        "Expected parse error for `for<X>` in where-clause position; \
         parser accepted the higher-ranked syntax"
    );
}

#[test]
fn gat_slice9_c_effect_param_on_gat_decl_still_rejected() {
    // Re-verification of slice 1's `E_GAT_EFFECT_PARAM` diagnostic
    // surface. Slice 1 already pins this in
    // `gat_slice1_trait_assoc_type_rejects_effect_param`; the slice 9
    // pin is intentionally redundant so a regression that loosens the
    // parser's effect-param rejection trips a slice-9-framed failure
    // (signalling that the v1 negative-space contract was broken),
    // alongside the slice 1 failure (signalling that the
    // slice-1-as-shipped behaviour regressed). The two framings name
    // the same wound from two angles.
    let (_prog, errors) = parse_with_errors(
        r#"
        trait Functor {
            type Mapped[U, with E];
        }
    "#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_GAT_EFFECT_PARAM") && e.message.contains("with E")),
        "Expected E_GAT_EFFECT_PARAM diagnostic mentioning `with E`; \
         got: {errors:?}"
    );
    // The steering-suggestion surface is also load-bearing: the user
    // needs to know v1's prescribed shape (the carrying-method form).
    assert!(
        errors.iter().any(|e| e.message.contains("carrying method")),
        "Expected diagnostic to suggest the carrying-method form; \
         got: {errors:?}"
    );
}
