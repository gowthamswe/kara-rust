// src/parser.rs

//! Recursive-descent parser for Kāra source code.
//! Produces an AST from a token stream with error recovery and multi-error reporting.

use crate::ast::*;
use crate::lexer::{
    classify_ident, suggest_const_name, suggest_type_name, suggest_value_name, IdentClass,
};
use crate::token::{Span, SpannedToken, Token};

mod exprs;
mod items;

// ── Parse Errors ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}",
            self.span.line, self.span.column, self.message
        )
    }
}

pub struct ParseResult {
    pub program: Program,
    pub errors: Vec<ParseError>,
}

/// Surrounding signature kind for parameter parsing — selects between the
/// trait-method and free-function anonymous-parameter diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FnContext {
    /// `fn` inside a `trait { ... }` body. Drives `E_TRAIT_METHOD_ANONYMOUS_PARAM`.
    TraitMethod,
    /// Free function, impl method, or extern function. Drives
    /// `E_FN_ANONYMOUS_PARAM`. Impl methods reuse the free-function
    /// diagnostic per design.md § Trait method parameter names — required;
    /// the rule only requires *trait declarations* to name parameters,
    /// but the focused diagnostic for free fns helps catch the equivalent
    /// type-only paste typo.
    Function,
}

// `.` is both the path separator and the field/method access operator. The parser
// disambiguates using the case class of the initial identifier: Type- and Const-class
// names (uppercase leading letter) start paths; Value-class names (lowercase) start
// postfix chains. See docs/design.md § Identifiers and Naming.
pub(crate) fn starts_upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

// ── Parser ───────────────────────────────────────────────────────

pub struct Parser {
    pub(crate) tokens: Vec<SpannedToken>,
    pub(crate) pos: usize,
    pub(crate) errors: Vec<ParseError>,
    /// Active labels for disambiguating `break label` vs `break value` and
    /// for routing labeled-block label scopes. Each entry carries a
    /// `LabelKind` tag (`Loop` for labeled loops, `Block` for labeled
    /// blocks) — the kind is consulted by the resolver, not the parser,
    /// but the parser tracks it so that `is_known_label` lookups (which
    /// disambiguate `break label expr`) work uniformly across both label
    /// kinds. Pushed at the entry to a labeled construct, popped on exit.
    pub(crate) loop_labels: Vec<(String, LabelKind)>,
    /// Doc-comment text accumulated by the leading-`///` collection at the
    /// top of `parse_item`. Each item-construction site calls
    /// `Self::take_pending_doc` when filling the new node's `doc_comment`
    /// field. Cleared after consumption so a subsequent item without docs
    /// gets `None`.
    pub(crate) pending_doc: Option<String>,
    /// Stack of `FnContext`s for the function-like signature we are currently
    /// parsing parameters for. Drives the anonymous-parameter focused
    /// diagnostic: trait-method bodies emit `E_TRAIT_METHOD_ANONYMOUS_PARAM`
    /// while free / impl / extern function bodies emit
    /// `E_FN_ANONYMOUS_PARAM`. Empty when we are not inside a parameter
    /// list (e.g., parsing a struct field or top-level expression).
    pub(crate) fn_context_stack: Vec<FnContext>,
    /// Stack of effect-variable names declared in the enclosing function /
    /// trait method's `[with E]` generic params. Pushed when entering a
    /// signature with declared effect vars; popped when leaving. Consulted
    /// when parsing nested `Fn(...) with E` types in parameter / return
    /// position so an `E` token resolves to `EffectItem::Variable(E)`
    /// instead of `EffectItem::Group(E)`. Empty top frame means no
    /// effect variables in scope (parser treats all `with X` items as
    /// group references).
    pub(crate) effect_var_stack: Vec<Vec<String>>,
}

impl Parser {
    pub fn new(tokens: Vec<SpannedToken>) -> Self {
        Parser {
            tokens,
            pos: 0,
            loop_labels: Vec::new(),
            errors: Vec::new(),
            pending_doc: None,
            fn_context_stack: Vec::new(),
            effect_var_stack: Vec::new(),
        }
    }

    fn current_effect_vars(&self) -> &[String] {
        self.effect_var_stack
            .last()
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Drain pending leading `///` doc-comment text. Returns `None` when no
    /// doc comments are attached. Called from each item-construction site.
    fn take_pending_doc(&mut self) -> Option<String> {
        self.pending_doc.take()
    }

    /// Consume any leading `Token::DocComment` tokens at the current
    /// position and accumulate their text (newline-joined) into
    /// `pending_doc`. Idempotent — safe to call when no doc comments
    /// follow. Stops at the first non-doc-comment token.
    fn collect_leading_doc_comments(&mut self) {
        let mut buf: Option<String> = None;
        while let Token::DocComment(text) = self.peek_token() {
            let line = text.clone();
            self.advance();
            match &mut buf {
                Some(s) => {
                    s.push('\n');
                    s.push_str(&line);
                }
                None => buf = Some(line),
            }
        }
        if buf.is_some() {
            self.pending_doc = buf;
        }
    }

    pub fn parse(mut self) -> ParseResult {
        let program = self.parse_program();
        ParseResult {
            program,
            errors: self.errors,
        }
    }

    // ── Program ──────────────────────────────────────────────────

    fn parse_program(&mut self) -> Program {
        // Consume any leading `//!` module-level doc comments. They may
        // only appear before the first item; subsequent `//!` lines are
        // a parse error, but for the v1 MVP we silently treat trailing
        // `//!` as a regular line comment via the lexer's normal path.
        let mut module_doc_lines: Vec<String> = Vec::new();
        while let Token::ModuleDocComment(text) = self.peek_token() {
            module_doc_lines.push(text.clone());
            self.advance();
        }
        let module_doc_comment = if module_doc_lines.is_empty() {
            None
        } else {
            Some(module_doc_lines.join("\n"))
        };

        let mut items = Vec::new();
        while !self.is_at_end() {
            match self.parse_item() {
                Some(item) => items.push(item),
                None => {
                    // Error recovery: skip to next item-starting token
                    self.synchronize_to_item();
                }
            }
        }
        Program {
            items,
            module_doc_comment,
            ..Program::default()
        }
    }

    // ── Attributes ───────────────────────────────────────────────

    fn parse_attributes(&mut self) -> Vec<Attribute> {
        let mut attrs = Vec::new();
        while self.check(&Token::Pound) || self.check(&Token::At) {
            if self.check(&Token::At) {
                if let Some(attr) = self.parse_at_attribute() {
                    attrs.push(attr);
                }
            } else if let Some(attr) = self.parse_attribute() {
                attrs.push(attr);
            }
        }
        attrs
    }

    /// Parse `@name` shorthand attribute (e.g. `@no_rc`, `@noblock`)
    fn parse_at_attribute(&mut self) -> Option<Attribute> {
        let start = self.current_span();
        self.expect(&Token::At)?;
        let name = self.expect_identifier()?;
        Some(Attribute {
            span: self.span_from(&start),
            name,
            args: Vec::new(),
            string_value: None,
        })
    }

    fn parse_attribute(&mut self) -> Option<Attribute> {
        let start = self.current_span();
        self.expect(&Token::Pound)?;
        self.expect(&Token::LeftBracket)?;

        // `#[unsafe(...)]` wrap (design.md § Linker Control Attributes)
        // — the soundness-affecting linker attributes (`no_mangle`,
        // `link_section`) must be authored inside an `unsafe(...)`
        // wrap. The wrap is a visual trust-boundary marker; the parser
        // unwraps it into a plain attribute so downstream consumers
        // (codegen `apply_linker_attrs`) keep working unchanged. The
        // formatter re-emits the wrap when rendering — see
        // `format_attributes` for the canonical rendering.
        if self.check(&Token::Unsafe) {
            return self.parse_unsafe_wrapped_attribute(start);
        }

        let name = self.expect_identifier()?;

        // Bare `#[no_mangle]` / `#[link_section(...)]` — reject with a
        // focused diagnostic suggesting the `#[unsafe(...)]` wrap.
        // Errors do NOT bail; we keep parsing so error recovery remains
        // useful and the rest of the file still type-checks.
        if name == "no_mangle" {
            self.error(
                "bare `#[no_mangle]` is not allowed — write `#[unsafe(no_mangle)]` \
                 instead. The `unsafe(...)` wrap is a visual trust-boundary marker: \
                 disabling name mangling can collide with foreign symbols and is an \
                 obligation the compiler cannot verify. \
                 See design.md § Linker Control Attributes.",
            );
        } else if name == "link_section" {
            self.error(
                "bare `#[link_section(...)]` is not allowed — write \
                 `#[unsafe(link_section(\"...\"))]` instead. The `unsafe(...)` wrap \
                 is a visual trust-boundary marker: section placement carries layout \
                 and aliasing obligations the compiler cannot verify. \
                 See design.md § Linker Control Attributes.",
            );
        }

        // #[name = "string"] form
        let (args, string_value) = if self.eat(&Token::Equal) {
            match self.peek_token() {
                Token::StringLiteral(s) => {
                    let s = s.clone();
                    self.advance();
                    (Vec::new(), Some(s))
                }
                _ => {
                    self.error("Expected string literal after '=' in attribute");
                    return None;
                }
            }
        } else if self.eat(&Token::LeftParen) {
            let mut args = Vec::new();
            while !self.check(&Token::RightParen) && !self.is_at_end() {
                let arg_start = self.current_span();
                // Distinguish named (`name = value` / `name: value`) from
                // positional (`expr`) arg forms by two-token lookahead.
                // Named arg iff current token is a contextual-identifier
                // name AND the next token is `=` or `:`. Otherwise the arg
                // is a bare expression — used by `#[with_provider(Clock,
                // FakeClock.new)]` where the second argument is a method/
                // path expression, not an `ident = value` pair. Older
                // attributes that use `:` (`#[rc_budget(max: 5)]`) continue
                // to parse identically since the identifier-then-colon
                // shape still matches named.
                let (arg_name, value) = if self.is_named_attr_arg_head() {
                    let name = self.expect_attr_arg_name()?;
                    let val = if self.eat(&Token::Colon) || self.eat(&Token::Equal) {
                        Some(self.parse_expression()?)
                    } else {
                        None
                    };
                    (Some(name), val)
                } else {
                    (None, Some(self.parse_expression()?))
                };
                args.push(AttrArg {
                    name: arg_name,
                    value,
                    span: self.span_from(&arg_start),
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RightParen)?;
            (args, None)
        } else {
            (Vec::new(), None)
        };

        self.expect(&Token::RightBracket)?;

        // Slice 5 of the `unsafe_op_in_unsafe_fn` epic
        // (`docs/implementation_checklist/phase-5-diagnostics.md` §
        // `unsafe_op_in_unsafe_fn` rule, slice 5). Kāra is greenfield —
        // there is no migration story — so the rule is a hard error with
        // no opt-out. Reject `#[allow(unsafe_op_in_unsafe_fn)]` at parse
        // time and redirect the author to the actual fix: wrap the
        // offending operation in an `unsafe { ... }` block (with a
        // `// Safety:` comment per the `undocumented_unsafe` lint). The
        // attribute is recognised in two surface forms — positional
        // (`#[allow(unsafe_op_in_unsafe_fn)]`, the form anyone would
        // write) and named (`#[allow(name = unsafe_op_in_unsafe_fn)]`,
        // theoretical only) — both are caught.
        if name == "allow"
            && args.iter().any(|a| {
                a.name.as_deref() == Some("unsafe_op_in_unsafe_fn")
                    || a.value
                        .as_ref()
                        .map(|v| {
                            matches!(&v.kind, ExprKind::Identifier(n) if n == "unsafe_op_in_unsafe_fn")
                        })
                        .unwrap_or(false)
            })
        {
            self.error(
                "`#[allow(unsafe_op_in_unsafe_fn)]` is not accepted — the \
                 `unsafe_op_in_unsafe_fn` rule is a hard error with no opt-out \
                 (Kāra is greenfield; there is no migration story). Wrap the \
                 offending operation in an `unsafe { ... }` block instead, and \
                 add a `// Safety: ...` comment above the block per the \
                 `undocumented_unsafe` lint.",
            );
        }

        Some(Attribute {
            span: self.span_from(&start),
            name,
            args,
            string_value,
        })
    }

    /// Parse the `#[unsafe(NAME)]` / `#[unsafe(NAME("string"))]` wrap.
    /// `start` is the span of the leading `#`. The opening `#[` has
    /// already been consumed; the current token is `unsafe`.
    ///
    /// Currently the only inner names accepted are `no_mangle` and
    /// `link_section("name")` (per design.md § Linker Control Attributes).
    /// Any other inner name is rejected with a focused diagnostic — `used`
    /// in particular stays plain (`#[used]`) and is rejected here so a
    /// reader writing `#[unsafe(used)]` gets a helpful redirect rather
    /// than silent acceptance.
    ///
    /// The wrap is unwrapped at parse time: the resulting `Attribute`
    /// has `name == "no_mangle"` or `name == "link_section"` so existing
    /// downstream consumers (codegen `apply_linker_attrs`) keep working
    /// unchanged. The formatter re-emits the wrap on output.
    fn parse_unsafe_wrapped_attribute(&mut self, start: Span) -> Option<Attribute> {
        self.expect(&Token::Unsafe)?;
        self.expect(&Token::LeftParen)?;
        let inner_name = self.expect_identifier()?;

        let attr_string_value = match inner_name.as_str() {
            "no_mangle" => {
                // No further arguments accepted inside the wrap.
                None
            }
            "link_section" => {
                self.expect(&Token::LeftParen)?;
                let s = match self.peek_token() {
                    Token::StringLiteral(s) => {
                        let s = s.clone();
                        self.advance();
                        s
                    }
                    _ => {
                        self.error(
                            "expected a string literal section name inside \
                             `#[unsafe(link_section(...))]`, e.g. \
                             `#[unsafe(link_section(\".init_array\"))]`",
                        );
                        return None;
                    }
                };
                self.expect(&Token::RightParen)?;
                Some(s)
            }
            _ => {
                self.error(&format!(
                    "unknown attribute inside `#[unsafe(...)]`: `{inner_name}`. The \
                     `#[unsafe(...)]` wrap is reserved for soundness-affecting linker \
                     attributes — `no_mangle` and `link_section(\"...\")`. Plain \
                     attributes (e.g. `#[used]`, `#[noblock]`, `#[kara_name = \"...\"]`) \
                     are written without the wrap. \
                     See design.md § Linker Control Attributes."
                ));
                return None;
            }
        };

        self.expect(&Token::RightParen)?;
        self.expect(&Token::RightBracket)?;

        Some(Attribute {
            span: self.span_from(&start),
            name: inner_name,
            args: Vec::new(),
            string_value: attr_string_value,
        })
    }

    // ── Generics ─────────────────────────────────────────────────

    fn parse_optional_generic_params(&mut self) -> Option<GenericParams> {
        if !self.check(&Token::LeftBracket) {
            return None;
        }
        self.parse_generic_params()
    }

    fn parse_generic_params(&mut self) -> Option<GenericParams> {
        let start = self.current_span();
        self.expect(&Token::LeftBracket)?;

        let mut params = Vec::new();
        let mut effect_params = Vec::new();
        // Per design.md (line 4858): type params come first, then `with`
        // introduces effect-variable params. Once we've seen `with`, every
        // subsequent comma-separated identifier is an effect variable —
        // both the `[with E, F]` and `[with E, with F]` spellings are
        // accepted.
        let mut in_effect_params = false;
        loop {
            if self.check(&Token::RightBracket) {
                break;
            }
            // `with E[, F]` — `with` enters effect-vars mode (sticky).
            if self.check(&Token::With) {
                self.advance();
                in_effect_params = true;
            }
            if in_effect_params {
                let name = self.expect_identifier()?;
                effect_params.push(name);
                if !self.eat(&Token::Comma) {
                    break;
                }
                continue;
            }
            let pstart = self.current_span();
            // Check for const generic: `const N: Type`
            if self.check(&Token::Const) {
                self.advance();
                let name = self.expect_identifier()?;
                let pname_span = self.span_from(&pstart);
                // Const generic params follow the same Type-class convention
                // as type generic params (see design.md § Identifiers and
                // Naming). Single uppercase letters (`N`, `K`) and
                // PascalCase names both classify as Type-class.
                self.check_ident_class(
                    &name,
                    IdentClass::Type,
                    "const generic parameter",
                    pname_span,
                );
                self.expect(&Token::Colon)?;
                let ty = self.parse_type()?;
                params.push(GenericParam {
                    name,
                    bounds: Vec::new(),
                    is_const: true,
                    const_type: Some(ty),
                    span: self.span_from(&pstart),
                });
            } else {
                let name = self.expect_identifier()?;
                let pname_span = self.span_from(&pstart);
                self.check_ident_class(
                    &name,
                    IdentClass::Type,
                    "generic type parameter",
                    pname_span,
                );
                let mut bounds = Vec::new();
                if self.eat(&Token::Colon) {
                    loop {
                        let bound = self.parse_trait_bound()?;
                        bounds.push(bound);
                        if !self.eat(&Token::Plus) {
                            break;
                        }
                    }
                }
                params.push(GenericParam {
                    name,
                    bounds,
                    is_const: false,
                    const_type: None,
                    span: self.span_from(&pstart),
                });
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RightBracket)?;

        Some(GenericParams {
            params,
            effect_params,
            span: self.span_from(&start),
        })
    }

    fn parse_trait_bound(&mut self) -> Option<TraitBound> {
        let start = self.current_span();
        let path = self.parse_path_segments()?;
        let generic_args = if self.check(&Token::LeftBracket) {
            Some(self.parse_generic_type_args()?)
        } else {
            None
        };
        Some(TraitBound {
            path,
            generic_args,
            span: self.span_from(&start),
        })
    }

    fn parse_optional_where_clause(&mut self) -> Option<WhereClause> {
        if !self.eat(&Token::Where) {
            return None;
        }
        let start = self.current_span();
        let mut constraints = Vec::new();

        loop {
            // Stop at body opener or end-of-item markers
            if self.check(&Token::LeftBrace) || self.check(&Token::Semicolon) || self.is_at_end() {
                break;
            }
            let cstart = self.current_span();
            let saved = self.pos;
            let type_name = match self.expect_identifier() {
                Some(name) => name,
                None => break,
            };

            // Check for associated type equality: `T.Assoc = Type`
            if self.eat(&Token::Dot) {
                let assoc_name = self.expect_identifier()?;
                self.expect(&Token::Equal)?;
                let ty = self.parse_type()?;
                constraints.push(WhereConstraint::AssocTypeEq {
                    type_name,
                    assoc_name,
                    ty,
                    span: self.span_from(&cstart),
                });
            } else if self.eat(&Token::Colon) {
                let mut bounds = Vec::new();
                while let Some(bound) = self.parse_trait_bound() {
                    bounds.push(bound);
                    if !self.eat(&Token::Plus) {
                        break;
                    }
                }
                constraints.push(WhereConstraint::TypeBound {
                    type_name,
                    bounds,
                    span: self.span_from(&cstart),
                });
            } else {
                // Const-predicate fall-through: backtrack to the saved
                // position and parse the constraint as a const expression
                // (e.g. `where N >= 0`, `where M < 4096`). Slice 1 parses;
                // slice 2's evaluator + slice 3's discharge engine consume.
                self.pos = saved;
                let expr = self.parse_expression()?;
                constraints.push(WhereConstraint::ConstPredicate {
                    expr,
                    span: self.span_from(&cstart),
                });
            }

            if !self.eat(&Token::Comma) {
                break;
            }
        }

        Some(WhereClause {
            constraints,
            span: self.span_from(&start),
        })
    }

    // ── Contracts ─────────────────────────────────────────────────

    fn parse_requires_clauses(&mut self) -> Vec<Expr> {
        let mut clauses = Vec::new();
        while self.eat(&Token::Requires) {
            if let Some(expr) = self.parse_expression() {
                clauses.push(expr);
            }
        }
        clauses
    }

    fn parse_ensures_clauses(&mut self) -> Vec<EnsuresClause> {
        let mut clauses = Vec::new();
        while self.eat(&Token::Ensures) {
            let start = self.current_span();
            // Check for |param| closure-style syntax
            let param = if self.eat(&Token::Pipe) {
                let name = self.expect_identifier();
                self.expect(&Token::Pipe);
                name
            } else {
                None
            };
            if let Some(body) = self.parse_expression() {
                clauses.push(EnsuresClause {
                    param,
                    body,
                    span: self.span_from(&start),
                });
            }
        }
        clauses
    }

    fn parse_struct_body(&mut self) -> Option<(Vec<StructField>, Vec<Expr>)> {
        let mut fields = Vec::new();
        let mut invariants = Vec::new();

        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            self.collect_leading_doc_comments();
            // Check for invariant
            if self.eat(&Token::Invariant) {
                // Doc comments don't attach to invariants — drop any
                // accumulated text so it doesn't bleed into the next field.
                let _ = self.take_pending_doc();
                if let Some(expr) = self.parse_expression() {
                    invariants.push(expr);
                }
                // Optional trailing comma/semicolon after invariant — not required
                continue;
            }

            // Otherwise, parse a struct field
            let start = self.current_span();
            let attributes = self.parse_attributes();
            let is_pub = self.eat(&Token::Pub);
            let is_mut = self.eat(&Token::Mut);
            // Field-modifier `weak` per design.md § Shared Types — Weak
            // references. `mut weak field: T` and `weak field: T` are
            // both legal; the modifier wraps the parsed field type in
            // `TypeKind::Weak`. This is sugar — the type-position form
            // `field: weak T` is also accepted via `parse_type`.
            let weak_modifier_span = if matches!(self.peek_token(), Token::Weak) {
                let span = self.current_span();
                self.advance();
                Some(span)
            } else {
                None
            };
            let name = self.expect_identifier()?;
            let name_span = self.span_from(&start);
            self.check_ident_class(&name, IdentClass::Value, "struct field", name_span);
            self.expect(&Token::Colon)?;
            let inner_ty = self.parse_type()?;
            let ty = if let Some(span) = weak_modifier_span {
                TypeExpr {
                    kind: TypeKind::Weak(Box::new(inner_ty)),
                    span,
                }
            } else {
                inner_ty
            };
            let doc_comment = self.take_pending_doc();
            fields.push(StructField {
                span: self.span_from(&start),
                attributes,
                doc_comment,
                is_pub,
                is_mut,
                name,
                ty,
            });
            if !self.eat(&Token::Comma) {
                break;
            }
        }

        Some((fields, invariants))
    }

    fn parse_generic_type_args(&mut self) -> Option<Vec<GenericArg>> {
        self.expect(&Token::LeftBracket)?;
        let mut args = Vec::new();
        loop {
            if self.check(&Token::RightBracket) {
                break;
            }
            // Const expression args: integer literals, negative integers, bool literals,
            // character literals, and `Identifier OP ...` shapes (e.g. `Array[T, N + 1]`)
            // where the operator following the identifier disambiguates a const-arg
            // expression from a type-arg. Plain `Identifier` (no trailing op)
            // continues to parse as a type — `Vec[Map[K, V]]` and similar
            // type-position shapes are preserved.
            let is_const_arg_expr = match self.peek_token() {
                Token::Integer(_, _)
                | Token::True
                | Token::False
                | Token::Minus
                | Token::CharLiteral(_) => true,
                Token::Identifier { .. } => {
                    let next = self.tokens.get(self.pos + 1).map(|t| &t.token);
                    matches!(
                        next,
                        Some(
                            Token::Plus
                                | Token::Minus
                                | Token::Star
                                | Token::Slash
                                | Token::Percent
                                | Token::Caret
                                | Token::Amp
                                | Token::Pipe
                                | Token::LessLess
                                | Token::GreaterGreater
                                | Token::EqualEqual
                                | Token::BangEqual
                                | Token::LessThanOrEqual
                                | Token::GreaterThanOrEqual
                        )
                    )
                }
                _ => false,
            };
            if is_const_arg_expr {
                let expr = self.parse_expression()?;
                args.push(GenericArg::Const(expr));
            } else {
                args.push(GenericArg::Type(self.parse_type()?));
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RightBracket)?;
        Some(args)
    }

    // ── Types ────────────────────────────────────────────────────

    fn parse_type(&mut self) -> Option<TypeExpr> {
        let start = self.current_span();

        match self.peek_token() {
            // ref Type
            Token::Ref => {
                self.advance();
                let inner = self.parse_type()?;
                Some(TypeExpr {
                    kind: TypeKind::Ref(Box::new(inner)),
                    span: self.span_from(&start),
                })
            }
            // mut ref Type  |  mut Slice[T]
            Token::Mut => {
                self.advance();
                // `mut Slice[T]` — mutable slice view (no `ref` keyword).
                if let Token::Identifier { name, .. } = self.peek_token() {
                    if name == "Slice" {
                        // Parse the Slice path as a normal type, then strip
                        // down to its element and re-wrap as MutSlice.
                        let slice_ty = self.parse_type()?;
                        let element = match slice_ty.kind {
                            TypeKind::Path(ref path)
                                if path.segments.len() == 1 && path.segments[0] == "Slice" =>
                            {
                                match &path.generic_args {
                                    Some(args) if args.len() == 1 => match &args[0] {
                                        crate::ast::GenericArg::Type(t) => t.clone(),
                                        _ => {
                                            self.error(
                                                "mut Slice[T] requires a type argument, found const",
                                            );
                                            return None;
                                        }
                                    },
                                    _ => {
                                        self.error(
                                            "mut Slice[T] requires exactly one type argument",
                                        );
                                        return None;
                                    }
                                }
                            }
                            _ => {
                                self.error("expected Slice[T] after `mut`");
                                return None;
                            }
                        };
                        return Some(TypeExpr {
                            kind: TypeKind::MutSlice(Box::new(element)),
                            span: self.span_from(&start),
                        });
                    }
                }
                // Otherwise `mut ref T`.
                self.expect(&Token::Ref)?;
                let inner = self.parse_type()?;
                Some(TypeExpr {
                    kind: TypeKind::MutRef(Box::new(inner)),
                    span: self.span_from(&start),
                })
            }
            // weak Type
            Token::Weak => {
                self.advance();
                let inner = self.parse_type()?;
                Some(TypeExpr {
                    kind: TypeKind::Weak(Box::new(inner)),
                    span: self.span_from(&start),
                })
            }
            // *const T or *mut T
            Token::Star => {
                self.advance();
                let is_mut = if self.eat(&Token::Mut) {
                    true
                } else {
                    // expect "const" as identifier
                    match self.peek_token() {
                        Token::Const => {
                            self.advance();
                            false
                        }
                        _ => {
                            self.error("Expected 'const' or 'mut' after '*' in pointer type");
                            return None;
                        }
                    }
                };
                let inner = self.parse_type()?;
                Some(TypeExpr {
                    kind: TypeKind::Pointer {
                        is_mut,
                        inner: Box::new(inner),
                    },
                    span: self.span_from(&start),
                })
            }
            // () unit type or (A, B) tuple type
            Token::LeftParen => {
                self.advance();
                if self.eat(&Token::RightParen) {
                    return Some(TypeExpr {
                        kind: TypeKind::Unit,
                        span: self.span_from(&start),
                    });
                }
                let first = self.parse_type()?;
                if self.eat(&Token::Comma) {
                    // Tuple type
                    let mut types = vec![first];
                    while !self.check(&Token::RightParen) && !self.is_at_end() {
                        types.push(self.parse_type()?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    self.expect(&Token::RightParen)?;
                    Some(TypeExpr {
                        kind: TypeKind::Tuple(types),
                        span: self.span_from(&start),
                    })
                } else {
                    // Parenthesized type
                    self.expect(&Token::RightParen)?;
                    Some(first)
                }
            }
            // Fn(T) -> U with _ — and the once-callable variant `OnceFn(...)`
            // (round 12.46, Step 4). Both share the same AST shape; the
            // `is_once` flag distinguishes them so `lower_type_expr` can emit
            // `Type::OnceFunction` for the OnceFn form.
            Token::Identifier { ref name, .. } if name == "Fn" || name == "OnceFn" => {
                let is_once = name == "OnceFn";
                self.advance();
                self.expect(&Token::LeftParen)?;
                let mut params = Vec::new();
                while !self.check(&Token::RightParen) && !self.is_at_end() {
                    params.push(self.parse_type()?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                self.expect(&Token::RightParen)?;

                let return_type = if self.eat(&Token::Arrow) {
                    Some(Box::new(self.parse_type()?))
                } else {
                    None
                };

                let effect_spec = if self.check(&Token::With) {
                    // Peek-only: `parse_effect_list` consumes the `with`
                    // keyword itself and then handles `_` / verbs / named
                    // effect variables / group names uniformly. Pre-
                    // consuming would force the call into the without-
                    // `with`-prefix branch, which only handles bare verbs
                    // and silently drops named variables (latent bug fixed
                    // here as part of round 9).
                    let saved = self.pos;
                    if let Some(token) = self.tokens.get(self.pos + 1) {
                        if matches!(token.token, Token::Underscore) {
                            self.advance(); // with
                            self.advance(); // _
                            Some(EffectSpec::Polymorphic)
                        } else {
                            let effect_vars: Vec<String> = self.current_effect_vars().to_vec();
                            match self.parse_effect_list(&effect_vars) {
                                Some(effects) => Some(EffectSpec::Specific(effects)),
                                None => {
                                    self.pos = saved;
                                    return None;
                                }
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                Some(TypeExpr {
                    kind: TypeKind::FnType {
                        params,
                        return_type,
                        effect_spec,
                        is_once,
                    },
                    span: self.span_from(&start),
                })
            }
            // Path type: ident[::ident]*[<T, U>]
            Token::Identifier { .. } | Token::SelfType => {
                let path = self.parse_path_type()?;
                Some(TypeExpr {
                    kind: TypeKind::Path(path),
                    span: self.span_from(&start),
                })
            }
            _ => {
                self.error(&format!("Expected type, found {:?}", self.peek_token()));
                None
            }
        }
    }

    fn parse_path_type(&mut self) -> Option<PathExpr> {
        let start = self.current_span();
        let segments = self.parse_path_segments()?;

        // Check for generic args [T, U] — unambiguous in type position
        let generic_args = if self.check(&Token::LeftBracket) {
            Some(self.parse_generic_type_args()?)
        } else {
            None
        };

        Some(PathExpr {
            segments,
            generic_args,
            span: self.span_from(&start),
        })
    }

    // ── Blocks ───────────────────────────────────────────────────

    fn parse_block(&mut self) -> Option<Block> {
        let start = self.current_span();
        self.expect(&Token::LeftBrace)?;

        let mut stmts = Vec::new();
        let mut final_expr = None;

        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            // Try to parse a statement or final expression
            if self.is_statement_start() {
                match self.parse_statement() {
                    Some(stmt) => stmts.push(stmt),
                    None => {
                        self.synchronize_to_stmt();
                    }
                }
            } else {
                // Try to parse an expression
                match self.parse_expression_stmt() {
                    Some(expr) => {
                        if self.eat(&Token::Semicolon) {
                            // Expression statement
                            stmts.push(Stmt {
                                span: expr.span.clone(),
                                kind: StmtKind::Expr(expr),
                            });
                        } else if self.check(&Token::RightBrace) {
                            // Last item in block without semicolon
                            if self.is_block_expr(&expr) {
                                // Block-like expressions (while, for, loop, etc.)
                                // are statements that don't need semicolons
                                stmts.push(Stmt {
                                    span: expr.span.clone(),
                                    kind: StmtKind::Expr(expr),
                                });
                            } else {
                                // Value-producing expression (implicit return)
                                final_expr = Some(Box::new(expr));
                            }
                        } else if self.eat(&Token::Equal) {
                            // Assignment: expr = value
                            let value = self.parse_expression()?;
                            self.expect(&Token::Semicolon)?;
                            let span = expr.span.clone();
                            stmts.push(Stmt {
                                span,
                                kind: StmtKind::Assign {
                                    target: expr,
                                    value,
                                },
                            });
                        } else if let Some(cop) = self.try_compound_op() {
                            // Compound assignment: expr += value
                            let value = self.parse_expression()?;
                            self.expect(&Token::Semicolon)?;
                            let span = expr.span.clone();
                            stmts.push(Stmt {
                                span,
                                kind: StmtKind::CompoundAssign {
                                    target: expr,
                                    op: cop,
                                    value,
                                },
                            });
                        } else if self.is_block_expr(&expr) {
                            // Block-like expressions (if, while, for, loop, match, unsafe)
                            // don't need semicolons when used as statements
                            stmts.push(Stmt {
                                span: expr.span.clone(),
                                kind: StmtKind::Expr(expr),
                            });
                        } else {
                            // Expression without semicolon and not at end
                            stmts.push(Stmt {
                                span: expr.span.clone(),
                                kind: StmtKind::Expr(expr),
                            });
                        }
                    }
                    None => {
                        self.synchronize_to_stmt();
                    }
                }
            }
        }

        self.expect(&Token::RightBrace)?;

        Some(Block {
            stmts,
            final_expr,
            span: self.span_from(&start),
        })
    }

    /// Parse a `providers { R => e, ... } in { body }` expression.
    /// Caller positions at the `providers` keyword. Resource keys are
    /// bare identifiers (Type-class; the case-class check is a later
    /// pass). Trailing comma is accepted. Empty binding lists are
    /// rejected — an empty `providers { } in { body }` is semantically
    /// equivalent to just `body` and almost certainly a typo.
    /// Parse `providers { R => e, ... } in { body }` — the keyword
    /// `providers` is contextual and has already been consumed by the
    /// caller (`parse_identifier_expr`'s "providers"-name dispatch).
    /// `start` is the span of the consumed keyword for fidelity in the
    /// resulting Expr.
    fn parse_providers_block(&mut self, start: Span) -> Option<Expr> {
        self.expect(&Token::LeftBrace)?;

        let mut bindings: Vec<crate::ast::ProviderBinding> = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            let resource_span = self.current_span();
            let resource = self.expect_identifier()?;
            self.expect(&Token::FatArrow)?;
            let value = self.parse_expression()?;
            bindings.push(crate::ast::ProviderBinding {
                resource,
                resource_span,
                value,
            });
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RightBrace)?;

        if bindings.is_empty() {
            self.error("`providers { ... } in { ... }` requires at least one binding");
            return None;
        }

        self.expect(&Token::In)?;
        let body = self.parse_block()?;

        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::Providers { bindings, body },
        })
    }

    /// Parse the contextual special form `offset_of[T](field.path)`.
    /// `T` is a regular `TypeExpr` (so `offset_of[Vec[i64]](len)` works
    /// transparently); the paren contents are an identifier-only path
    /// `IDENT (. IDENT)*`. Any non-identifier expression form in the
    /// path position emits a focused diagnostic and returns `None`.
    /// See design.md § Field Offsets for the spec; the typechecker
    /// validates the path against `T`'s field set.
    fn parse_offset_of_special_form(&mut self, start: Span) -> Option<Expr> {
        self.expect(&Token::LeftBracket)?;
        let ty = self.parse_type()?;
        self.expect(&Token::RightBracket)?;
        self.expect(&Token::LeftParen)?;

        let mut field_path: Vec<String> = Vec::new();
        match self.peek_token() {
            Token::Identifier { .. } => {
                field_path.push(self.expect_identifier()?);
            }
            _ => {
                self.error(
                    "error[E_OFFSET_OF_INVALID_PATH]: offset_of accepts a field-name path \
                     (e.g. `offset_of[T](field)` or `offset_of[T](inner.y)`); expression \
                     forms (literals, calls, indexing, dereferences) are not legal here",
                );
                return None;
            }
        }
        loop {
            match self.peek_token() {
                Token::Dot => {
                    self.advance();
                    match self.peek_token() {
                        Token::Identifier { .. } => {
                            field_path.push(self.expect_identifier()?);
                        }
                        _ => {
                            self.error(
                                "error[E_OFFSET_OF_INVALID_PATH]: each segment of the offset_of \
                                 field path must be a bare identifier; indexing, method calls, \
                                 and dereferences are not legal here",
                            );
                            return None;
                        }
                    }
                }
                Token::RightParen => break,
                // `field[0]` (indexing), `field()` (call), `*field` (deref),
                // and any other expression-form continuation are rejected
                // with a focused diagnostic. The generic "Expected
                // RightParen" message would point at the wrong intent.
                _ => {
                    self.error(
                        "error[E_OFFSET_OF_INVALID_PATH]: offset_of accepts a field-name path \
                         (e.g. `offset_of[T](field)` or `offset_of[T](inner.y)`); indexing, \
                         method calls, dereferences, and other expression forms are not legal \
                         here",
                    );
                    return None;
                }
            }
        }
        self.expect(&Token::RightParen)?;

        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::OffsetOf { ty, field_path },
        })
    }

    fn is_statement_start(&self) -> bool {
        matches!(
            self.peek_token(),
            Token::Let | Token::Defer | Token::ErrDefer
        )
    }

    fn is_block_expr(&self, expr: &Expr) -> bool {
        // Block-like expressions that don't need semicolons when used as statements.
        // These are always treated as statements (not final_expr / implicit return).
        // For `loop` with break-value, use `let x = loop { ... };` pattern.
        matches!(
            &expr.kind,
            ExprKind::While { .. }
                | ExprKind::WhileLet { .. }
                | ExprKind::For { .. }
                | ExprKind::Loop { .. }
        )
    }

    // Block-like expressions that, in statement context, terminate the
    // current statement at their closing `}`. The next token — even one
    // normally accepted as a postfix operator (`[`, `(`, `.`, `?`, `?.`)
    // — starts a fresh statement.
    //
    // Required so that `while cond { ... }` followed by `[1, 2]` on the
    // next line parses as two statements rather than as
    // `(while cond {...})[1, 2]`. To apply postfix to a block-like
    // expression in statement context, parenthesize:
    // `(if cond { v1 } else { v2 }).method()`.
    fn is_block_like_prefix(expr: &Expr) -> bool {
        matches!(
            &expr.kind,
            ExprKind::If { .. }
                | ExprKind::IfLet { .. }
                | ExprKind::Match { .. }
                | ExprKind::While { .. }
                | ExprKind::WhileLet { .. }
                | ExprKind::For { .. }
                | ExprKind::Loop { .. }
                | ExprKind::Block(_)
                | ExprKind::Unsafe(_)
                | ExprKind::Seq(_)
                | ExprKind::Par(_)
                | ExprKind::Lock { .. }
                | ExprKind::Providers { .. }
        )
    }

    // ── Statements ───────────────────────────────────────────────

    fn parse_statement(&mut self) -> Option<Stmt> {
        match self.peek_token() {
            Token::Let => self.parse_let_statement(),
            Token::Defer => {
                let start = self.current_span();
                self.advance();
                let body = if self.check(&Token::LeftBrace) {
                    self.parse_block()?
                } else {
                    // defer expr;
                    let expr = self.parse_expression()?;
                    self.expect(&Token::Semicolon)?;
                    let span = expr.span.clone();
                    Block {
                        stmts: vec![Stmt {
                            span: span.clone(),
                            kind: StmtKind::Expr(expr),
                        }],
                        final_expr: None,
                        span,
                    }
                };
                Some(Stmt {
                    span: self.span_from(&start),
                    kind: StmtKind::Defer { body },
                })
            }
            Token::ErrDefer => {
                let start = self.current_span();
                self.advance();
                // errdefer(e) { ... } — paren-delimited binding
                let binding = if self.check(&Token::LeftParen) {
                    self.advance();
                    let name = self.expect_identifier()?;
                    self.expect(&Token::RightParen)?;
                    Some(name)
                } else {
                    None
                };
                let body = if self.check(&Token::LeftBrace) {
                    self.parse_block()?
                } else {
                    // errdefer expr;
                    let expr = self.parse_expression()?;
                    self.expect(&Token::Semicolon)?;
                    let span = expr.span.clone();
                    Block {
                        stmts: vec![Stmt {
                            span: span.clone(),
                            kind: StmtKind::Expr(expr),
                        }],
                        final_expr: None,
                        span,
                    }
                };
                Some(Stmt {
                    span: self.span_from(&start),
                    kind: StmtKind::ErrDefer { binding, body },
                })
            }
            _ => {
                let expr = self.parse_expression()?;
                if self.eat(&Token::Equal) {
                    // Assignment
                    let value = self.parse_expression()?;
                    let span = expr.span.clone();
                    self.expect(&Token::Semicolon)?;
                    Some(Stmt {
                        span,
                        kind: StmtKind::Assign {
                            target: expr,
                            value,
                        },
                    })
                } else if let Some(cop) = self.try_compound_op() {
                    // Compound assignment
                    let value = self.parse_expression()?;
                    let span = expr.span.clone();
                    self.expect(&Token::Semicolon)?;
                    Some(Stmt {
                        span,
                        kind: StmtKind::CompoundAssign {
                            target: expr,
                            op: cop,
                            value,
                        },
                    })
                } else {
                    self.expect(&Token::Semicolon)?;
                    Some(Stmt {
                        span: expr.span.clone(),
                        kind: StmtKind::Expr(expr),
                    })
                }
            }
        }
    }

    fn parse_let_statement(&mut self) -> Option<Stmt> {
        let start = self.current_span();
        self.expect(&Token::Let)?;

        let is_mut = self.eat(&Token::Mut);
        let pattern = self.parse_pattern()?;

        let ty = if self.eat(&Token::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };

        // Uninitialized form: `let pat: T;` (no `=` initializer).
        // Requires a type annotation (no RHS to infer from) and a single-name
        // pattern (destructuring needs a value). Definite-assignment analysis
        // tracks initialization through later assignments.
        if self.check(&Token::Semicolon) {
            self.advance();
            let Some(ty) = ty else {
                self.errors.push(ParseError {
                    message: "uninitialized `let` requires a type annotation; write `let x: T;` (or supply an initializer with `= ...`)"
                        .to_string(),
                    span: self.span_from(&start),
                });
                return None;
            };
            let (name, name_span) = match &pattern.kind {
                PatternKind::Binding(name) => (name.clone(), pattern.span.clone()),
                _ => {
                    self.errors.push(ParseError {
                        message: "uninitialized `let` must bind a single name; destructuring patterns require an initializer"
                            .to_string(),
                        span: pattern.span.clone(),
                    });
                    return None;
                }
            };
            return Some(Stmt {
                span: self.span_from(&start),
                kind: StmtKind::LetUninit {
                    is_mut,
                    name,
                    name_span,
                    ty,
                },
            });
        }

        self.expect(&Token::Equal)?;
        let value = self.parse_expression()?;

        // let ... else { diverging_block }
        if self.eat(&Token::Else) {
            let else_block = self.parse_block()?;
            return Some(Stmt {
                span: self.span_from(&start),
                kind: StmtKind::LetElse {
                    pattern,
                    ty,
                    value,
                    else_block,
                },
            });
        }

        self.expect(&Token::Semicolon)?;

        Some(Stmt {
            span: self.span_from(&start),
            kind: StmtKind::Let {
                is_mut,
                pattern,
                ty,
                value,
            },
        })
    }

    // ── Patterns ─────────────────────────────────────────────────

    fn parse_pattern(&mut self) -> Option<Pattern> {
        let start = self.current_span();
        let first = self.parse_single_pattern()?;

        // Check for or-pattern: A | B | C
        if self.check(&Token::Pipe) {
            let mut alternatives = vec![first];
            while self.eat(&Token::Pipe) {
                alternatives.push(self.parse_single_pattern()?);
            }
            return Some(Pattern {
                kind: PatternKind::Or(alternatives),
                span: self.span_from(&start),
            });
        }

        Some(first)
    }

    fn parse_single_pattern(&mut self) -> Option<Pattern> {
        let start = self.current_span();

        match self.peek_token() {
            Token::Underscore => {
                self.advance();
                Some(Pattern {
                    kind: PatternKind::Wildcard,
                    span: self.span_from(&start),
                })
            }
            // Half-open range patterns with a missing start: `..lit` and `..=lit`.
            // Bare `..` is not a valid pattern (use `_` for wildcard).
            Token::DotDot => {
                self.advance();
                let end = self.parse_literal_pattern()?;
                Some(Pattern {
                    kind: PatternKind::RangePattern {
                        start: None,
                        end: Some(end),
                        inclusive: false,
                    },
                    span: self.span_from(&start),
                })
            }
            Token::DotDotEq => {
                self.advance();
                let end = self.parse_literal_pattern()?;
                Some(Pattern {
                    kind: PatternKind::RangePattern {
                        start: None,
                        end: Some(end),
                        inclusive: true,
                    },
                    span: self.span_from(&start),
                })
            }
            Token::True => {
                self.advance();
                Some(Pattern {
                    kind: PatternKind::Literal(LiteralPattern::Bool(true)),
                    span: self.span_from(&start),
                })
            }
            Token::False => {
                self.advance();
                Some(Pattern {
                    kind: PatternKind::Literal(LiteralPattern::Bool(false)),
                    span: self.span_from(&start),
                })
            }
            Token::Integer(n, sfx) => {
                self.advance();
                let lit = LiteralPattern::Integer(n, sfx);
                // Check for range pattern: `1..=10` or `1..`
                if self.eat(&Token::DotDotEq) {
                    let end = self.parse_literal_pattern()?;
                    return Some(Pattern {
                        kind: PatternKind::RangePattern {
                            start: Some(lit),
                            end: Some(end),
                            inclusive: true,
                        },
                        span: self.span_from(&start),
                    });
                }
                if self.eat(&Token::DotDot) {
                    // `lo..hi` (bounded exclusive) when the next token is
                    // a literal; `lo..` (half-open) otherwise.
                    let end = if Self::starts_literal_pattern(&self.peek_token()) {
                        Some(self.parse_literal_pattern()?)
                    } else {
                        None
                    };
                    return Some(Pattern {
                        kind: PatternKind::RangePattern {
                            start: Some(lit),
                            end,
                            inclusive: false,
                        },
                        span: self.span_from(&start),
                    });
                }
                Some(Pattern {
                    kind: PatternKind::Literal(lit),
                    span: self.span_from(&start),
                })
            }
            Token::Float(n, sfx) => {
                self.advance();
                Some(Pattern {
                    kind: PatternKind::Literal(LiteralPattern::Float(n, sfx)),
                    span: self.span_from(&start),
                })
            }
            Token::StringLiteral(s) => {
                let s = s.clone();
                self.advance();
                Some(Pattern {
                    kind: PatternKind::Literal(LiteralPattern::String(s)),
                    span: self.span_from(&start),
                })
            }
            Token::CharLiteral(c) => {
                self.advance();
                let lit = LiteralPattern::Char(c);
                // Check for range pattern: `'a'..='z'` or `'a'..`
                if self.eat(&Token::DotDotEq) {
                    let end = self.parse_literal_pattern()?;
                    return Some(Pattern {
                        kind: PatternKind::RangePattern {
                            start: Some(lit),
                            end: Some(end),
                            inclusive: true,
                        },
                        span: self.span_from(&start),
                    });
                }
                if self.eat(&Token::DotDot) {
                    // `'a'..'z'` (bounded exclusive) when the next token
                    // is a literal; `'a'..` (half-open) otherwise.
                    let end = if Self::starts_literal_pattern(&self.peek_token()) {
                        Some(self.parse_literal_pattern()?)
                    } else {
                        None
                    };
                    return Some(Pattern {
                        kind: PatternKind::RangePattern {
                            start: Some(lit),
                            end,
                            inclusive: false,
                        },
                        span: self.span_from(&start),
                    });
                }
                Some(Pattern {
                    kind: PatternKind::Literal(lit),
                    span: self.span_from(&start),
                })
            }
            Token::LeftParen => {
                // Tuple pattern
                self.advance();
                let mut patterns = Vec::new();
                while !self.check(&Token::RightParen) && !self.is_at_end() {
                    patterns.push(self.parse_pattern()?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                self.expect(&Token::RightParen)?;
                Some(Pattern {
                    kind: PatternKind::Tuple(patterns),
                    span: self.span_from(&start),
                })
            }
            Token::LeftBracket => {
                // Slice/array pattern: `[p1, p2, ..rest, p_n-1, p_n]`.
                // Sub-item 1 of the slice/array-patterns entry (phase 5.2):
                // parser produces the variant; typechecker emits a stub
                // diagnostic until sub-item 2 lands.
                self.advance();
                let mut prefix: Vec<Pattern> = Vec::new();
                let mut suffix: Vec<Pattern> = Vec::new();
                let mut rest: Option<RestPattern> = None;
                while !self.check(&Token::RightBracket) && !self.is_at_end() {
                    if self.check(&Token::DotDot) {
                        let rest_span = self.current_span();
                        self.advance();
                        let new_rest = if let Token::Identifier { .. } = self.peek_token() {
                            let name = self.expect_identifier()?;
                            self.check_ident_class(
                                &name,
                                IdentClass::Value,
                                "binding",
                                rest_span.clone(),
                            );
                            RestPattern::Bound(name)
                        } else {
                            RestPattern::Ignored
                        };
                        if rest.is_some() {
                            // Recovery: keep the first rest marker; later
                            // elements continue collecting into `suffix`.
                            self.errors.push(ParseError {
                                message:
                                    "slice pattern may have at most one `..` marker; remove the extras"
                                        .to_string(),
                                span: rest_span,
                            });
                        } else {
                            rest = Some(new_rest);
                        }
                    } else {
                        let pat = self.parse_pattern()?;
                        if rest.is_none() {
                            prefix.push(pat);
                        } else {
                            suffix.push(pat);
                        }
                    }
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                self.expect(&Token::RightBracket)?;
                Some(Pattern {
                    kind: PatternKind::Slice {
                        prefix,
                        rest,
                        suffix,
                    },
                    span: self.span_from(&start),
                })
            }
            Token::Identifier { .. } => {
                let name = self.expect_identifier()?;

                // Check for @ binding: name @ pattern
                if self.eat(&Token::At) {
                    let name_span = self.span_from(&start);
                    self.check_ident_class(&name, IdentClass::Value, "binding", name_span);
                    let sub_pattern = self.parse_single_pattern()?;
                    return Some(Pattern {
                        kind: PatternKind::AtBinding {
                            name,
                            pattern: Box::new(sub_pattern),
                        },
                        span: self.span_from(&start),
                    });
                }

                // Check for struct destructure: Name { ... }
                if self.check(&Token::LeftBrace) {
                    self.advance();
                    let mut fields = Vec::new();
                    while !self.check(&Token::RightBrace) && !self.is_at_end() {
                        let fs = self.current_span();
                        let field_name = self.expect_identifier()?;
                        let pattern = if self.eat(&Token::Colon) {
                            Some(self.parse_pattern()?)
                        } else {
                            None
                        };
                        fields.push(FieldPattern {
                            name: field_name,
                            pattern,
                            span: self.span_from(&fs),
                        });
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    self.expect(&Token::RightBrace)?;
                    Some(Pattern {
                        kind: PatternKind::Struct {
                            path: vec![name],
                            fields,
                        },
                        span: self.span_from(&start),
                    })
                }
                // Check for tuple variant: Name(...)
                else if self.check(&Token::LeftParen) {
                    self.advance();
                    let mut patterns = Vec::new();
                    while !self.check(&Token::RightParen) && !self.is_at_end() {
                        patterns.push(self.parse_pattern()?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    self.expect(&Token::RightParen)?;
                    Some(Pattern {
                        kind: PatternKind::TupleVariant {
                            path: vec![name],
                            patterns,
                        },
                        span: self.span_from(&start),
                    })
                }
                // Check for qualified path: Name.Variant ...
                // Only Type/Const-class idents (uppercase leading) root a path in
                // pattern position; lowercase is always a plain binding.
                else if self.check(&Token::Dot) && starts_upper(&name) {
                    let mut path = vec![name];
                    while self.eat(&Token::Dot) {
                        path.push(self.expect_identifier()?);
                    }
                    // Check for struct or tuple variant
                    if self.check(&Token::LeftBrace) {
                        self.advance();
                        let mut fields = Vec::new();
                        while !self.check(&Token::RightBrace) && !self.is_at_end() {
                            let fs = self.current_span();
                            let field_name = self.expect_identifier()?;
                            let pattern = if self.eat(&Token::Colon) {
                                Some(self.parse_pattern()?)
                            } else {
                                None
                            };
                            fields.push(FieldPattern {
                                name: field_name,
                                pattern,
                                span: self.span_from(&fs),
                            });
                            if !self.eat(&Token::Comma) {
                                break;
                            }
                        }
                        self.expect(&Token::RightBrace)?;
                        Some(Pattern {
                            kind: PatternKind::Struct { path, fields },
                            span: self.span_from(&start),
                        })
                    } else if self.check(&Token::LeftParen) {
                        self.advance();
                        let mut patterns = Vec::new();
                        while !self.check(&Token::RightParen) && !self.is_at_end() {
                            patterns.push(self.parse_pattern()?);
                            if !self.eat(&Token::Comma) {
                                break;
                            }
                        }
                        self.expect(&Token::RightParen)?;
                        Some(Pattern {
                            kind: PatternKind::TupleVariant { path, patterns },
                            span: self.span_from(&start),
                        })
                    } else {
                        // Just a binding with a qualified name (unit variant)
                        Some(Pattern {
                            kind: PatternKind::Binding(path.join(".")),
                            span: self.span_from(&start),
                        })
                    }
                } else {
                    // Simple binding — may also be a unit variant reference (e.g. `None`
                    // in a match arm). The resolver distinguishes the two cases; skip the
                    // naming check here to avoid false positives on valid variant patterns.
                    Some(Pattern {
                        kind: PatternKind::Binding(name),
                        span: self.span_from(&start),
                    })
                }
            }
            _ => {
                self.error(&format!("Expected pattern, found {:?}", self.peek_token()));
                None
            }
        }
    }

    // ── Path Helpers ─────────────────────────────────────────────

    fn parse_path_segments(&mut self) -> Option<Vec<String>> {
        let mut segments = Vec::new();

        match self.peek_token() {
            Token::Identifier { .. } => segments.push(self.expect_identifier()?),
            Token::SelfType => {
                self.advance();
                segments.push("Self".to_string());
            }
            _ => {
                self.error("Expected identifier");
                return None;
            }
        }

        while self.eat(&Token::Dot) {
            segments.push(self.expect_identifier()?);
        }
        Some(segments)
    }

    // ── Pattern Helpers ───────────────────────────────────────────

    /// Parse a literal for use in range patterns (integer or char).
    /// True when `tok` starts a literal pattern (integer or char). Used
    /// by the range-pattern parser to disambiguate the bounded-exclusive
    /// form `lo..hi` from the half-open form `lo..` — only the former
    /// has a literal in end position.
    fn starts_literal_pattern(tok: &Token) -> bool {
        matches!(tok, Token::Integer(..) | Token::CharLiteral(_))
    }

    fn parse_literal_pattern(&mut self) -> Option<LiteralPattern> {
        match self.peek_token() {
            Token::Integer(n, sfx) => {
                self.advance();
                Some(LiteralPattern::Integer(n, sfx))
            }
            Token::CharLiteral(c) => {
                self.advance();
                Some(LiteralPattern::Char(c))
            }
            _ => {
                self.error("Expected integer or character literal in range pattern");
                None
            }
        }
    }

    // ── Token Helpers ────────────────────────────────────────────

    fn peek_token(&self) -> Token {
        if self.pos < self.tokens.len() {
            self.tokens[self.pos].token.clone()
        } else {
            Token::EOF
        }
    }

    fn peek_token_at(&self, offset: usize) -> Token {
        let idx = self.pos + offset;
        if idx < self.tokens.len() {
            self.tokens[idx].token.clone()
        } else {
            Token::EOF
        }
    }

    fn current_span(&self) -> Span {
        if self.pos < self.tokens.len() {
            self.tokens[self.pos].span.clone()
        } else if !self.tokens.is_empty() {
            self.tokens.last().unwrap().span.clone()
        } else {
            Span {
                line: 1,
                column: 1,
                offset: 0,
                length: 0,
            }
        }
    }

    fn advance(&mut self) -> &SpannedToken {
        let tok = &self.tokens[self.pos];
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn check(&self, expected: &Token) -> bool {
        std::mem::discriminant(&self.peek_token()) == std::mem::discriminant(expected)
    }

    fn eat(&mut self, expected: &Token) -> bool {
        if self.check(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &Token) -> Option<()> {
        if self.check(expected) {
            self.advance();
            Some(())
        } else {
            self.error(&format!(
                "Expected {:?}, found {:?}",
                expected,
                self.peek_token()
            ));
            None
        }
    }

    fn expect_identifier(&mut self) -> Option<String> {
        match self.peek_token() {
            Token::Identifier { name, .. } => {
                self.advance();
                Some(name)
            }
            _ => {
                self.error(&format!(
                    "Expected identifier, found {:?}",
                    self.peek_token()
                ));
                None
            }
        }
    }

    /// Like [`expect_identifier`] but accepts a small set of contextual
    /// keywords as identifier-equivalents in attribute-arg position. The
    /// design uses `requires` (already a contract keyword) and `ensures`
    /// inside `#[test(...)]`-style attributes; treating them as identifiers
    /// only here keeps the contract grammar untouched while letting
    /// design-conformant attribute syntax parse without quoting.
    /// Like [`expect_identifier`] but accepts the logical-operator
    /// keywords `not` / `and` / `or` as identifier-equivalents. Used
    /// at trait-method-name and impl-method-name positions so the
    /// stdlib can declare `trait Not { fn not(self) -> Self }` —
    /// design.md's bitwise-Not trait — without a separate raw-ident
    /// syntax. The lexer eagerly tokenizes these as `Token::Not` /
    /// `Token::And` / `Token::Or` regardless of context, so a
    /// targeted post-parse escape is the cleanest fix. Mirrors
    /// `expect_attr_arg_name`'s treatment of `requires` / `ensures`.
    fn expect_method_name(&mut self) -> Option<String> {
        match self.peek_token() {
            Token::Identifier { name, .. } => {
                self.advance();
                Some(name)
            }
            Token::Not => {
                self.advance();
                Some("not".to_string())
            }
            Token::And => {
                self.advance();
                Some("and".to_string())
            }
            Token::Or => {
                self.advance();
                Some("or".to_string())
            }
            _ => {
                self.error(&format!(
                    "Expected method name, found {:?}",
                    self.peek_token()
                ));
                None
            }
        }
    }

    fn expect_attr_arg_name(&mut self) -> Option<String> {
        match self.peek_token() {
            Token::Identifier { name, .. } => {
                self.advance();
                Some(name)
            }
            Token::Requires => {
                self.advance();
                Some("requires".to_string())
            }
            Token::Ensures => {
                self.advance();
                Some("ensures".to_string())
            }
            _ => {
                self.error(&format!(
                    "Expected attribute argument name, found {:?}",
                    self.peek_token()
                ));
                None
            }
        }
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.tokens.len() || self.peek_token() == Token::EOF
    }

    /// Lookahead used by `parse_attribute` to decide whether the current
    /// attribute-arg position starts a named (`ident = value` /
    /// `ident: value`) or positional (bare expression) argument. A named
    /// arg is an identifier-shaped token whose immediate successor is
    /// `=` or `:`; everything else — including a bare identifier followed
    /// by `,` / `)` / `.` — is treated as the start of an expression.
    /// The contract keywords `requires` / `ensures` are treated as
    /// identifiers here to match `expect_attr_arg_name`.
    fn is_named_attr_arg_head(&self) -> bool {
        if self.pos + 1 >= self.tokens.len() {
            return false;
        }
        let is_name_token = matches!(
            self.tokens[self.pos].token,
            Token::Identifier { .. } | Token::Requires | Token::Ensures
        );
        if !is_name_token {
            return false;
        }
        matches!(self.tokens[self.pos + 1].token, Token::Equal | Token::Colon)
    }

    fn span_from(&self, start: &Span) -> Span {
        let end = if self.pos > 0 {
            &self.tokens[self.pos - 1].span
        } else {
            start
        };
        Span {
            line: start.line,
            column: start.column,
            offset: start.offset,
            length: (end.offset + end.length).saturating_sub(start.offset),
        }
    }

    // ── Error Recovery ───────────────────────────────────────────

    fn error(&mut self, message: &str) {
        let span = self.current_span();
        self.errors.push(ParseError {
            message: message.to_string(),
            span,
        });
    }

    /// Emit a non-fatal diagnostic if `name` does not have the expected
    /// `IdentClass`. The `context` string is the declaration kind (e.g.
    /// `"struct"`, `"fn"`, `"const"`). The diagnostic includes a rename
    /// suggestion.
    fn check_ident_class(&mut self, name: &str, expected: IdentClass, context: &str, span: Span) {
        let actual = classify_ident(name);
        if actual == expected {
            return;
        }
        let (expected_desc, suggestion) = match expected {
            IdentClass::Type => ("Type-class (PascalCase)", suggest_type_name(name)),
            IdentClass::Value => ("Value-class (snake_case)", suggest_value_name(name)),
            IdentClass::Const => (
                "Const-class (SCREAMING_SNAKE_CASE)",
                suggest_const_name(name),
            ),
        };
        let actual_desc = match actual {
            IdentClass::Type => "Type-class",
            IdentClass::Value => "Value-class",
            IdentClass::Const => "Const-class",
        };
        self.errors.push(ParseError {
            message: format!(
                "`{name}` is {actual_desc} but {context} names must be {expected_desc}; consider renaming to `{suggestion}`"
            ),
            span,
        });
    }

    fn synchronize_to_item(&mut self) {
        while !self.is_at_end() {
            match self.peek_token() {
                Token::Fn
                | Token::Struct
                | Token::Enum
                | Token::Trait
                | Token::Impl
                | Token::Effect
                | Token::Transparent
                | Token::Layout
                | Token::Mod
                | Token::Use
                | Token::Import
                | Token::Const
                | Token::Alias
                | Token::Independent
                | Token::Extern
                | Token::Type
                | Token::Distinct
                | Token::Pub
                | Token::Stable
                | Token::Pound => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn synchronize_to_stmt(&mut self) {
        while !self.is_at_end() {
            match self.peek_token() {
                Token::Semicolon => {
                    self.advance();
                    return;
                }
                Token::RightBrace => return,
                Token::Let
                | Token::Return
                | Token::If
                | Token::While
                | Token::For
                | Token::Loop
                | Token::Match
                | Token::Break
                | Token::Continue => return,
                _ => {
                    self.advance();
                }
            }
        }
    }
}

// ── Diagnostic helpers ──────────────────────────────────────────

/// Render a `TypeExpr` back to a compact source-style string for inclusion
/// in parser diagnostics (e.g., the `_: <type>` fix-it on
/// `E_FN_ANONYMOUS_PARAM`). Covers every `TypeKind` variant the parser
/// can build; not byte-for-byte identical to the original source, but
/// produces a copy-pasteable surface form.
pub(crate) fn render_type_for_diagnostic(ty: &TypeExpr) -> String {
    let mut out = String::new();
    write_type_for_diagnostic(ty, &mut out);
    out
}

fn write_type_for_diagnostic(ty: &TypeExpr, out: &mut String) {
    match &ty.kind {
        TypeKind::Path(path) => {
            for (i, seg) in path.segments.iter().enumerate() {
                if i > 0 {
                    out.push('.');
                }
                out.push_str(seg);
            }
            if let Some(args) = &path.generic_args {
                if !args.is_empty() {
                    out.push('[');
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        match arg {
                            crate::ast::GenericArg::Type(t) => write_type_for_diagnostic(t, out),
                            crate::ast::GenericArg::Const(_) => out.push('_'),
                        }
                    }
                    out.push(']');
                }
            }
        }
        TypeKind::Tuple(types) => {
            out.push('(');
            for (i, t) in types.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_type_for_diagnostic(t, out);
            }
            out.push(')');
        }
        TypeKind::Array { element, .. } => {
            out.push('[');
            write_type_for_diagnostic(element, out);
            out.push_str("; _]");
        }
        TypeKind::Pointer { is_mut, inner } => {
            out.push('*');
            out.push_str(if *is_mut { "mut " } else { "const " });
            write_type_for_diagnostic(inner, out);
        }
        TypeKind::FnType {
            params,
            return_type,
            is_once,
            ..
        } => {
            out.push_str(if *is_once { "OnceFn(" } else { "Fn(" });
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_type_for_diagnostic(p, out);
            }
            out.push(')');
            if let Some(rt) = return_type {
                out.push_str(" -> ");
                write_type_for_diagnostic(rt, out);
            }
        }
        TypeKind::Ref(inner) => {
            out.push_str("ref ");
            write_type_for_diagnostic(inner, out);
        }
        TypeKind::MutRef(inner) => {
            out.push_str("mut ref ");
            write_type_for_diagnostic(inner, out);
        }
        TypeKind::MutSlice(element) => {
            out.push_str("mut Slice[");
            write_type_for_diagnostic(element, out);
            out.push(']');
        }
        TypeKind::Weak(inner) => {
            out.push_str("weak ");
            write_type_for_diagnostic(inner, out);
        }
        TypeKind::Unit => out.push_str("()"),
        TypeKind::Error => out.push('_'),
    }
}
