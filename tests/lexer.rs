use karac::token::Token;
use karac::tokenize;

/// Helper: extract just the Token variants from SpannedTokens for easier assertion.
fn tokens_only(source: &str) -> Vec<Token> {
    tokenize(source).into_iter().map(|st| st.token).collect()
}

fn ident(name: &str) -> Token {
    Token::Identifier {
        name: name.to_string(),
        raw: false,
    }
}

#[test]
fn test_basic_bindings() {
    let source = r#"
        let x = 5;
        let mut y = 10.5;
        let z = "hello world";
    "#;

    let tokens = tokens_only(source);
    let expected = vec![
        Token::Let,
        ident("x"),
        Token::Equal,
        Token::Integer(5, None),
        Token::Semicolon,
        Token::Let,
        Token::Mut,
        ident("y"),
        Token::Equal,
        Token::Float(10.5, None),
        Token::Semicolon,
        Token::Let,
        ident("z"),
        Token::Equal,
        Token::StringLiteral("hello world".to_string()),
        Token::Semicolon,
        Token::EOF,
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn test_struct_definition() {
    let source = r#"
        struct Entity {
            id: u64,
            name: String,
        }
    "#;

    let tokens = tokens_only(source);
    let expected = vec![
        Token::Struct,
        ident("Entity"),
        Token::LeftBrace,
        ident("id"),
        Token::Colon,
        ident("u64"),
        Token::Comma,
        ident("name"),
        Token::Colon,
        ident("String"),
        Token::Comma,
        Token::RightBrace,
        Token::EOF,
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn test_function_with_effects() {
    let source = r#"
        pub fn save_user(user: User) -> Result<(), Error>
            writes(UserDB) {
        }
    "#;

    let tokens = tokens_only(source);
    let expected = vec![
        Token::Pub,
        Token::Fn,
        ident("save_user"),
        Token::LeftParen,
        ident("user"),
        Token::Colon,
        ident("User"),
        Token::RightParen,
        Token::Arrow,
        ident("Result"),
        Token::LessThan,
        Token::LeftParen,
        Token::RightParen,
        Token::Comma,
        ident("Error"),
        Token::GreaterThan,
        Token::Writes,
        Token::LeftParen,
        ident("UserDB"),
        Token::RightParen,
        Token::LeftBrace,
        Token::RightBrace,
        Token::EOF,
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn test_enum_and_match() {
    let source = r#"
        enum Shape {
            Circle { radius: f64 },
            Rectangle { width: f64, height: f64 },
        }

        match shape {
            Circle { radius } => radius * radius,
            Rectangle { width, height } => width * height,
        }
    "#;

    let tokens = tokens_only(source);
    // Just verify key tokens are present
    assert_eq!(tokens[0], Token::Enum);
    assert_eq!(tokens[1], ident("Shape"));
    assert!(tokens.contains(&Token::Match));
    assert!(tokens.contains(&Token::FatArrow));
}

#[test]
fn test_trait_and_impl() {
    let source = r#"
        trait Processor {
            fn process(self, data: Data) -> Result[Output, Error] with _;
        }

        impl Processor for LocalProcessor {
            fn process(self, data: Data) -> Result[Output, Error] {
                Ok(compute(data))
            }
        }
    "#;

    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::Trait);
    assert!(tokens.contains(&Token::With));
    assert!(tokens.contains(&Token::Underscore));
    assert!(tokens.contains(&Token::Impl));
}

#[test]
fn test_effect_declarations() {
    let source = r#"
        effect resource UserDB: DatabaseProvider;
        effect group OrderProcessing = Validation + Fulfillment;
        transparent effect verb traces;
    "#;

    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::Effect);
    assert!(tokens.contains(&Token::Transparent));
    assert!(tokens.contains(&Token::Group));
}

#[test]
fn test_ownership_keywords() {
    let source = r#"
        fn first_word(s: ref String) -> ref String {
            s.split(' ').first()
        }

        struct Child {
            parent: weak Parent,
        }
    "#;

    let tokens = tokens_only(source);
    assert!(tokens.contains(&Token::Ref));
    assert!(tokens.contains(&Token::Weak));
}

#[test]
fn test_control_flow() {
    let source = r#"
        if condition {
            return value;
        } else {
            break;
        }
        while running {
            continue;
        }
        for item in items {
            loop { break; }
        }
    "#;

    let tokens = tokens_only(source);
    assert!(tokens.contains(&Token::If));
    assert!(tokens.contains(&Token::Else));
    assert!(tokens.contains(&Token::Return));
    assert!(tokens.contains(&Token::While));
    assert!(tokens.contains(&Token::For));
    assert!(tokens.contains(&Token::In));
    assert!(tokens.contains(&Token::Loop));
    assert!(tokens.contains(&Token::Break));
    assert!(tokens.contains(&Token::Continue));
}

#[test]
fn test_operators() {
    let source = "a && b || !c & d | e ^ f << g >> h % i ..= j .. k";
    let tokens = tokens_only(source);
    // Lexer still emits the legacy logical-symbol tokens; the parser rejects
    // them with a migration error pointing at the keyword forms.
    assert!(tokens.contains(&Token::AmpAmp));
    assert!(tokens.contains(&Token::PipePipe));
    assert!(tokens.contains(&Token::Bang));
    assert!(tokens.contains(&Token::Amp));
    assert!(tokens.contains(&Token::Pipe));
    assert!(tokens.contains(&Token::Caret));
    assert!(tokens.contains(&Token::LessLess));
    assert!(tokens.contains(&Token::GreaterGreater));
    assert!(tokens.contains(&Token::Percent));
    assert!(tokens.contains(&Token::DotDotEq));
    assert!(tokens.contains(&Token::DotDot));
}

#[test]
fn test_logical_keywords() {
    let source = "a and b or not c";
    let tokens = tokens_only(source);
    assert!(tokens.contains(&Token::And));
    assert!(tokens.contains(&Token::Or));
    assert!(tokens.contains(&Token::Not));
}

#[test]
fn test_move_keyword_reserved() {
    // `move` is a reserved keyword (used in the parser's closure-capture-mode
    // diagnostic). It cannot be used as an identifier.
    let tokens = tokens_only("move");
    assert!(tokens.contains(&Token::Move));
}

#[test]
fn test_path_separator_and_question() {
    // `.` is both the path separator and field/method access operator since v29;
    // the parser disambiguates by identifier case class.
    let source = "std.collections.HashMap value?";
    let tokens = tokens_only(source);
    assert!(tokens.contains(&Token::Dot));
    assert!(tokens.contains(&Token::Question));
}

#[test]
fn test_numeric_literals() {
    let source = "42 1_000_000 1.5 0xFF 0b1010 0o77";
    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::Integer(42, None));
    assert_eq!(tokens[1], Token::Integer(1_000_000, None));
    assert_eq!(tokens[2], Token::Float(1.5, None));
    assert_eq!(tokens[3], Token::Integer(0xFF, None));
    assert_eq!(tokens[4], Token::Integer(0b1010, None));
    assert_eq!(tokens[5], Token::Integer(0o77, None));
}

#[test]
fn test_unsafe_extern() {
    let source = r#"
        unsafe {
            let ptr = value;
        }
        extern "C" fn write(fd: i32) -> i32
            writes(FileSystem);
    "#;

    let tokens = tokens_only(source);
    assert!(tokens.contains(&Token::Unsafe));
    assert!(tokens.contains(&Token::Extern));
}

#[test]
fn test_layout_block() {
    let source = r#"
        layout entities: Collection[Entity] {
            group physics { position, velocity }
            group combat { health, armor }
        }
    "#;

    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::Layout);
    assert!(tokens.contains(&Token::Group));
}

#[test]
fn test_self_keywords() {
    let source = "self Self";
    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::SelfValue);
    assert_eq!(tokens[1], Token::SelfType);
}

#[test]
fn test_block_comments() {
    let source = "let /* this is a comment */ x = 5;";
    let tokens = tokens_only(source);
    let expected = vec![
        Token::Let,
        ident("x"),
        Token::Equal,
        Token::Integer(5, None),
        Token::Semicolon,
        Token::EOF,
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn test_nested_block_comments() {
    let source = "let /* outer /* nested */ still comment */ x = 5;";
    let tokens = tokens_only(source);
    let expected = vec![
        Token::Let,
        ident("x"),
        Token::Equal,
        Token::Integer(5, None),
        Token::Semicolon,
        Token::EOF,
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn test_span_tracking() {
    let source = "let x = 5;";
    let spanned_tokens = tokenize(source);

    // "let" starts at line 1, column 1
    assert_eq!(spanned_tokens[0].span.line, 1);
    assert_eq!(spanned_tokens[0].span.column, 1);
    assert_eq!(spanned_tokens[0].span.length, 3);

    // "x" starts at line 1, column 5
    assert_eq!(spanned_tokens[1].span.line, 1);
    assert_eq!(spanned_tokens[1].span.column, 5);
    assert_eq!(spanned_tokens[1].span.length, 1);
}

#[test]
fn test_alias_and_independent() {
    let source = r#"
        alias mylib.UserDB = theirlib.TheirDB;
        independent mylib.UserDB, theirlib.TheirDB;
    "#;

    let tokens = tokens_only(source);
    assert!(tokens.contains(&Token::Alias));
    assert!(tokens.contains(&Token::Independent));
}

#[test]
fn test_attributes() {
    let source = "#[no_rc]";
    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::Pound);
    assert_eq!(tokens[1], Token::LeftBracket);
    assert_eq!(tokens[2], ident("no_rc"));
    assert_eq!(tokens[3], Token::RightBracket);
}

#[test]
fn test_modules() {
    let source = r#"
        mod parser;
        use std.collections.HashMap;
        pub const MAX: i64 = 1024;
    "#;

    let tokens = tokens_only(source);
    assert!(tokens.contains(&Token::Mod));
    assert!(tokens.contains(&Token::Use));
    assert!(tokens.contains(&Token::Pub));
    assert!(tokens.contains(&Token::Const));
}

#[test]
fn test_type_keyword() {
    let source = "type UserId = u64;";
    let tokens = tokens_only(source);
    let expected = vec![
        Token::Type,
        ident("UserId"),
        Token::Equal,
        ident("u64"),
        Token::Semicolon,
        Token::EOF,
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn test_string_escape_sequences() {
    let source = r#""hello\nworld""#;
    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::StringLiteral("hello\nworld".to_string()));
}

#[test]
fn test_string_escape_tab_and_backslash() {
    let source = r#""col1\tcol2\\end""#;
    let tokens = tokens_only(source);
    assert_eq!(
        tokens[0],
        Token::StringLiteral("col1\tcol2\\end".to_string())
    );
}

#[test]
fn test_string_escape_quote() {
    let source = r#""say \"hello\"""#;
    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::StringLiteral("say \"hello\"".to_string()));
}

#[test]
fn test_doc_comments() {
    let source = "/// This is a doc comment\nfn main() {}";
    let tokens = tokens_only(source);
    assert_eq!(
        tokens[0],
        Token::DocComment("This is a doc comment".to_string())
    );
    assert_eq!(tokens[1], Token::Fn);
}

#[test]
fn test_regular_comments_still_skipped() {
    let source = "// regular comment\nlet x = 5;";
    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::Let);
}

#[test]
fn test_doc_comment_vs_regular_comment() {
    let source = "// skip this\n/// keep this\nlet x = 5;";
    let tokens = tokens_only(source);
    assert_eq!(tokens[0], Token::DocComment("keep this".to_string()));
    assert_eq!(tokens[1], Token::Let);
}

#[test]
fn test_module_doc_comments() {
    // `//!` lines emit a distinct ModuleDocComment token, separate from
    // regular `///` doc comments and from plain `//` line comments.
    let source = "//! Crate-level summary.\n//! Second line.\nfn main() {}";
    let tokens = tokens_only(source);
    assert_eq!(
        tokens[0],
        Token::ModuleDocComment("Crate-level summary.".to_string())
    );
    assert_eq!(
        tokens[1],
        Token::ModuleDocComment("Second line.".to_string())
    );
    assert_eq!(tokens[2], Token::Fn);
}

#[test]
fn test_char_literal_simple() {
    let tokens = tokens_only("'a'");
    assert_eq!(tokens[0], Token::CharLiteral('a'));
    assert_eq!(tokens[1], Token::EOF);
}

#[test]
fn test_char_literal_escape_sequences() {
    assert_eq!(tokens_only("'\\n'")[0], Token::CharLiteral('\n'));
    assert_eq!(tokens_only("'\\t'")[0], Token::CharLiteral('\t'));
    assert_eq!(tokens_only("'\\r'")[0], Token::CharLiteral('\r'));
    assert_eq!(tokens_only("'\\\\'")[0], Token::CharLiteral('\\'));
    assert_eq!(tokens_only("'\\''")[0], Token::CharLiteral('\''));
    assert_eq!(tokens_only("'\\0'")[0], Token::CharLiteral('\0'));
}

#[test]
fn test_char_literal_unicode_escape() {
    let tokens = tokens_only("'\\u{1F600}'");
    assert_eq!(tokens[0], Token::CharLiteral('\u{1F600}'));
}

#[test]
fn test_char_literal_in_binding() {
    let tokens = tokens_only("let c = 'x';");
    let expected = vec![
        Token::Let,
        ident("c"),
        Token::Equal,
        Token::CharLiteral('x'),
        Token::Semicolon,
        Token::EOF,
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn test_char_literal_unterminated() {
    let tokens = tokens_only("'a");
    assert!(matches!(tokens[0], Token::Error(_)));
}

#[test]
fn test_char_literal_unknown_escape() {
    let tokens = tokens_only("'\\q'");
    assert!(matches!(tokens[0], Token::Error(_)));
}

#[test]
fn test_dyn_keyword() {
    let tokens = tokens_only("dyn");
    assert_eq!(tokens, vec![Token::Dyn, Token::EOF]);
}

#[test]
fn test_dyn_is_reserved() {
    // dyn cannot be used as an identifier — it lexes as a keyword
    let tokens = tokens_only("let dyn = 5;");
    assert_eq!(
        tokens,
        vec![
            Token::Let,
            Token::Dyn,
            Token::Equal,
            Token::Integer(5, None),
            Token::Semicolon,
            Token::EOF,
        ]
    );
}

// ── Interpolated strings ─────────────────────────────────────────

use karac::token::InterpolationPart;

#[test]
fn test_interpolated_string_basic() {
    let tokens = tokens_only(r#"f"hello {name}""#);
    assert!(matches!(&tokens[0], Token::InterpolatedStringLiteral(_)));
    if let Token::InterpolatedStringLiteral(parts) = &tokens[0] {
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], InterpolationPart::Text(s) if s == "hello "));
        assert!(matches!(&parts[1], InterpolationPart::Expr(s) if s == "name"));
    }
}

#[test]
fn test_interpolated_string_multiple_exprs() {
    let tokens = tokens_only(r#"f"{a} and {b}""#);
    if let Token::InterpolatedStringLiteral(parts) = &tokens[0] {
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], InterpolationPart::Expr(s) if s == "a"));
        assert!(matches!(&parts[1], InterpolationPart::Text(s) if s == " and "));
        assert!(matches!(&parts[2], InterpolationPart::Expr(s) if s == "b"));
    }
}

#[test]
fn test_interpolated_string_no_exprs() {
    let tokens = tokens_only(r#"f"plain text""#);
    if let Token::InterpolatedStringLiteral(parts) = &tokens[0] {
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], InterpolationPart::Text(s) if s == "plain text"));
    }
}

// ── Multi-line strings ───────────────────────────────────────────

#[test]
fn test_multi_line_string() {
    let source = "let s = \"\"\"hello\nworld\"\"\"";
    let tokens = tokens_only(source);
    // Find the MultiStringLiteral token (might be at index 2 or 3 depending on parsing)
    let has_multi = tokens
        .iter()
        .any(|t| matches!(t, Token::MultiStringLiteral(_)));
    assert!(has_multi, "Expected MultiStringLiteral, got: {:?}", tokens);
}

// ── Defer/errdefer keywords ──────────────────────────────────────

#[test]
fn test_defer_keywords() {
    let tokens = tokens_only("defer errdefer");
    assert_eq!(tokens[0], Token::Defer);
    assert_eq!(tokens[1], Token::ErrDefer);
}

// ── Asm keywords ─────────────────────────────────────────────────

#[test]
fn test_asm_keywords() {
    let tokens = tokens_only("asm global_asm");
    assert_eq!(tokens[0], Token::Asm);
    assert_eq!(tokens[1], Token::GlobalAsm);
}

// ── New keywords (syntax spec §1.1) ─────────────────────────────

#[test]
fn test_visibility_private() {
    let tokens = tokens_only("pub private");
    assert_eq!(tokens[0], Token::Pub);
    assert_eq!(tokens[1], Token::Private);
}

#[test]
fn test_ownership_keywords_full() {
    let tokens = tokens_only("own ref weak lock");
    assert_eq!(tokens[0], Token::Own);
    assert_eq!(tokens[1], Token::Ref);
    assert_eq!(tokens[2], Token::Weak);
    assert_eq!(tokens[3], Token::Lock);
}

#[test]
fn test_effect_keywords_full() {
    let tokens = tokens_only(
        "effect resource verb reads writes sends receives allocates panics blocks suspends",
    );
    assert_eq!(tokens[0], Token::Effect);
    assert_eq!(tokens[1], Token::Resource);
    assert_eq!(tokens[2], Token::Verb);
    assert_eq!(tokens[3], Token::Reads);
    assert_eq!(tokens[4], Token::Writes);
    assert_eq!(tokens[5], Token::Sends);
    assert_eq!(tokens[6], Token::Receives);
    assert_eq!(tokens[7], Token::Allocates);
    assert_eq!(tokens[8], Token::Panics);
    assert_eq!(tokens[9], Token::Blocks);
    assert_eq!(tokens[10], Token::Suspends);
}

#[test]
fn test_effect_modifier_keywords() {
    let tokens = tokens_only("with transparent stable seq par yield");
    assert_eq!(tokens[0], Token::With);
    assert_eq!(tokens[1], Token::Transparent);
    assert_eq!(tokens[2], Token::Stable);
    assert_eq!(tokens[3], Token::Seq);
    assert_eq!(tokens[4], Token::Par);
    assert_eq!(tokens[5], Token::Yield);
}

#[test]
fn test_providers_is_contextual_identifier() {
    // Theme 4 follow-up (2026-05-10): `providers` is no longer
    // globally-reserved at the lexer level — it lexes as a regular
    // identifier so module names / function names / variable bindings
    // can use the bareword (e.g., `examples/parallax/src/providers.kara`
    // imported via `import providers.{...}`). The parser dispatches to
    // the `providers { R => e } in { body }` block shape contextually
    // when an identifier expression named "providers" is followed by
    // `{` — see `parse_identifier_expr`.
    let tokens = tokens_only("providers");
    assert_eq!(
        tokens[0],
        Token::Identifier {
            name: "providers".to_string(),
            raw: false,
        }
    );
}

// ── Compound assignment operators ───────────────────────────────

#[test]
fn test_compound_assignment_arithmetic() {
    let tokens = tokens_only("x += 1; y -= 2; z *= 3; w /= 4; v %= 5;");
    // x += 1 ;
    assert_eq!(tokens[1], Token::PlusEqual);
    // y -= 2 ;
    assert_eq!(tokens[5], Token::MinusEqual);
    // z *= 3 ;
    assert_eq!(tokens[9], Token::StarEqual);
    // w /= 4 ;
    assert_eq!(tokens[13], Token::SlashEqual);
    // v %= 5 ;
    assert_eq!(tokens[17], Token::PercentEqual);
}

#[test]
fn test_compound_assignment_bitwise() {
    let tokens = tokens_only("a &= b; c |= d; e ^= f; g <<= h; i >>= j;");
    assert_eq!(tokens[1], Token::AmpEqual);
    assert_eq!(tokens[5], Token::PipeEqual);
    assert_eq!(tokens[9], Token::CaretEqual);
    assert_eq!(tokens[13], Token::LessLessEqual);
    assert_eq!(tokens[17], Token::GreaterGreaterEqual);
}

#[test]
fn test_compound_vs_plain_operators() {
    // Ensure plain operators still work when not followed by '='
    let tokens = tokens_only("+ - * / % & | ^ << >>");
    assert_eq!(tokens[0], Token::Plus);
    assert_eq!(tokens[1], Token::Minus);
    assert_eq!(tokens[2], Token::Star);
    assert_eq!(tokens[3], Token::Slash);
    assert_eq!(tokens[4], Token::Percent);
    assert_eq!(tokens[5], Token::Amp);
    assert_eq!(tokens[6], Token::Pipe);
    assert_eq!(tokens[7], Token::Caret);
    assert_eq!(tokens[8], Token::LessLess);
    assert_eq!(tokens[9], Token::GreaterGreater);
}

// ── Unicode escapes in strings ──────────────────────────────────

#[test]
fn test_string_unicode_escape() {
    let tokens = tokens_only(r#""\u{1F600}""#);
    match &tokens[0] {
        Token::StringLiteral(s) => assert_eq!(s, "\u{1F600}"),
        other => panic!("Expected StringLiteral, got {:?}", other),
    }
}

#[test]
fn test_string_unicode_escape_ascii() {
    let tokens = tokens_only(r#""\u{41}""#);
    match &tokens[0] {
        Token::StringLiteral(s) => assert_eq!(s, "A"),
        other => panic!("Expected StringLiteral, got {:?}", other),
    }
}

#[test]
fn test_interpolated_string_unicode_escape() {
    use karac::token::InterpolationPart;
    let tokens = tokens_only(r#"f"hello \u{2764}""#);
    if let Token::InterpolatedStringLiteral(parts) = &tokens[0] {
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], InterpolationPart::Text(s) if s == "hello \u{2764}"));
    } else {
        panic!("Expected InterpolatedStringLiteral, got {:?}", tokens[0]);
    }
}

// ── Integer and float type suffixes ─────────────────────────────

use karac::token::{FloatSuffix, IntSuffix};

#[test]
fn test_integer_suffix_i32() {
    let tokens = tokens_only("42i32");
    assert_eq!(tokens[0], Token::Integer(42, Some(IntSuffix::I32)));
}

#[test]
fn test_integer_suffix_u8() {
    let tokens = tokens_only("255u8");
    assert_eq!(tokens[0], Token::Integer(255, Some(IntSuffix::U8)));
}

#[test]
fn test_integer_suffix_i128() {
    let tokens = tokens_only("100i128");
    assert_eq!(tokens[0], Token::Integer(100, Some(IntSuffix::I128)));
}

#[test]
fn test_integer_no_suffix() {
    let tokens = tokens_only("42");
    assert_eq!(tokens[0], Token::Integer(42, None));
}

#[test]
fn test_float_suffix_f32() {
    let tokens = tokens_only("1.5f32");
    assert_eq!(tokens[0], Token::Float(1.5, Some(FloatSuffix::F32)));
}

#[test]
fn test_float_suffix_f64() {
    let tokens = tokens_only("2.25f64");
    assert_eq!(tokens[0], Token::Float(2.25, Some(FloatSuffix::F64)));
}

#[test]
fn test_float_no_suffix() {
    let tokens = tokens_only("1.0");
    assert_eq!(tokens[0], Token::Float(1.0, None));
}

#[test]
fn test_hex_with_suffix() {
    let tokens = tokens_only("0xFFu32");
    assert_eq!(tokens[0], Token::Integer(0xFF, Some(IntSuffix::U32)));
}

#[test]
fn test_binary_with_suffix() {
    let tokens = tokens_only("0b1010i8");
    assert_eq!(tokens[0], Token::Integer(0b1010, Some(IntSuffix::I8)));
}

#[test]
fn test_octal_with_suffix() {
    let tokens = tokens_only("0o77u16");
    assert_eq!(tokens[0], Token::Integer(0o77, Some(IntSuffix::U16)));
}

#[test]
fn test_suffix_not_confused_with_identifier() {
    // "42i" should not match any suffix — i is not a valid suffix
    let tokens = tokens_only("42 i32_var");
    assert_eq!(tokens[0], Token::Integer(42, None));
    assert_eq!(tokens[1], ident("i32_var"));
}

// ── Float literal exponent notation ─────────────────────────────

#[test]
fn test_float_exponent_basic() {
    let tokens = tokens_only("1e10");
    assert_eq!(tokens[0], Token::Float(1e10, None));
}

#[test]
fn test_float_exponent_with_decimal() {
    let tokens = tokens_only("1.5e-3");
    assert_eq!(tokens[0], Token::Float(1.5e-3, None));
}

#[test]
fn test_float_exponent_large() {
    let tokens = tokens_only("6.022e23");
    assert_eq!(tokens[0], Token::Float(6.022e23, None));
}

#[test]
fn test_float_exponent_uppercase_positive_sign() {
    let tokens = tokens_only("2.5E+6");
    assert_eq!(tokens[0], Token::Float(2.5E+6, None));
}

#[test]
fn test_float_exponent_underscore_separators() {
    // underscores permitted in both mantissa and exponent digits
    let tokens = tokens_only("1_000e3");
    assert_eq!(tokens[0], Token::Float(1_000e3_f64, None));
}

#[test]
fn test_float_exponent_with_f32_suffix() {
    let tokens = tokens_only("2.5E+6f32");
    assert_eq!(tokens[0], Token::Float(2.5E+6_f64, Some(FloatSuffix::F32)));
}

#[test]
fn test_float_exponent_with_f64_suffix() {
    let tokens = tokens_only("1e10f64");
    assert_eq!(tokens[0], Token::Float(1e10_f64, Some(FloatSuffix::F64)));
}

#[test]
fn test_float_dot_then_exponent() {
    // `1.e10` → float `1.0e10` per design.md spec
    let tokens = tokens_only("1.e10");
    assert_eq!(tokens[0], Token::Float(1.0e10, None));
}

#[test]
fn test_float_exponent_no_digits_is_error() {
    // `1e` — no digits after e → error token
    let tokens = tokens_only("1e");
    matches!(tokens[0], Token::Error(_));
}

#[test]
fn test_float_exponent_sign_no_digits_is_error() {
    // `1e+` — sign but no digits → error token
    let tokens = tokens_only("1e+");
    matches!(tokens[0], Token::Error(_));
}

#[test]
fn test_float_exponent_negative_exponent() {
    let tokens = tokens_only("1.5e-10");
    assert_eq!(tokens[0], Token::Float(1.5e-10, None));
}

#[test]
fn test_f16_is_reserved_keyword() {
    let tokens = tokens_only("f16");
    assert_eq!(
        tokens,
        vec![
            Token::Error(
                "'f16' is a reserved keyword for a future numeric type; not available until Phase 7".to_string()
            ),
            Token::EOF,
        ]
    );
}

#[test]
fn test_bf16_is_reserved_keyword() {
    let tokens = tokens_only("bf16");
    assert_eq!(
        tokens,
        vec![
            Token::Error(
                "'bf16' is a reserved keyword for a future numeric type; not available until Phase 7".to_string()
            ),
            Token::EOF,
        ]
    );
}

#[test]
fn test_v60_reserved_hash_guarded_string() {
    // Per design.md § Reserved `#`-Guarded String Syntax (v60 item 11).
    // The forms `#"..."#`, `##"..."##`, `###"..."###`, etc. and any
    // multi-`#` cluster are reserved at v1; `#[attr]` continues to lex
    // unchanged.
    //
    // Single-`#` raw string.
    let tokens = tokens_only(r##"#"hello"#"##);
    assert_eq!(
        tokens[0],
        Token::Error(
            r##"`#`-guarded string syntax (`#"..."#`) is reserved for future use; not available in v1"##.to_string()
        ),
        "expected reserved-hash-string error, got {:?}", tokens[0]
    );
    // Double-`#` raw string.
    let tokens = tokens_only(r###"##"hello"##"###);
    assert_eq!(
        tokens[0],
        Token::Error(
            r###"`#`-guarded string syntax (`##"..."##`) is reserved for future use; not available in v1"###.to_string()
        ),
        "expected reserved-double-hash-string error, got {:?}", tokens[0]
    );
    // Bare `##` cluster (no string body).
    let tokens = tokens_only("##");
    assert_eq!(
        tokens[0],
        Token::Error(
            "`##` (multi-`#` cluster) is reserved for future use; only `#[...]` attribute syntax is recognized in v1".to_string()
        ),
        "expected reserved-multi-hash error, got {:?}", tokens[0]
    );
    // `#[attr]` form continues to lex as Pound + LeftBracket — unchanged.
    let tokens = tokens_only("#[derive(Eq)]");
    assert_eq!(tokens[0], Token::Pound);
    assert_eq!(tokens[1], Token::LeftBracket);
}

#[test]
fn test_v60_reserved_string_prefix_diagnostic() {
    // Per design.md § Reserved Single-Letter String-Prefix Syntax (v60 item 10).
    // Every ASCII single-letter prefix immediately followed by `"` is reserved
    // at v1, except `f"..."` (interpolated strings) and `c"..."` (C-string
    // literals). The lexer emits a focused reserved-prefix diagnostic and
    // consumes the string body for clean error recovery.
    for prefix in ['a', 'b', 'g', 'r', 'x', 'z', '_'] {
        let source = format!(r#"{prefix}"hello""#);
        let tokens = tokens_only(&source);
        assert_eq!(
            tokens,
            vec![
                Token::Error(format!(
                    "string prefix '{prefix}\"...\"' is reserved for future use; only `f\"...\"` and `c\"...\"` are recognized in v1"
                )),
                Token::EOF,
            ],
            "expected reserved-prefix error for '{prefix}\"...\"'",
        );
    }
    // `f"..."` is one recognized prefix — it must continue to lex as
    // an interpolated string, not as a reserved-prefix error.
    let tokens = tokens_only(r#"f"hello""#);
    assert!(
        matches!(tokens[0], Token::InterpolatedStringLiteral(_)),
        "expected f\"...\" to lex as InterpolatedStringLiteral, got {:?}",
        tokens[0]
    );
    // `c"..."` is the other recognized prefix — it lexes to
    // Token::CStringLiteral. Detailed coverage below.
    let tokens = tokens_only(r#"c"hello""#);
    assert!(
        matches!(tokens[0], Token::CStringLiteral { .. }),
        "expected c\"...\" to lex as CStringLiteral, got {:?}",
        tokens[0]
    );
}

#[test]
fn test_c_string_literal_ascii_body() {
    let tokens = tokens_only(r#"c"hello""#);
    match &tokens[0] {
        Token::CStringLiteral { bytes, source_len } => {
            assert_eq!(bytes, b"hello");
            assert_eq!(*source_len, 5, "source_len excludes prefix and quotes");
        }
        other => panic!("expected CStringLiteral, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_empty() {
    let tokens = tokens_only(r#"c"""#);
    match &tokens[0] {
        Token::CStringLiteral { bytes, source_len } => {
            assert!(bytes.is_empty());
            assert_eq!(*source_len, 0);
        }
        other => panic!("expected CStringLiteral, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_basic_escapes() {
    let tokens = tokens_only(r#"c"a\nb\tc\\d\"e""#);
    match &tokens[0] {
        Token::CStringLiteral { bytes, .. } => {
            assert_eq!(bytes, b"a\nb\tc\\d\"e");
        }
        other => panic!("expected CStringLiteral, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_hex_byte_escape() {
    // \xHH lets the user inject arbitrary bytes — including non-UTF-8
    // sequences a regular `String` couldn't carry.
    let tokens = tokens_only(r#"c"\x7F\xFF""#);
    match &tokens[0] {
        Token::CStringLiteral { bytes, .. } => {
            assert_eq!(bytes, &[0x7F, 0xFF]);
        }
        other => panic!("expected CStringLiteral, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_unicode_escape_encodes_utf8() {
    // U+00E9 (é) → 2 bytes (0xC3 0xA9) in UTF-8.
    let tokens = tokens_only(r#"c"caf\u{00E9}""#);
    match &tokens[0] {
        Token::CStringLiteral { bytes, .. } => {
            assert_eq!(bytes, &[b'c', b'a', b'f', 0xC3, 0xA9]);
        }
        other => panic!("expected CStringLiteral, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_multibyte_codepoint_in_body() {
    // A non-escaped multi-byte codepoint in the body encodes to its UTF-8
    // bytes (parity with regular `String`).
    let tokens = tokens_only("c\"é\"");
    match &tokens[0] {
        Token::CStringLiteral { bytes, .. } => {
            assert_eq!(bytes, &[0xC3, 0xA9]);
        }
        other => panic!("expected CStringLiteral, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_interior_nul_escape_zero_rejected() {
    let tokens = tokens_only(r#"c"a\0b""#);
    match &tokens[0] {
        Token::Error(msg) => {
            assert!(msg.contains("E_INTERIOR_NUL_IN_C_STRING"), "got: {msg}");
        }
        other => panic!("expected Error token, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_interior_nul_hex_escape_rejected() {
    let tokens = tokens_only(r#"c"a\x00b""#);
    match &tokens[0] {
        Token::Error(msg) => {
            assert!(msg.contains("E_INTERIOR_NUL_IN_C_STRING"), "got: {msg}");
        }
        other => panic!("expected Error token, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_interior_nul_unicode_escape_rejected() {
    let tokens = tokens_only(r#"c"a\u{0}b""#);
    match &tokens[0] {
        Token::Error(msg) => {
            assert!(msg.contains("E_INTERIOR_NUL_IN_C_STRING"), "got: {msg}");
        }
        other => panic!("expected Error token, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_invalid_hex_escape() {
    let tokens = tokens_only(r#"c"\xZZ""#);
    match &tokens[0] {
        Token::Error(msg) => {
            assert!(
                msg.contains("hex digit") || msg.contains("hex digits"),
                "got: {msg}"
            );
        }
        other => panic!("expected Error token, got {other:?}"),
    }
}

#[test]
fn test_c_string_literal_unterminated() {
    let tokens = tokens_only(r#"c"hello"#);
    assert!(
        matches!(tokens[0], Token::Error(ref msg) if msg.contains("Unterminated")),
        "got {:?}",
        tokens[0]
    );
}

#[test]
fn test_v60_reserved_for_future_use_keywords() {
    // Per design.md § Reserved-for-Future-Use Keywords (v60 item 9). Each must
    // be rejected at the lexer level so they cannot be used as identifiers.
    for keyword in [
        "gen", "become", "do", "final", "override", "priv", "typeof", "virtual", "async", "await",
        "comptime", "pure", "box",
    ] {
        let tokens = tokens_only(keyword);
        assert_eq!(
            tokens,
            vec![
                Token::Error(format!(
                    "'{keyword}' is reserved for future use and cannot be used as an identifier"
                )),
                Token::EOF,
            ],
            "expected reserved-keyword error for '{keyword}'",
        );
    }
}

#[test]
fn test_integer_stays_integer_without_exponent() {
    // plain integer unchanged
    let tokens = tokens_only("42");
    assert_eq!(tokens[0], Token::Integer(42, None));
}

#[test]
fn test_float_exponent_zero() {
    let tokens = tokens_only("0e0");
    assert_eq!(tokens[0], Token::Float(0.0, None));
}

// ── Raw-identifier escape r#NAME ────────────────────────────────

fn raw_ident(name: &str) -> Token {
    Token::Identifier {
        name: name.to_string(),
        raw: true,
    }
}

#[test]
fn test_raw_ident_basic() {
    // `async` is reserved-for-future-use; `r#async` lexes through as a plain
    // identifier with raw=true, bypassing the keyword check.
    let tokens = tokens_only("r#async");
    assert_eq!(tokens[0], raw_ident("async"));
    assert_eq!(tokens[1], Token::EOF);
}

#[test]
fn test_raw_ident_bypasses_normal_keyword() {
    // `fn` is a real keyword; `r#fn` produces a plain identifier named "fn".
    let tokens = tokens_only("r#fn");
    assert_eq!(tokens[0], raw_ident("fn"));
}

#[test]
fn test_raw_ident_value_class_preserved() {
    // The case class is determined by the identifier portion *after* `r#`.
    // `r#async` → lowercase first letter (Value-class shape).
    let tokens = tokens_only("r#async r#Async");
    assert_eq!(tokens[0], raw_ident("async"));
    assert_eq!(tokens[1], raw_ident("Async"));
}

#[test]
fn test_raw_ident_in_let_binding() {
    let tokens = tokens_only("let r#async = 1;");
    assert_eq!(tokens[0], Token::Let);
    assert_eq!(tokens[1], raw_ident("async"));
    assert_eq!(tokens[2], Token::Equal);
    assert_eq!(tokens[3], Token::Integer(1, None));
    assert_eq!(tokens[4], Token::Semicolon);
}

#[test]
fn test_raw_ident_in_field_access() {
    // `obj.r#await` — field access; `await` is a reserved-future word, raw lets
    // it appear as a field name.
    let tokens = tokens_only("obj.r#await");
    assert_eq!(tokens[0], ident("obj"));
    assert_eq!(tokens[1], Token::Dot);
    assert_eq!(tokens[2], raw_ident("await"));
}

#[test]
fn test_raw_ident_path_segment() {
    // Each path segment may independently carry r#.
    let tokens = tokens_only("std.r#async_io");
    assert_eq!(tokens[0], ident("std"));
    assert_eq!(tokens[1], Token::Dot);
    assert_eq!(tokens[2], raw_ident("async_io"));
}

#[test]
fn test_raw_ident_in_generic_arg() {
    let tokens = tokens_only("Foo[r#async]");
    assert_eq!(tokens[0], ident("Foo"));
    assert_eq!(tokens[1], Token::LeftBracket);
    assert_eq!(tokens[2], raw_ident("async"));
    assert_eq!(tokens[3], Token::RightBracket);
}

#[test]
fn test_raw_ident_with_digits() {
    // Trailing digits and underscores are part of the identifier portion.
    let tokens = tokens_only("r#try2 r#await_v2");
    assert_eq!(tokens[0], raw_ident("try2"));
    assert_eq!(tokens[1], raw_ident("await_v2"));
}

#[test]
fn test_raw_ident_rejects_self() {
    let tokens = tokens_only("r#self");
    assert!(matches!(&tokens[0], Token::Error(msg) if msg.contains("structural marker")));
}

#[test]
fn test_raw_ident_rejects_self_type() {
    let tokens = tokens_only("r#Self");
    assert!(matches!(&tokens[0], Token::Error(msg) if msg.contains("structural marker")));
}

#[test]
fn test_raw_ident_rejects_underscore() {
    let tokens = tokens_only("r#_");
    assert!(matches!(&tokens[0], Token::Error(msg) if msg.contains("structural marker")));
}

#[test]
fn test_raw_ident_rejects_mut_pub_ref_own() {
    for marker in [
        "mut", "pub", "ref", "own", "priv", "private", "mod", "super", "crate",
    ] {
        let src = format!("r#{marker}");
        let tokens = tokens_only(&src);
        assert!(
            matches!(&tokens[0], Token::Error(msg) if msg.contains("structural marker")),
            "expected E_RAW_IDENT_NOT_ALLOWED for '{marker}'",
        );
    }
}

#[test]
fn test_raw_prefix_on_digit_falls_through() {
    // `r#1foo` — `r#` not followed by an identifier-start byte. The lexer must
    // NOT enter the raw path; the existing diagnostic stack handles it.
    // Expect: `r` lexes as identifier, `#` lexes as Pound, then 1 / foo lex
    // as their normal forms. No new error needed here.
    let tokens = tokens_only("r#1foo");
    assert_eq!(tokens[0], ident("r"));
    assert_eq!(tokens[1], Token::Pound);
}

#[test]
fn test_raw_double_hash_falls_through_to_reserved_hash_string() {
    // `r##` — `r#` followed by another `#` (not alpha). Falls back to the
    // existing path: lone `r` identifier + reserved hash-cluster diagnostic.
    let tokens = tokens_only("r##");
    assert_eq!(tokens[0], ident("r"));
    assert!(matches!(&tokens[1], Token::Error(msg) if msg.contains("reserved")));
}

#[test]
fn test_raw_identifier_does_not_intercept_r_string_prefix() {
    // `r"..."` is the reserved-string-prefix path — must not enter the raw
    // identifier path.
    let tokens = tokens_only(r#"r"hello""#);
    assert!(matches!(&tokens[0], Token::Error(msg) if msg.contains("reserved")));
}

// ── Non-ASCII identifier diagnostics (design.md § Identifiers — Unicode) ────
//
// Non-ASCII identifiers are deferred to a future edition. The lexer must emit
// a single focused diagnostic per would-be identifier — not a per-byte cascade
// of garbage from interpreting UTF-8 continuation bytes as Latin-1 chars.

#[test]
fn test_non_ascii_identifier_start_emits_one_error() {
    // `αβγ` — three 2-byte codepoints, all letters. Pre-fix this would emit
    // six "Unexpected character" tokens with bogus bytes-as-chars. Post-fix it
    // is a single error that names the actual codepoint.
    let tokens = tokens_only("αβγ");
    assert_eq!(tokens.len(), 2, "expected one error + EOF, got {tokens:?}");
    let Token::Error(msg) = &tokens[0] else {
        panic!("expected Token::Error, got {:?}", tokens[0]);
    };
    assert!(msg.contains("non-ASCII"), "msg = {msg}");
    assert!(
        msg.contains('α'),
        "diagnostic should name the actual codepoint, got {msg}"
    );
    assert!(msg.contains("deferred") || msg.contains("ASCII"));
    assert!(matches!(tokens[1], Token::EOF));
}

#[test]
fn test_non_ascii_identifier_mid_word_emits_one_error() {
    // `kāra` — ASCII prefix, non-ASCII letter, ASCII suffix. The whole
    // would-be identifier must be consumed as one error token.
    let tokens = tokens_only("kāra");
    assert_eq!(tokens.len(), 2, "expected one error + EOF, got {tokens:?}");
    let Token::Error(msg) = &tokens[0] else {
        panic!("expected Token::Error, got {:?}", tokens[0]);
    };
    assert!(msg.contains("non-ASCII"), "msg = {msg}");
    assert!(
        msg.contains('ā'),
        "diagnostic should name the actual codepoint, got {msg}"
    );
}

#[test]
fn test_non_ascii_identifier_in_let_recovery() {
    // `let kāra = 1;` — surrounding tokens must lex normally; only the
    // identifier produces the diagnostic.
    let tokens = tokens_only("let kāra = 1;");
    assert_eq!(tokens[0], Token::Let);
    assert!(matches!(&tokens[1], Token::Error(msg) if msg.contains("non-ASCII")));
    assert_eq!(tokens[2], Token::Equal);
    assert_eq!(tokens[3], Token::Integer(1, None));
    assert_eq!(tokens[4], Token::Semicolon);
    assert_eq!(tokens[5], Token::EOF);
}

#[test]
fn test_non_ascii_non_letter_emits_clean_unexpected_character() {
    // `🚀` is a 4-byte UTF-8 codepoint that is not a letter. We must emit a
    // single "Unexpected character" with the actual codepoint, not four
    // separate errors interpreting each byte as Latin-1.
    let tokens = tokens_only("🚀");
    assert_eq!(tokens.len(), 2, "expected one error + EOF, got {tokens:?}");
    let Token::Error(msg) = &tokens[0] else {
        panic!("expected Token::Error, got {:?}", tokens[0]);
    };
    assert!(msg.contains("Unexpected character"), "msg = {msg}");
    assert!(
        msg.contains('🚀'),
        "diagnostic should name the actual codepoint, got {msg}"
    );
}

#[test]
fn test_non_ascii_in_string_literal_still_lexes() {
    // String-literal bodies are a separate path — non-ASCII identifiers being
    // a parse error must not break unrelated literal lexing.
    let tokens = tokens_only("let s = \"hello\";");
    assert_eq!(tokens[0], Token::Let);
    assert_eq!(tokens[1], ident("s"));
    assert_eq!(tokens[2], Token::Equal);
    assert!(matches!(&tokens[3], Token::StringLiteral(s) if s == "hello"));
    assert_eq!(tokens[4], Token::Semicolon);
}

#[test]
fn test_string_literal_preserves_non_ascii_codepoints() {
    // The lexed value must equal the source body verbatim — multi-byte UTF-8
    // codepoints land as one `char`, not as a sequence of bytes-as-chars.
    let tokens = tokens_only(r#""Kāra 日本語""#);
    let Token::StringLiteral(s) = &tokens[0] else {
        panic!("expected StringLiteral, got {:?}", tokens[0]);
    };
    assert_eq!(s, "Kāra 日本語");
    // K, ā, r, a, ' ', 日, 本, 語 — 8 codepoints (would be 14 bytes pre-fix).
    assert_eq!(s.chars().count(), 8);
}

#[test]
fn test_char_literal_holds_non_ascii_codepoint() {
    // `'ā'` must yield CharLiteral('ā'), not a per-byte error or a misdecoded char.
    let tokens = tokens_only("'ā'");
    assert_eq!(tokens[0], Token::CharLiteral('ā'));
    assert_eq!(tokens[1], Token::EOF);
}

#[test]
fn test_char_literal_emoji() {
    // 4-byte codepoint in a char literal.
    let tokens = tokens_only("'🚀'");
    assert_eq!(tokens[0], Token::CharLiteral('🚀'));
}

#[test]
fn test_multi_string_preserves_non_ascii() {
    // Triple-quoted multi-line strings go through a separate body path.
    let tokens = tokens_only(r#""""hello αβγ""""#);
    let Token::MultiStringLiteral(s) = &tokens[0] else {
        panic!("expected MultiStringLiteral, got {:?}", tokens[0]);
    };
    assert_eq!(s, "hello αβγ");
}

#[test]
fn test_interpolated_string_preserves_non_ascii() {
    // Both the text segment and an expr segment must round-trip non-ASCII.
    use karac::token::InterpolationPart;
    let tokens = tokens_only(r#"f"hi {名前} 日本語""#);
    let Token::InterpolatedStringLiteral(parts) = &tokens[0] else {
        panic!("expected InterpolatedStringLiteral, got {:?}", tokens[0]);
    };
    assert_eq!(parts.len(), 3);
    assert!(matches!(&parts[0], InterpolationPart::Text(s) if s == "hi "));
    assert!(matches!(&parts[1], InterpolationPart::Expr(s) if s == "名前"));
    assert!(matches!(&parts[2], InterpolationPart::Text(s) if s == " 日本語"));
}

#[test]
fn test_non_ascii_in_line_comment_skipped() {
    // Comments are byte-skipped through to `\n`; UTF-8 in a comment must not
    // leak diagnostics into the token stream.
    let tokens = tokens_only("// Kāra greeting\nlet x = 1;");
    assert_eq!(tokens[0], Token::Let);
    assert_eq!(tokens[1], ident("x"));
    assert_eq!(tokens[2], Token::Equal);
    assert_eq!(tokens[3], Token::Integer(1, None));
}

// ── Reserved fragment-specifier identifier namespace ──────────────
// Per design.md § Reserved Fragment-Specifier Identifier Namespace
// (v60 item 62). `expr_<NNNN>` for NNNN in 2020..=2099 is reserved at v1.

fn reserved_fragment_specifier_error(name: &str) -> Token {
    Token::Error(format!(
        "'{name}' is a reserved identifier name; this naming convention is reserved for future edition-versionable syntax categories in macros / comptime fragment specifiers — use 'r#{name}' if you need this exact identifier today, or rename to a non-year-suffixed form"
    ))
}

#[test]
fn test_reserved_fragment_specifier_rejected_in_let() {
    // Boundary years and a mid-range year all reject.
    for year in ["2020", "2026", "2099"] {
        let source = format!("let expr_{year} = 1;");
        let tokens = tokens_only(&source);
        let name = format!("expr_{year}");
        assert_eq!(tokens[0], Token::Let);
        assert_eq!(
            tokens[1],
            reserved_fragment_specifier_error(&name),
            "expected reservation error for '{name}'",
        );
        assert_eq!(tokens[2], Token::Equal);
        assert_eq!(tokens[3], Token::Integer(1, None));
        assert_eq!(tokens[4], Token::Semicolon);
    }
}

#[test]
fn test_reserved_fragment_specifier_rejected_at_fn_name() {
    // The reservation fires at the identifier-token boundary, regardless of
    // syntactic position. `fn expr_2026() {}` rejects on the function name.
    let tokens = tokens_only("fn expr_2026() { }");
    assert_eq!(tokens[0], Token::Fn);
    assert_eq!(tokens[1], reserved_fragment_specifier_error("expr_2026"),);
}

#[test]
fn test_reserved_fragment_specifier_rejected_at_struct_field() {
    // Struct field names lex as plain identifiers, so the reservation fires
    // there too.
    let tokens = tokens_only("struct S { expr_2030: i64 }");
    assert_eq!(tokens[0], Token::Struct);
    assert_eq!(tokens[1], ident("S"));
    assert_eq!(tokens[2], Token::LeftBrace);
    assert_eq!(tokens[3], reserved_fragment_specifier_error("expr_2030"),);
}

#[test]
fn test_reserved_fragment_specifier_diagnostic_contains_both_fix_its() {
    // Diagnostic shape (slice 5): help line offers the raw-escape fix-it
    // and the rename fix-it, both inline in the message.
    let tokens = tokens_only("let expr_2026 = 1;");
    let Token::Error(msg) = &tokens[1] else {
        panic!("expected reservation error, got {:?}", tokens[1]);
    };
    assert!(
        msg.contains("'r#expr_2026'"),
        "missing raw-escape fix-it: {msg}"
    );
    assert!(
        msg.contains("rename to a non-year-suffixed form"),
        "missing rename fix-it: {msg}",
    );
}

#[test]
fn test_reserved_fragment_specifier_negative_cases_accept() {
    // Each of these must lex as an ordinary identifier — covering every
    // exit condition of the namespace check.
    let cases = [
        // No `expr_` prefix.
        ("x", "x"),
        // Different prefix.
        ("expression", "expression"),
        // `expr_` prefix but non-year suffix.
        ("expr_v2", "expr_v2"),
        // Different prefix containing a year.
        ("version_2026", "version_2026"),
        // `expr_` prefix with year out of reserved range (low).
        ("expr_1999", "expr_1999"),
        // `expr_` prefix with year out of reserved range (high).
        ("expr_3000", "expr_3000"),
        // 4 digits but not a year-shaped value at boundary just below 2020.
        ("expr_2019", "expr_2019"),
        // 4 digits just above 2099.
        ("expr_2100", "expr_2100"),
        // Wrong digit count — 3 digits.
        ("expr_202", "expr_202"),
        // Wrong digit count — 5 digits.
        ("expr_20260", "expr_20260"),
    ];
    for (source, expected_name) in cases {
        let tokens = tokens_only(&format!("let {source} = 1;"));
        assert_eq!(tokens[0], Token::Let);
        assert_eq!(
            tokens[1],
            ident(expected_name),
            "expected '{source}' to lex as plain identifier",
        );
        assert_eq!(tokens[2], Token::Equal);
    }
}

#[test]
fn test_reserved_fragment_specifier_raw_escape_exempted() {
    // `r#expr_2026` lexes as a raw-escaped identifier with name `expr_2026`
    // and `raw=true`. The reservation check lives in `identifier()`; the raw
    // path is structurally separate.
    let tokens = tokens_only("let r#expr_2026 = 1;");
    assert_eq!(tokens[0], Token::Let);
    assert_eq!(tokens[1], raw_ident("expr_2026"));
    assert_eq!(tokens[2], Token::Equal);
    assert_eq!(tokens[3], Token::Integer(1, None));
    assert_eq!(tokens[4], Token::Semicolon);
}

#[test]
fn test_reserved_fragment_specifier_fn_with_non_year_suffix_accepts() {
    // `fn expr_v2()` is the user-facing rename fix-it shape; it must lex
    // through as a plain identifier with no diagnostic.
    let tokens = tokens_only("fn expr_v2() { }");
    assert_eq!(tokens[0], Token::Fn);
    assert_eq!(tokens[1], ident("expr_v2"));
    assert_eq!(tokens[2], Token::LeftParen);
}
