// src/parser.rs

//! Recursive-descent parser for Kāra source code.
//! Produces an AST from a token stream with error recovery and multi-error reporting.

use crate::ast::*;
use crate::lexer::{
    classify_ident, suggest_const_name, suggest_type_name, suggest_value_name, IdentClass,
};
use crate::token::{Span, SpannedToken, Token};

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
enum FnContext {
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
fn starts_upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

// ── Parser ───────────────────────────────────────────────────────

pub struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    errors: Vec<ParseError>,
    /// Active labels for disambiguating `break label` vs `break value` and
    /// for routing labeled-block label scopes. Each entry carries a
    /// `LabelKind` tag (`Loop` for labeled loops, `Block` for labeled
    /// blocks) — the kind is consulted by the resolver, not the parser,
    /// but the parser tracks it so that `is_known_label` lookups (which
    /// disambiguate `break label expr`) work uniformly across both label
    /// kinds. Pushed at the entry to a labeled construct, popped on exit.
    loop_labels: Vec<(String, LabelKind)>,
    /// Doc-comment text accumulated by the leading-`///` collection at the
    /// top of `parse_item`. Each item-construction site calls
    /// `Self::take_pending_doc` when filling the new node's `doc_comment`
    /// field. Cleared after consumption so a subsequent item without docs
    /// gets `None`.
    pending_doc: Option<String>,
    /// Stack of `FnContext`s for the function-like signature we are currently
    /// parsing parameters for. Drives the anonymous-parameter focused
    /// diagnostic: trait-method bodies emit `E_TRAIT_METHOD_ANONYMOUS_PARAM`
    /// while free / impl / extern function bodies emit
    /// `E_FN_ANONYMOUS_PARAM`. Empty when we are not inside a parameter
    /// list (e.g., parsing a struct field or top-level expression).
    fn_context_stack: Vec<FnContext>,
    /// Stack of effect-variable names declared in the enclosing function /
    /// trait method's `[with E]` generic params. Pushed when entering a
    /// signature with declared effect vars; popped when leaving. Consulted
    /// when parsing nested `Fn(...) with E` types in parameter / return
    /// position so an `E` token resolves to `EffectItem::Variable(E)`
    /// instead of `EffectItem::Group(E)`. Empty top frame means no
    /// effect variables in scope (parser treats all `with X` items as
    /// group references).
    effect_var_stack: Vec<Vec<String>>,
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

    // ── Items ────────────────────────────────────────────────────

    fn parse_item(&mut self) -> Option<Item> {
        // Collect any leading `///` doc-comment tokens. The text gets
        // attached to the next item via `take_pending_doc` at the
        // item-construction site.
        self.collect_leading_doc_comments();
        let attributes = self.parse_attributes();
        // Visibility keyword: `pub` OR `private`, never both.
        let is_pub = self.eat(&Token::Pub);
        let is_private = if !is_pub {
            self.eat(&Token::Private)
        } else if self.check(&Token::Private) {
            // `pub private` is a user mistake — be loud about it.
            self.error("cannot combine `pub` and `private` on the same item");
            self.advance();
            false
        } else {
            false
        };
        if is_private && self.check(&Token::Pub) {
            self.error("cannot combine `private` and `pub` on the same item");
            self.advance();
        }

        match self.peek_token() {
            Token::Fn => Some(Item::Function(
                self.parse_function(attributes, is_pub, is_private, false)?,
            )),
            Token::Struct => Some(Item::StructDef(
                self.parse_struct_def(attributes, is_pub, is_private, false)?,
            )),
            Token::Enum => Some(Item::EnumDef(
                self.parse_enum_def(attributes, is_pub, is_private, false)?,
            )),
            Token::Shared => {
                self.advance();
                match self.peek_token() {
                    Token::Struct => Some(Item::StructDef(
                        self.parse_struct_def(attributes, is_pub, is_private, true)?,
                    )),
                    Token::Enum => Some(Item::EnumDef(
                        self.parse_enum_def(attributes, is_pub, is_private, true)?,
                    )),
                    _ => {
                        self.error("Expected 'struct' or 'enum' after 'shared'");
                        None
                    }
                }
            }
            Token::Trait => self.parse_trait_or_alias(attributes, is_pub, is_private),
            Token::Marker => Some(Item::MarkerTrait(
                self.parse_marker_trait(attributes, is_pub, is_private)?,
            )),
            Token::Impl => Some(Item::ImplBlock(self.parse_impl_block(attributes)?)),
            Token::Effect => self.parse_effect_decl(is_pub, false, false),
            Token::Stable => {
                self.advance();
                // stable effect group ...
                self.parse_effect_decl(is_pub, true, false)
            }
            Token::Transparent => {
                self.advance();
                self.parse_effect_decl(is_pub, false, true)
            }
            Token::Layout => {
                let def = self.parse_layout_def(attributes, is_pub)?;
                Some(Item::LayoutDef(def))
            }
            Token::Mod => {
                self.reject_mod_decl();
                None
            }
            Token::Use => {
                let decl = self.parse_use_decl(is_pub)?;
                Some(Item::UseDecl(decl))
            }
            Token::Import => {
                let decl = self.parse_import_decl(is_pub)?;
                Some(Item::Import(decl))
            }
            Token::Const => {
                let decl = self.parse_const_decl(is_pub, is_private)?;
                Some(Item::ConstDecl(decl))
            }
            Token::Alias => {
                let decl = self.parse_alias_decl()?;
                Some(Item::AliasDecl(decl))
            }
            Token::Independent => {
                let decl = self.parse_independent_decl()?;
                Some(Item::IndependentDecl(decl))
            }
            Token::Unsafe => {
                // `unsafe` at module scope prefixes one of:
                //   - `unsafe extern "ABI" { ... }` block (FFI trust boundary)
                //   - `unsafe fn name(...) { ... }` (declaration-side
                //     precondition: callers must wrap calls to this fn in
                //     `unsafe { ... }` per the `unsafe_op_in_unsafe_fn`
                //     rule, which also requires unsafe ops INSIDE the
                //     body to be wrapped even when the outer fn is
                //     `unsafe fn`)
                // Dispatch is by lookahead at the token after `unsafe`.
                match self.peek_token_at(1) {
                    Token::Extern => {
                        let decl =
                            self.parse_unsafe_extern_block(attributes, is_pub, is_private)?;
                        Some(Item::ExternBlock(decl))
                    }
                    Token::Fn => {
                        self.advance(); // consume `unsafe`
                        Some(Item::Function(
                            self.parse_function(attributes, is_pub, is_private, true)?,
                        ))
                    }
                    _ => {
                        self.error(
                            "expected `extern` or `fn` after `unsafe` at module scope — \
                             `unsafe` may only prefix an `unsafe extern \"ABI\" { ... }` \
                             block or an `unsafe fn` declaration.",
                        );
                        self.advance(); // consume `unsafe` for recovery
                        None
                    }
                }
            }
            Token::Extern => {
                // Bare module-scope `extern "C" fn name(...);` and `extern "C"
                // { ... }` are no longer accepted — foreign imports must live
                // inside an `unsafe extern "ABI" { ... }` block (the trust
                // boundary the programmer asserts at). Definitions with a
                // body (`pub extern "C" fn name() { ... }`) are a separate
                // form and keep their own parsing path (not yet implemented
                // in v1; tracked at design.md § FFI > "Definitions are not
                // affected").
                self.error_bare_extern_at_module_scope();
                None
            }
            Token::Distinct => {
                let def = self.parse_distinct_type(attributes, is_pub, is_private)?;
                Some(Item::DistinctType(def))
            }
            Token::Type => {
                let def = self.parse_type_alias(is_pub, is_private)?;
                Some(Item::TypeAlias(def))
            }
            _ => {
                if !attributes.is_empty() || is_pub || is_private {
                    self.error("Expected item declaration after attributes/pub/private");
                }
                None
            }
        }
    }

    // ── Functions ────────────────────────────────────────────────

    fn parse_function(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
        is_unsafe: bool,
    ) -> Option<Function> {
        let start = self.current_span();
        self.expect(&Token::Fn)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Value, "fn", name_span);
        // Take the item-level doc *before* descending into the param list —
        // per-param doc collection inside `parse_fn_params` would otherwise
        // overwrite it.
        let doc_comment = self.take_pending_doc();
        let generic_params = self.parse_optional_generic_params();
        let effect_vars: Vec<String> = generic_params
            .as_ref()
            .map(|gp| gp.effect_params.clone())
            .unwrap_or_default();
        self.effect_var_stack.push(effect_vars.clone());

        self.expect(&Token::LeftParen)?;
        self.fn_context_stack.push(FnContext::Function);
        let (self_param, params) = self.parse_fn_params()?;
        self.fn_context_stack.pop();
        self.expect(&Token::RightParen)?;

        let return_type = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        let effects = self.parse_optional_effect_list(&effect_vars);
        let requires = self.parse_requires_clauses();
        let ensures = self.parse_ensures_clauses();
        let where_clause = self.parse_optional_where_clause();
        let body = self.parse_block()?;
        self.effect_var_stack.pop();

        Some(Function {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            is_unsafe,
            name,
            generic_params,
            params,
            self_param,
            return_type,
            effects,
            requires,
            ensures,
            where_clause,
            body,
            stdlib_origin: false,
        })
    }

    fn parse_fn_params(&mut self) -> Option<(Option<SelfParam>, Vec<Param>)> {
        let mut self_param = None;
        let mut params = Vec::new();

        if self.check(&Token::RightParen) {
            return Some((None, params));
        }

        // Check for self parameter
        if self.check(&Token::SelfValue)
            || self.check(&Token::Own)
            || self.check(&Token::Ref)
            || self.check(&Token::Mut)
        {
            if let Some(sp) = self.try_parse_self_param() {
                self_param = Some(sp);
                if !self.eat(&Token::Comma) {
                    return Some((self_param, params));
                }
            }
        }

        // Parse remaining params
        loop {
            if self.check(&Token::RightParen) {
                break;
            }
            let param = self.parse_param()?;
            params.push(param);
            if !self.eat(&Token::Comma) {
                break;
            }
        }

        Some((self_param, params))
    }

    fn try_parse_self_param(&mut self) -> Option<SelfParam> {
        let saved = self.pos;

        // own self — rejected under 2A; bare `self` is the owned/consuming receiver.
        if self.eat(&Token::Own) {
            if self.eat(&Token::SelfValue) {
                self.error(
                    "`own self` is not a valid receiver form. \
                     Bare `self` is the owned/consuming receiver; \
                     `ref self` and `mut ref self` are the two borrow forms.",
                );
                return Some(SelfParam::Owned);
            }
            self.pos = saved;
            return None;
        }

        // self (bare — owned/consuming receiver under 2A)
        if self.eat(&Token::SelfValue) {
            return Some(SelfParam::Owned);
        }

        // mut ref self
        if self.eat(&Token::Mut) {
            if self.eat(&Token::Ref) && self.eat(&Token::SelfValue) {
                return Some(SelfParam::MutRef);
            }
            self.pos = saved;
            return None;
        }

        // ref self
        if self.eat(&Token::Ref) {
            if self.eat(&Token::SelfValue) {
                return Some(SelfParam::Ref);
            }
            self.pos = saved;
            return None;
        }

        None
    }

    fn parse_param(&mut self) -> Option<Param> {
        // Collect any `///` doc comments preceding this parameter. Mirrors
        // the item-level pattern in `parse_item` — collect first, consume
        // via `take_pending_doc` at construction. Callers must drain the
        // surrounding item's pending_doc before descending into the param
        // list to avoid clobbering.
        self.collect_leading_doc_comments();
        let start = self.current_span();

        // Focused diagnostic for the anonymous-parameter shape — `fn f(Type)`
        // / `trait T { fn m(self, Type); }`. Try to recognize a TYPE in
        // parameter position with no preceding name+colon; if it succeeds,
        // emit `E_TRAIT_METHOD_ANONYMOUS_PARAM` / `E_FN_ANONYMOUS_PARAM`
        // (per design.md § Trait method parameter names — required) and
        // recover by treating the parameter as `_: TY`.
        if let Some(ty) = self.try_parse_anonymous_param_type() {
            let doc_comment = self.take_pending_doc();
            let ty_span = ty.span.clone();
            let pattern = Pattern {
                kind: PatternKind::Wildcard,
                span: ty_span,
            };
            return Some(Param {
                span: self.span_from(&start),
                pattern,
                ty,
                default_value: None,
                doc_comment,
            });
        }

        let pattern = self.parse_param_pattern()?;
        self.expect(&Token::Colon)?;
        let ty = self.parse_type()?;
        let default_value = if self.eat(&Token::Equal) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        let doc_comment = self.take_pending_doc();
        Some(Param {
            span: self.span_from(&start),
            pattern,
            ty,
            default_value,
            doc_comment,
        })
    }

    /// Speculatively recognize the shape `fn f(TYPE)` — a TYPE in parameter
    /// position with no preceding name+colon. Returns `Some(ty)` after
    /// emitting the focused anonymous-parameter diagnostic; the caller
    /// recovers by treating the parameter as `_: TY` so the rest of the
    /// signature keeps parsing. Returns `None` if the position does not
    /// look like an anonymous param (the parser state is fully restored,
    /// including any errors `parse_type` produced before deciding the
    /// position was something else).
    fn try_parse_anonymous_param_type(&mut self) -> Option<TypeExpr> {
        // Cheap rule-out: positions that start a normal name-bound parameter.
        match self.peek_token() {
            // `_: TY` — the wildcard pattern path; treat as a normal param.
            Token::Underscore => return None,
            // `name: TY` and `name { … }` (struct destructure) and
            // `name(…)` (tuple-struct destructure) all start a pattern.
            Token::Identifier { .. } => {
                let next = self.tokens.get(self.pos + 1).map(|t| &t.token);
                if matches!(
                    next,
                    Some(Token::Colon) | Some(Token::LeftBrace) | Some(Token::LeftParen)
                ) {
                    return None;
                }
            }
            _ => {}
        }

        let saved_pos = self.pos;
        let saved_errors_len = self.errors.len();
        let ty = self.parse_type();

        // Only recognize the anonymous shape when the type parse succeeded
        // and landed on a token that ends a parameter (`,` / `)` / `=`).
        // Anything else means we miss-classified — restore state so the
        // caller's normal pattern-then-type parse runs and produces the
        // existing diagnostic.
        let landed_well = matches!(
            self.peek_token(),
            Token::Comma | Token::RightParen | Token::Equal
        );
        let ty = match (ty, landed_well) {
            (Some(ty), true) => ty,
            _ => {
                self.pos = saved_pos;
                self.errors.truncate(saved_errors_len);
                return None;
            }
        };

        let (code, kind_label) = match self.fn_context_stack.last() {
            Some(FnContext::TraitMethod) => ("E_TRAIT_METHOD_ANONYMOUS_PARAM", "trait method"),
            // Default to the free-function diagnostic when the context
            // stack is empty (defensive — every signature site should
            // have pushed before reaching `parse_param`).
            _ => ("E_FN_ANONYMOUS_PARAM", "function"),
        };
        let type_text = render_type_for_diagnostic(&ty);
        self.errors.push(ParseError {
            message: format!(
                "error[{code}]: {kind_label} parameters require a name; \
                 write `_: {type_text}` for an unused parameter, or \
                 `arg: {type_text}` for a meaningful name"
            ),
            span: ty.span.clone(),
        });
        Some(ty)
    }

    /// Parse an irrefutable pattern for a function parameter position.
    /// Supports: identifier, `_`, tuple `(a, b)`, and struct `Name { x, y }`.
    fn parse_param_pattern(&mut self) -> Option<Pattern> {
        let start = self.current_span();
        match self.peek_token() {
            // Wildcard
            Token::Underscore => {
                self.advance();
                Some(Pattern {
                    kind: PatternKind::Wildcard,
                    span: self.span_from(&start),
                })
            }
            // Tuple destructuring: (a, b, ...)
            Token::LeftParen => {
                self.advance();
                let mut patterns = Vec::new();
                while !self.check(&Token::RightParen) && !self.is_at_end() {
                    patterns.push(self.parse_param_pattern()?);
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
            // Identifier — could be plain binding or struct destructure `Name { ... }`
            Token::Identifier { .. } => {
                let name = self.expect_identifier()?;
                if self.check(&Token::LeftBrace) {
                    // Struct destructuring: Name { field1, field2: pat, ... }
                    self.advance();
                    let mut fields = Vec::new();
                    while !self.check(&Token::RightBrace) && !self.is_at_end() {
                        let fstart = self.current_span();
                        let field_name = self.expect_identifier()?;
                        let sub_pattern = if self.eat(&Token::Colon) {
                            Some(self.parse_param_pattern()?)
                        } else {
                            None
                        };
                        fields.push(FieldPattern {
                            name: field_name,
                            pattern: sub_pattern,
                            span: self.span_from(&fstart),
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
                } else {
                    // Simple binding
                    let name_span = self.span_from(&start);
                    self.check_ident_class(&name, IdentClass::Value, "parameter", name_span);
                    Some(Pattern {
                        kind: PatternKind::Binding(name),
                        span: self.span_from(&start),
                    })
                }
            }
            _ => {
                self.error(&format!(
                    "expected parameter pattern, found {:?}",
                    self.peek_token()
                ));
                None
            }
        }
    }

    // ── Structs ──────────────────────────────────────────────────

    fn parse_struct_def(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
        is_shared: bool,
    ) -> Option<StructDef> {
        let start = self.current_span();
        self.expect(&Token::Struct)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "struct", name_span);
        // Take the item-level doc *before* descending into the body —
        // field-level doc collection inside `parse_struct_body` would
        // otherwise overwrite it.
        let doc_comment = self.take_pending_doc();
        let generic_params = self.parse_optional_generic_params();
        let where_clause = self.parse_optional_where_clause();

        self.expect(&Token::LeftBrace)?;
        let (fields, invariants) = self.parse_struct_body()?;
        self.expect(&Token::RightBrace)?;

        let no_rc = attributes.iter().any(|a| a.name == "no_rc");

        Some(StructDef {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            is_shared,
            no_rc,
            name,
            generic_params,
            where_clause,
            fields,
            invariants,
            stdlib_origin: false,
        })
    }

    fn parse_struct_fields(&mut self) -> Option<Vec<StructField>> {
        let mut fields = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            self.collect_leading_doc_comments();
            let start = self.current_span();
            let attributes = self.parse_attributes();
            let is_pub = self.eat(&Token::Pub);
            let is_mut = self.eat(&Token::Mut);
            let name = self.expect_identifier()?;
            let name_span = self.span_from(&start);
            self.check_ident_class(&name, IdentClass::Value, "struct field", name_span);
            self.expect(&Token::Colon)?;
            let ty = self.parse_type()?;
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
        Some(fields)
    }

    // ── Enums ────────────────────────────────────────────────────

    fn parse_enum_def(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
        is_shared: bool,
    ) -> Option<EnumDef> {
        let start = self.current_span();
        self.expect(&Token::Enum)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "enum", name_span);
        // Take the item-level doc *before* descending into the variant list
        // — variant-level doc collection inside `parse_variant` would
        // otherwise overwrite it.
        let doc_comment = self.take_pending_doc();
        let generic_params = self.parse_optional_generic_params();
        let where_clause = self.parse_optional_where_clause();

        self.expect(&Token::LeftBrace)?;
        let mut variants = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            let variant = self.parse_variant()?;
            variants.push(variant);
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RightBrace)?;

        Some(EnumDef {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            is_shared,
            name,
            generic_params,
            where_clause,
            variants,
            stdlib_origin: false,
        })
    }

    fn parse_variant(&mut self) -> Option<Variant> {
        // Collect any `///` doc comments preceding this variant. Mirrors
        // the item-level pattern in `parse_item` — collect first, then
        // consume via `take_pending_doc` at construction.
        self.collect_leading_doc_comments();
        let start = self.current_span();
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "enum variant", name_span);
        // Take the variant-level doc *before* descending into a struct
        // payload — per-field doc collection inside `parse_struct_fields`
        // would otherwise overwrite it. Mirrors the same fix in
        // `parse_struct_def` / `parse_enum_def`.
        let doc_comment = self.take_pending_doc();

        let kind = if self.check(&Token::LeftBrace) {
            // Struct variant
            self.advance();
            let fields = self.parse_struct_fields()?;
            self.expect(&Token::RightBrace)?;
            VariantKind::Struct(fields)
        } else if self.check(&Token::LeftParen) {
            // Tuple variant
            self.advance();
            let mut types = Vec::new();
            while !self.check(&Token::RightParen) && !self.is_at_end() {
                types.push(self.parse_type()?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RightParen)?;
            VariantKind::Tuple(types)
        } else {
            VariantKind::Unit
        };

        Some(Variant {
            span: self.span_from(&start),
            doc_comment,
            name,
            kind,
        })
    }

    // ── Traits ───────────────────────────────────────────────────

    /// Parse `marker trait NAME[GENERICS] [: SUPERTRAITS] [where ...]
    /// (";" | "{" "}")` per syntax.md §3.4 / design.md § Marker Traits.
    /// The body must be empty — methods, associated types, and
    /// associated consts inside the body are rejected with a focused
    /// diagnostic. `body_brace` records whether the user wrote `{ }`
    /// (so the formatter can round-trip) or the canonical `;`.
    fn parse_marker_trait(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
    ) -> Option<MarkerTraitDef> {
        let start = self.current_span();
        self.expect(&Token::Marker)?;
        self.expect(&Token::Trait)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "marker trait", name_span);
        let doc_comment = self.take_pending_doc();
        let generic_params = self.parse_optional_generic_params();

        let mut supertraits = Vec::new();
        if self.eat(&Token::Colon) {
            loop {
                let bound = self.parse_trait_bound()?;
                supertraits.push(bound);
                if !self.eat(&Token::Plus) {
                    break;
                }
            }
        }

        let where_clause = self.parse_optional_where_clause();

        let body_brace = if self.eat(&Token::LeftBrace) {
            // Empty-brace form `marker trait Foo { }`. Any item inside is
            // rejected with a focused diagnostic; we recover by skipping
            // to the closing brace so the rest of the file parses.
            while !self.check(&Token::RightBrace) && !self.is_at_end() {
                let body_span = self.current_span();
                let (code, msg) = if self.check(&Token::Fn) {
                    (
                        "E_MARKER_TRAIT_HAS_METHOD",
                        "marker traits cannot declare methods; \
                         remove the method or change `marker trait` to `trait`",
                    )
                } else if self.check(&Token::Type) || self.check(&Token::Const) {
                    (
                        "E_MARKER_TRAIT_HAS_ITEM",
                        "marker traits cannot declare associated types or consts; \
                         remove the item or change `marker trait` to `trait`",
                    )
                } else {
                    (
                        "E_MARKER_TRAIT_HAS_ITEM",
                        "marker traits cannot have a body; \
                         remove the item or change `marker trait` to `trait`",
                    )
                };
                self.errors.push(ParseError {
                    message: format!("error[{code}]: {msg}"),
                    span: body_span,
                });
                // Skip the offending item — advance one token at a time
                // until we see a `}` or matched-fn end (recovery is
                // intentionally conservative; a marker trait body is
                // expected to be empty in practice).
                self.advance();
            }
            self.expect(&Token::RightBrace)?;
            true
        } else {
            self.expect(&Token::Semicolon)?;
            false
        };

        Some(MarkerTraitDef {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            name,
            generic_params,
            supertraits,
            where_clause,
            body_brace,
        })
    }

    /// Top-level dispatch for the `trait` keyword. Reads the trait header
    /// (name + optional generic params), then peeks the next token: `=`
    /// enters the trait-alias path (`trait NAME = bounds;` per v60 item
    /// 40 / design.md § Trait Aliases); anything else falls through to
    /// the regular trait-def path.
    fn parse_trait_or_alias(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
    ) -> Option<Item> {
        let start = self.current_span();
        self.expect(&Token::Trait)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "trait", name_span);
        let doc_comment = self.take_pending_doc();
        let generic_params = self.parse_optional_generic_params();

        if self.check(&Token::Equal) {
            return self
                .parse_trait_alias_tail(
                    attributes,
                    is_pub,
                    is_private,
                    name,
                    generic_params,
                    doc_comment,
                    &start,
                )
                .map(Item::TraitAlias);
        }

        self.parse_trait_def_tail(
            attributes,
            is_pub,
            is_private,
            name,
            generic_params,
            doc_comment,
            &start,
        )
        .map(Item::TraitDef)
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_trait_def_tail(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
        name: String,
        generic_params: Option<GenericParams>,
        doc_comment: Option<String>,
        start: &Span,
    ) -> Option<TraitDef> {
        // Optional supertrait list: `trait Foo: Bar + Baz`
        let mut supertraits = Vec::new();
        if self.eat(&Token::Colon) {
            loop {
                let bound = self.parse_trait_bound()?;
                supertraits.push(bound);
                if !self.eat(&Token::Plus) {
                    break;
                }
            }
        }

        // Optional trait-level effect ceiling: `trait Foo with reads(R)`
        let trait_effects = self.parse_optional_effect_list(&[]);

        let where_clause = self.parse_optional_where_clause();

        self.expect(&Token::LeftBrace)?;
        let mut items = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            if self.check(&Token::Type) {
                let item = self.parse_assoc_type_decl()?;
                items.push(TraitItem::AssocType(item));
            } else {
                // `unsafe fn` in a trait body mirrors the module-scope
                // dispatch in `parse_item`: consume an optional `unsafe`
                // before `fn` and thread it into `parse_trait_method`.
                // `unsafe` followed by anything other than `fn` here is
                // rejected with the same focused diagnostic.
                let is_unsafe = if self.check(&Token::Unsafe) {
                    if self.peek_token_at(1) == Token::Fn {
                        self.advance(); // consume `unsafe`
                        true
                    } else {
                        self.error(
                            "expected `fn` after `unsafe` in trait body — `unsafe` \
                             may only prefix an `unsafe fn` method declaration here.",
                        );
                        self.advance(); // consume `unsafe` for recovery
                        false
                    }
                } else {
                    false
                };
                let method = self.parse_trait_method(is_unsafe)?;
                items.push(TraitItem::Method(Box::new(method)));
            }
        }
        self.expect(&Token::RightBrace)?;

        Some(TraitDef {
            span: self.span_from(start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            name,
            generic_params,
            supertraits,
            trait_effects,
            where_clause,
            items,
            stdlib_origin: false,
        })
    }

    /// Parse the tail of `trait NAME[GENERICS] = bound1 + bound2 + ...
    /// [where ...];` after the header (`trait`, name, generics) has been
    /// consumed. Per syntax.md §3.4 `TRAIT_ALIAS_DEF`. An empty bound
    /// list is a parse error; effect-predicate keywords (`reads`,
    /// `panics`, ...) cannot reach the bound parser, so the
    /// `E_EFFECT_IN_TRAIT_ALIAS` diagnostic from design.md is
    /// structurally unreachable here.
    #[allow(clippy::too_many_arguments)]
    fn parse_trait_alias_tail(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
        name: String,
        generic_params: Option<GenericParams>,
        doc_comment: Option<String>,
        start: &Span,
    ) -> Option<TraitAliasDef> {
        self.expect(&Token::Equal)?;

        // `trait Foo = ;` — empty bound list rejected at parse with a
        // focused diagnostic.
        if self.check(&Token::Semicolon) || self.check(&Token::Where) {
            self.error(
                "trait alias requires at least one trait bound on the right-hand side; \
                 write `trait Foo = SomeTrait;` instead of `trait Foo = ;`",
            );
            // Recover by consuming the rest of the form.
            self.parse_optional_where_clause();
            self.eat(&Token::Semicolon);
            return None;
        }

        // Parse the `+`-separated trait bound list. Effect predicates
        // (`reads`, `writes`, `panics`, ...) are keyword tokens at the
        // lexer level and cannot syntactically appear in trait-bound
        // position, so the design.md `E_EFFECT_IN_TRAIT_ALIAS` diagnostic
        // is structurally unreachable here — `parse_trait_bound` would
        // fail to match the keyword token before the alias parser saw
        // it. Effect-group references for the alias body land via the
        // P1 expansion work alongside the broader trait-alias surface.
        let mut bounds = Vec::new();
        loop {
            let bound = self.parse_trait_bound()?;
            bounds.push(bound);
            if !self.eat(&Token::Plus) {
                break;
            }
        }

        let where_clause = self.parse_optional_where_clause();
        self.expect(&Token::Semicolon)?;

        Some(TraitAliasDef {
            span: self.span_from(start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            name,
            generic_params,
            bounds,
            where_clause,
        })
    }

    fn parse_assoc_type_decl(&mut self) -> Option<AssocTypeDecl> {
        let start = self.current_span();
        self.expect(&Token::Type)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "associated type", name_span);
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
        self.expect(&Token::Semicolon)?;
        Some(AssocTypeDecl {
            span: self.span_from(&start),
            name,
            bounds,
        })
    }

    fn parse_trait_method(&mut self, is_unsafe: bool) -> Option<TraitMethod> {
        let start = self.current_span();
        self.expect(&Token::Fn)?;
        let name = self.expect_method_name()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Value, "fn", name_span);
        let generic_params = self.parse_optional_generic_params();
        let effect_vars: Vec<String> = generic_params
            .as_ref()
            .map(|gp| gp.effect_params.clone())
            .unwrap_or_default();
        self.effect_var_stack.push(effect_vars.clone());

        self.expect(&Token::LeftParen)?;
        self.fn_context_stack.push(FnContext::TraitMethod);
        let (self_param, params) = self.parse_fn_params()?;
        self.fn_context_stack.pop();
        self.expect(&Token::RightParen)?;

        let return_type = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        let effects = self.parse_optional_effect_list(&effect_vars);
        let requires = self.parse_requires_clauses();
        let ensures = self.parse_ensures_clauses();
        let where_clause = self.parse_optional_where_clause();

        // Default method body or required method (semicolon)
        let body = if self.peek_token() == Token::LeftBrace {
            Some(self.parse_block()?)
        } else {
            self.expect(&Token::Semicolon)?;
            None
        };
        self.effect_var_stack.pop();

        Some(TraitMethod {
            span: self.span_from(&start),
            is_unsafe,
            name,
            generic_params,
            self_param,
            params,
            return_type,
            effects,
            requires,
            ensures,
            where_clause,
            body,
        })
    }

    // ── Impl Blocks ──────────────────────────────────────────────

    fn parse_impl_block(&mut self, attributes: Vec<Attribute>) -> Option<ImplBlock> {
        let start = self.current_span();
        self.expect(&Token::Impl)?;

        let generic_params = self.parse_optional_generic_params();

        // Parse the type or trait name
        let first_type = self.parse_type()?;

        // Check if this is `impl Trait for Type`
        let (trait_name, target_type) = if self.eat(&Token::For) {
            let path = match &first_type.kind {
                TypeKind::Path(p) => p.clone(),
                _ => {
                    self.error("Expected trait name before 'for'");
                    return None;
                }
            };
            let target = self.parse_type()?;
            (Some(path), target)
        } else {
            (None, first_type)
        };

        let where_clause = self.parse_optional_where_clause();

        self.expect(&Token::LeftBrace)?;
        let mut items = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            let attrs = self.parse_attributes();
            if self.check(&Token::Type) {
                let binding = self.parse_assoc_type_binding()?;
                items.push(ImplItem::AssocType(binding));
            } else {
                let is_pub = self.eat(&Token::Pub);
                let is_private = if !is_pub {
                    self.eat(&Token::Private)
                } else {
                    false
                };
                // `unsafe fn` inside an impl block mirrors the module-scope
                // dispatch in `parse_item`: consume an optional `unsafe`
                // before `fn` and thread it into `parse_function`. `unsafe`
                // followed by anything other than `fn` inside an impl body
                // is rejected with the same focused diagnostic. There is no
                // `unsafe impl` syntax at v1.
                let is_unsafe = if self.check(&Token::Unsafe) {
                    if self.peek_token_at(1) == Token::Fn {
                        self.advance(); // consume `unsafe`
                        true
                    } else {
                        self.error(
                            "expected `fn` after `unsafe` in impl block — `unsafe` \
                             may only prefix an `unsafe fn` method declaration here.",
                        );
                        self.advance(); // consume `unsafe` for recovery
                        false
                    }
                } else {
                    false
                };
                let method = self.parse_function(attrs, is_pub, is_private, is_unsafe)?;
                items.push(ImplItem::Method(Box::new(method)));
            }
        }
        self.expect(&Token::RightBrace)?;

        Some(ImplBlock {
            span: self.span_from(&start),
            attributes,
            generic_params,
            trait_name,
            target_type,
            where_clause,
            items,
        })
    }

    fn parse_assoc_type_binding(&mut self) -> Option<AssocTypeBinding> {
        let start = self.current_span();
        self.expect(&Token::Type)?;
        let name = self.expect_identifier()?;
        self.expect(&Token::Equal)?;
        let ty = self.parse_type()?;
        self.expect(&Token::Semicolon)?;
        Some(AssocTypeBinding {
            span: self.span_from(&start),
            name,
            ty,
        })
    }

    // ── Effect Declarations ──────────────────────────────────────

    fn parse_effect_decl(
        &mut self,
        is_pub: bool,
        is_stable: bool,
        is_transparent: bool,
    ) -> Option<Item> {
        let start = self.current_span();
        self.expect(&Token::Effect)?;

        if is_transparent {
            if !self.eat(&Token::Verb) {
                self.error("Expected 'verb' after 'transparent effect'");
                return None;
            }
            let name = self.expect_identifier()?;
            let name_span = self.span_from(&start);
            self.check_ident_class(&name, IdentClass::Value, "effect verb", name_span);
            self.expect(&Token::Semicolon)?;
            return Some(Item::EffectVerbDecl(EffectVerbDecl {
                span: self.span_from(&start),
                is_pub,
                is_transparent: true,
                verb_name: name,
            }));
        }

        match self.peek_token() {
            Token::Resource => {
                self.advance();
                let name = self.expect_identifier()?;
                let name_span = self.span_from(&start);
                self.check_ident_class(&name, IdentClass::Type, "effect resource", name_span);
                let generic_params = self.parse_optional_generic_params();
                let provider_trait = if self.eat(&Token::Colon) {
                    Some(self.expect_identifier()?)
                } else {
                    None
                };
                self.expect(&Token::Semicolon)?;
                Some(Item::EffectResource(EffectResourceDecl {
                    span: self.span_from(&start),
                    name,
                    generic_params,
                    provider_trait,
                }))
            }
            Token::Group => {
                self.advance();
                let name = self.expect_identifier()?;
                let name_span = self.span_from(&start);
                self.check_ident_class(&name, IdentClass::Value, "effect group", name_span);
                self.expect(&Token::Equal)?;
                let body = self.parse_effect_group_body()?;
                self.expect(&Token::Semicolon)?;
                Some(Item::EffectGroup(EffectGroupDecl {
                    span: self.span_from(&start),
                    is_pub,
                    is_stable,
                    name,
                    body,
                }))
            }
            Token::Verb => {
                self.advance();
                let name = self.expect_identifier()?;
                let name_span = self.span_from(&start);
                self.check_ident_class(&name, IdentClass::Value, "effect verb", name_span);
                self.expect(&Token::Semicolon)?;
                Some(Item::EffectVerbDecl(EffectVerbDecl {
                    span: self.span_from(&start),
                    is_pub,
                    is_transparent: false,
                    verb_name: name,
                }))
            }
            _ => {
                self.error("Expected 'resource', 'group', or 'verb' after 'effect'");
                None
            }
        }
    }

    fn parse_effect_group_body(&mut self) -> Option<Vec<EffectGroupTerm>> {
        let mut terms = Vec::new();
        loop {
            if let Some(verb) = self.try_parse_effect_verb() {
                terms.push(EffectGroupTerm::Verb(verb));
            } else if let Token::Identifier { .. } = self.peek_token() {
                let name = self.expect_identifier()?;
                terms.push(EffectGroupTerm::GroupRef(name));
            } else {
                break;
            }

            if !self.eat(&Token::Plus) {
                break;
            }
        }
        Some(terms)
    }

    // ── Effect Annotations ───────────────────────────────────────

    fn parse_optional_effect_list(&mut self, effect_vars: &[String]) -> Option<EffectList> {
        // Effects start with an effect verb keyword or `with`
        if !self.is_effect_start() {
            return None;
        }
        self.parse_effect_list(effect_vars)
    }

    fn is_effect_start(&self) -> bool {
        matches!(
            self.peek_token(),
            Token::Reads
                | Token::Writes
                | Token::Sends
                | Token::Receives
                | Token::Allocates
                | Token::Panics
                | Token::Blocks
                | Token::Suspends
                | Token::With
        )
    }

    fn parse_effect_list(&mut self, effect_vars: &[String]) -> Option<EffectList> {
        let start = self.current_span();
        let mut items = Vec::new();

        // Handle `with` keyword
        if self.eat(&Token::With) {
            // with _ (anonymous polymorphic)
            if self.eat(&Token::Underscore) {
                return Some(EffectList {
                    items: vec![EffectItem::Polymorphic],
                    span: self.span_from(&start),
                });
            }

            // Parse space-separated effect items: verbs, effect variables, or group names
            loop {
                // Try verb first (reads, writes, blocks, user-defined, etc.)
                if let Some(verb) = self.try_parse_effect_verb() {
                    items.push(EffectItem::Verb(verb));
                } else if let Token::Identifier { .. } = self.peek_token() {
                    let name = self.expect_identifier()?;
                    // Named effect variable declared in the function's [with E] params
                    // takes precedence over effect group lookup.
                    if effect_vars.contains(&name) {
                        items.push(EffectItem::Variable(name));
                    } else {
                        items.push(EffectItem::Group(name));
                    }
                } else {
                    break;
                }
            }

            if items.is_empty() {
                self.error("Expected effect verb, effect variable, or group name after 'with'");
                return None;
            }

            return Some(EffectList {
                items,
                span: self.span_from(&start),
            });
        }

        // Parse verb-based effect list (without `with` prefix — for effect groups in decls)
        while let Some(verb) = self.try_parse_effect_verb() {
            items.push(EffectItem::Verb(verb));
        }

        if items.is_empty() {
            None
        } else {
            Some(EffectList {
                items,
                span: self.span_from(&start),
            })
        }
    }

    fn try_parse_effect_verb(&mut self) -> Option<EffectVerb> {
        let start = self.current_span();
        let kind = match self.peek_token() {
            Token::Reads => {
                self.advance();
                EffectVerbKind::Reads
            }
            Token::Writes => {
                self.advance();
                EffectVerbKind::Writes
            }
            Token::Sends => {
                self.advance();
                EffectVerbKind::Sends
            }
            Token::Receives => {
                self.advance();
                EffectVerbKind::Receives
            }
            Token::Allocates => {
                self.advance();
                EffectVerbKind::Allocates
            }
            Token::Panics => {
                self.advance();
                return Some(EffectVerb {
                    kind: EffectVerbKind::Panics,
                    resources: Vec::new(),
                    span: self.span_from(&start),
                });
            }
            Token::Blocks => {
                self.advance();
                EffectVerbKind::Blocks
            }
            Token::Suspends => {
                self.advance();
                EffectVerbKind::Suspends
            }
            Token::Identifier { name, .. } if self.peek_ahead_is_left_paren() => {
                let name = name.clone();
                self.advance();
                EffectVerbKind::UserDefined(name)
            }
            _ => return None,
        };

        // Blocks and suspends may appear without resources (like panics)
        if !self.check(&Token::LeftParen) {
            return Some(EffectVerb {
                kind,
                resources: Vec::new(),
                span: self.span_from(&start),
            });
        }

        self.expect(&Token::LeftParen)?;
        let mut resources = Vec::new();
        loop {
            let res = self.parse_resource()?;
            resources.push(res);
            if !self.eat(&Token::Comma) {
                break;
            }
            if self.check(&Token::RightParen) {
                break;
            }
        }
        self.expect(&Token::RightParen)?;

        Some(EffectVerb {
            kind,
            resources,
            span: self.span_from(&start),
        })
    }

    /// Check if the token after the current one is a left paren (used for user-defined verb lookahead)
    fn peek_ahead_is_left_paren(&self) -> bool {
        if self.pos + 1 < self.tokens.len() {
            self.tokens[self.pos + 1].token == Token::LeftParen
        } else {
            false
        }
    }

    fn parse_resource(&mut self) -> Option<Resource> {
        let start = self.current_span();
        let path = self.parse_path_segments()?;

        let param = if self.eat(&Token::LeftBracket) {
            let expr = self.parse_expression()?;
            self.expect(&Token::RightBracket)?;
            Some(Box::new(expr))
        } else {
            None
        };

        Some(Resource {
            path,
            param,
            span: self.span_from(&start),
        })
    }

    // ── Layout ───────────────────────────────────────────────────

    fn parse_layout_def(&mut self, attributes: Vec<Attribute>, is_pub: bool) -> Option<LayoutDef> {
        let start = self.current_span();
        self.expect(&Token::Layout)?;
        let name = self.expect_identifier()?;
        // Layout names are Value-class — they bind to a logical collection
        // (e.g., `layout entities: Collection[Entity]`). The Type-class
        // identifier is the *element* type, not the layout itself.
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Value, "layout", name_span);
        self.expect(&Token::Colon)?;
        let collection_type = self.parse_type()?;

        self.expect(&Token::LeftBrace)?;
        let mut items = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            if self.check(&Token::Group) {
                let gs = self.current_span();
                self.advance();
                let group_name = self.expect_identifier()?;
                self.expect(&Token::LeftBrace)?;
                let fields = self.parse_layout_field_list()?;
                self.expect(&Token::RightBrace)?;
                // Optional `align(N)` modifier after the closing brace.
                let align = if matches!(self.peek_token(), Token::Identifier { ref name, .. } if name == "align")
                {
                    self.advance(); // consume `align`
                    self.expect(&Token::LeftParen)?;
                    let n = match self.peek_token() {
                        Token::Integer(n, _) => {
                            let v = n as u32;
                            self.advance();
                            v
                        }
                        _ => {
                            self.error("align(N) requires an integer literal");
                            0
                        }
                    };
                    self.expect(&Token::RightParen)?;
                    Some(n)
                } else {
                    None
                };
                items.push(LayoutItem::Group {
                    name: group_name,
                    fields,
                    align,
                    span: self.span_from(&gs),
                });
            } else {
                match self.peek_token() {
                    Token::Identifier { ref name, .. } if name == "cold" => {
                        let cs = self.current_span();
                        self.advance(); // consume `cold`
                        self.expect(&Token::LeftBrace)?;
                        let fields = self.parse_layout_field_list()?;
                        self.expect(&Token::RightBrace)?;
                        items.push(LayoutItem::Cold {
                            fields,
                            span: self.span_from(&cs),
                        });
                    }
                    Token::Identifier { ref name, .. } if name == "split_by_variant" => {
                        let s = self.current_span();
                        self.advance();
                        items.push(LayoutItem::SplitByVariant(s));
                    }
                    _ => {
                        self.error(
                            "Expected 'group', 'cold', or 'split_by_variant' in layout block",
                        );
                        self.advance();
                    }
                }
            }
        }
        self.expect(&Token::RightBrace)?;

        let doc_comment = self.take_pending_doc();
        Some(LayoutDef {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            name,
            collection_type,
            items,
        })
    }

    /// Parse a comma-separated list of identifiers (field names) inside a layout group or cold body.
    /// Caller must consume the opening `{` before calling and the closing `}` after.
    fn parse_layout_field_list(&mut self) -> Option<Vec<String>> {
        let mut fields = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            fields.push(self.expect_identifier()?);
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        Some(fields)
    }

    // ── Module & Import ──────────────────────────────────────────

    /// Per `docs/design.md § Module System` / brainstorming_v41.md §M1b,
    /// `mod name;` declarations do not exist in Kāra — the module tree is
    /// derived from the directory layout. The `mod` keyword stays reserved
    /// (a future inline-module feature could claim it), but at parse time
    /// every `mod ... ;` form is rejected with a directive-style diagnostic.
    /// We consume tokens through the trailing semicolon (or stop at the next
    /// item-starting token, whichever comes first) so a misplaced `mod`
    /// declaration does not poison the rest of the file.
    fn reject_mod_decl(&mut self) {
        let start = self.current_span();
        self.advance(); // consume `mod`
                        // Consume an optional identifier and the rest of the declaration up
                        // to the next semicolon, so the parser resumes cleanly on the next
                        // item. If the user wrote `mod` followed by something other than the
                        // canonical `name ;` form (e.g. `mod foo { ... }`), the resync below
                        // will stop at the next item-starting token via the outer
                        // `synchronize_to_item` pass.
        if let Token::Identifier { .. } = self.peek_token() {
            self.advance();
        }
        let _ = self.eat(&Token::Semicolon);
        let span = self.span_from(&start);
        self.errors.push(ParseError {
            message: "`mod` declarations are not used in Kāra — module structure is derived from the directory tree. Each `.kara` file is its own module; put this file in the appropriate directory to define its module path. See `docs/design.md § Module System`."
                .to_string(),
            span,
        });
    }

    fn parse_use_decl(&mut self, is_pub: bool) -> Option<UseDecl> {
        let start = self.current_span();
        self.expect(&Token::Use)?;
        let path = self.parse_path_segments()?;
        self.expect(&Token::Semicolon)?;
        Some(UseDecl {
            span: self.span_from(&start),
            is_pub,
            path,
        })
    }

    /// Parse `import` declarations per design.md § Module System:
    ///
    /// ```text
    /// import a.b.Item;
    /// import a.b.Item as X;
    /// import a.b.{A, B as X};
    /// import a.b;            // binds the last segment (module or item)
    /// pub import a.b.Item;   // re-export (slice 7 consumer)
    /// ```
    ///
    /// `ImportDecl.path` is the module prefix (everything before the item /
    /// last-segment binding) and `ImportDecl.items` lists the names bound in
    /// the current scope. Wildcard and nested grouping are deferred.
    fn parse_import_decl(&mut self, is_pub: bool) -> Option<ImportDecl> {
        let start = self.current_span();
        self.expect(&Token::Import)?;

        // Collect the dotted prefix up to the first `{` or the final segment.
        let mut prefix: Vec<(String, Span)> = Vec::new();
        let first_span = self.current_span();
        let first_name = self.expect_identifier()?;
        prefix.push((first_name, first_span));

        let mut items: Vec<ImportItem> = Vec::new();

        loop {
            if self.eat(&Token::Dot) {
                // After a `.`, either an identifier continues the path or a
                // `{` opens a brace-grouped item list.
                if self.check(&Token::LeftBrace) {
                    self.advance();
                    items = self.parse_import_item_list()?;
                    self.expect(&Token::RightBrace)?;
                    break;
                }
                let seg_span = self.current_span();
                let seg = self.expect_identifier()?;
                prefix.push((seg, seg_span));
                continue;
            }
            // Dot-free path ended. The last `prefix` entry is the bound name.
            break;
        }

        if items.is_empty() {
            // Bare `import a.b.c;` or `import a.b.c as X;` — the last segment
            // is the item, everything before is the path prefix.
            let (name, name_span) = prefix.pop().expect("at least one segment parsed");
            let alias = if self.eat(&Token::As) {
                Some(self.expect_identifier()?)
            } else {
                None
            };
            items.push(ImportItem {
                span: name_span,
                name,
                alias,
            });
        }

        self.expect(&Token::Semicolon)?;

        let (path, path_spans): (Vec<String>, Vec<Span>) = prefix.into_iter().unzip();
        Some(ImportDecl {
            span: self.span_from(&start),
            is_pub,
            path,
            path_spans,
            items,
        })
    }

    /// Parse the body of `import path.{ ... };` — a comma-separated list of
    /// `Name` or `Name as Alias`, with optional trailing comma.
    fn parse_import_item_list(&mut self) -> Option<Vec<ImportItem>> {
        let mut items = Vec::new();
        loop {
            if self.check(&Token::RightBrace) {
                break;
            }
            let name_span = self.current_span();
            let name = self.expect_identifier()?;
            let alias = if self.eat(&Token::As) {
                Some(self.expect_identifier()?)
            } else {
                None
            };
            items.push(ImportItem {
                span: name_span,
                name,
                alias,
            });
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        if items.is_empty() {
            self.error("empty import group — `import path.{}` is not allowed");
            return None;
        }
        Some(items)
    }

    // ── Constants ────────────────────────────────────────────────

    fn parse_const_decl(&mut self, is_pub: bool, is_private: bool) -> Option<ConstDecl> {
        let start = self.current_span();
        self.expect(&Token::Const)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Const, "const", name_span);
        self.expect(&Token::Colon)?;
        let ty = self.parse_type()?;
        self.expect(&Token::Equal)?;
        let value = self.parse_expression()?;
        self.expect(&Token::Semicolon)?;
        let doc_comment = self.take_pending_doc();
        Some(ConstDecl {
            span: self.span_from(&start),
            doc_comment,
            is_pub,
            is_private,
            name,
            ty,
            value,
        })
    }

    // ── Alias & Independent ──────────────────────────────────────

    fn parse_alias_decl(&mut self) -> Option<AliasDecl> {
        let start = self.current_span();
        self.expect(&Token::Alias)?;
        let left = self.parse_path_segments()?;
        self.expect(&Token::Equal)?;
        let right = self.parse_path_segments()?;
        self.expect(&Token::Semicolon)?;
        Some(AliasDecl {
            span: self.span_from(&start),
            left,
            right,
        })
    }

    fn parse_independent_decl(&mut self) -> Option<IndependentDecl> {
        let start = self.current_span();
        self.expect(&Token::Independent)?;
        let left = self.parse_path_segments()?;
        self.expect(&Token::Comma)?;
        let right = self.parse_path_segments()?;
        self.expect(&Token::Semicolon)?;
        Some(IndependentDecl {
            span: self.span_from(&start),
            left,
            right,
        })
    }

    // ── Extern Functions (FFI) ───────────────────────────────────

    /// Parse an `unsafe extern "ABI" { ... }` block at module scope.
    /// The leading `unsafe` keyword has not yet been consumed; the
    /// `pending_doc` slot holds any `///` lines that preceded the
    /// block. `block_attributes` is the set of `#[...]` / `@...`
    /// attributes the caller already parsed — they live on the
    /// `ExternBlock` itself; downstream consumers (effectchecker,
    /// codegen) read both the block-level set and per-item set so the
    /// formatter can round-trip the block-level position faithfully.
    fn parse_unsafe_extern_block(
        &mut self,
        block_attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
    ) -> Option<ExternBlock> {
        let start = self.current_span();
        // Capture the block's own doc-comment before we descend.
        let block_doc = self.take_pending_doc();

        self.expect(&Token::Unsafe)?;

        // `unsafe` at module scope is only valid as the block-header
        // prefix for `unsafe extern "ABI" { ... }`. Any other shape
        // (`unsafe fn`, `unsafe { ... }` at module scope, etc.) is
        // rejected here with a focused diagnostic.
        if !self.check(&Token::Extern) {
            self.error(
                "expected `extern` after `unsafe` at module scope — \
                 `unsafe` at module scope is only valid as the prefix \
                 of an `unsafe extern \"ABI\" { ... }` block. \
                 See design.md § FFI > `unsafe extern { ... }` block requirement.",
            );
            return None;
        }

        if is_pub || is_private {
            self.error(
                "visibility (`pub`/`private`) on an `unsafe extern { ... }` \
                 block is not meaningful — apply visibility to each item \
                 inside the block instead.",
            );
            // continue parsing for better error recovery
        }

        self.expect(&Token::Extern)?;

        let abi = match self.peek_token() {
            Token::StringLiteral(s) => {
                let s = s.clone();
                self.advance();
                s
            }
            _ => {
                self.error("expected ABI string (e.g., \"C\") after `extern`");
                return None;
            }
        };

        self.expect(&Token::LeftBrace)?;

        let mut items = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            let item_start = self.current_span();

            // Per-item leading doc + attributes. Block-level attributes
            // live on the `ExternBlock` and are NOT pre-merged here; the
            // round-trip through the formatter must preserve which
            // attributes were written at the block level vs. per-item.
            // Downstream consumers that need the union (effectchecker,
            // codegen) take both sets explicitly.
            self.collect_leading_doc_comments();
            let attributes = self.parse_attributes();

            // Take the item-level doc *now*, before descending into a
            // param list — per-param doc collection inside `parse_param`
            // would otherwise overwrite it.
            let doc_comment = self.take_pending_doc();

            let is_pub = self.eat(&Token::Pub);
            let is_private = if !is_pub {
                self.eat(&Token::Private)
            } else {
                false
            };

            // Dispatch on the next significant token: `fn` for a foreign
            // function declaration, `type` for an opaque foreign type
            // declaration. Anything else is a focused diagnostic.
            match self.peek_token() {
                Token::Fn => {
                    let item = self.parse_extern_block_item_fn(
                        &abi,
                        attributes,
                        doc_comment,
                        is_pub,
                        is_private,
                        item_start,
                    )?;
                    items.push(ExternItem::Function(Box::new(item)));
                }
                Token::Type => {
                    let item = self.parse_extern_block_item_opaque_type(
                        attributes,
                        doc_comment,
                        is_pub,
                        is_private,
                        item_start,
                    )?;
                    items.push(ExternItem::OpaqueType(item));
                }
                _ => {
                    self.error(
                        "expected `fn` or `type` inside `unsafe extern { ... }` block — \
                         only foreign function declarations (`fn name(...) -> T;`) and \
                         opaque foreign type declarations (`type Name;`) are legal here. \
                         See design.md § FFI > `unsafe extern { ... }` block requirement.",
                    );
                    return None;
                }
            }
        }
        self.expect(&Token::RightBrace)?;

        Some(ExternBlock {
            span: self.span_from(&start),
            attributes: block_attributes,
            doc_comment: block_doc,
            abi,
            items,
        })
    }

    /// Parse a single `fn name(...) -> T effects;` item *inside* an
    /// `unsafe extern { ... }` block. The leading doc / attributes /
    /// visibility have already been consumed by the dispatcher in
    /// `parse_unsafe_extern_block`; this function picks up at the `fn`
    /// keyword. `attributes` is the union of the block's pre-merged
    /// attributes and any per-item attributes.
    fn parse_extern_block_item_fn(
        &mut self,
        block_abi: &str,
        attributes: Vec<Attribute>,
        doc_comment: Option<String>,
        is_pub: bool,
        is_private: bool,
        start: Span,
    ) -> Option<ExternFunction> {
        self.expect(&Token::Fn)?;
        let name = self.expect_identifier()?;
        // Skip naming check when `#[kara_name]` is present — the C-side
        // name is the canonical identifier; the Kāra binding name is
        // whatever the user wants.
        if !attributes.iter().any(|a| a.name == "kara_name") {
            let name_span = self.span_from(&start);
            self.check_ident_class(&name, IdentClass::Value, "extern function", name_span);
        }
        self.expect(&Token::LeftParen)?;

        self.fn_context_stack.push(FnContext::Function);
        let mut params = Vec::new();
        while !self.check(&Token::RightParen) && !self.is_at_end() {
            params.push(self.parse_param()?);
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.fn_context_stack.pop();
        self.expect(&Token::RightParen)?;

        let return_type = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };

        let effects = self.parse_optional_effect_list(&[]);
        self.expect(&Token::Semicolon)?;

        Some(ExternFunction {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            abi: block_abi.to_string(),
            name,
            params,
            return_type,
            effects,
        })
    }

    /// Parse a single `type Name;` opaque foreign type declaration
    /// inside an `unsafe extern { ... }` block. The leading doc /
    /// attributes / visibility have already been consumed by the
    /// dispatcher in `parse_unsafe_extern_block`; this function picks
    /// up at the `type` keyword.
    ///
    /// Rejects `type Name[T];` (generics — `E_OPAQUE_TYPE_GENERIC_FORBIDDEN`
    /// per design.md) and `type Name { ... }` (body — opaque types have
    /// no fields by definition). Requires a trailing `;`.
    fn parse_extern_block_item_opaque_type(
        &mut self,
        attributes: Vec<Attribute>,
        doc_comment: Option<String>,
        is_pub: bool,
        is_private: bool,
        start: Span,
    ) -> Option<OpaqueTypeDecl> {
        self.expect(&Token::Type)?;
        let name_start = self.current_span();
        let name = self.expect_identifier()?;
        // Opaque foreign type names follow CN-1 (Type-class identifier)
        // when surfaced on the Kāra side; `#[kara_name = "..."]` rebinds
        // a non-conforming foreign name per CN-8.
        if !attributes.iter().any(|a| a.name == "kara_name") {
            let name_span = self.span_from(&name_start);
            self.check_ident_class(&name, IdentClass::Type, "opaque foreign type", name_span);
        }

        // Reject generics: `type Name[T];`. C does not have generic
        // types; if the binding needs to be generic on Kāra's side,
        // the wrapper type carries the parameter (per design.md).
        if self.check(&Token::LeftBracket) {
            self.error(
                "opaque foreign type declarations cannot be generic. \
                 C does not have generic types — if the binding needs to \
                 be parameterised on the Kāra side, declare a wrapper type \
                 (`distinct type` or `struct`) that carries the parameter \
                 and stores a `*mut Foo` internally.",
            );
            return None;
        }

        // Reject body: `type Name { ... }`. Opaque types have no fields,
        // no methods, no derives — that's the entire point of the form.
        if self.check(&Token::LeftBrace) {
            self.error(
                "opaque foreign type declarations have no body. \
                 The form is `type Name;` — the type's layout is private \
                 to the foreign library, so there are no fields, methods, \
                 or derives. Use `#[repr(C)] struct Name { ... }` instead \
                 if the C side publishes the layout.",
            );
            return None;
        }

        self.expect(&Token::Semicolon)?;

        Some(OpaqueTypeDecl {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            name,
        })
    }

    /// Diagnostic for bare module-scope `extern "ABI" fn ...;` and
    /// `extern "ABI" { ... }` (no `unsafe` keyword). Foreign-import
    /// declarations must live inside an `unsafe extern "ABI" { ... }`
    /// block per design.md § FFI > `unsafe extern { ... }` block
    /// requirement. After emitting, the parser swallows the offending
    /// declaration end-to-end (up to and including the next `;` or
    /// balanced `{ }` body) so the error doesn't cascade into a
    /// "missing brace" or "missing semicolon" follow-up.
    fn error_bare_extern_at_module_scope(&mut self) {
        self.error(
            "bare `extern \"ABI\" ...` is not allowed at module scope. \
             Foreign-import declarations must live inside an \
             `unsafe extern \"ABI\" { ... }` block — the `unsafe` keyword \
             marks the trust boundary at which the programmer asserts the \
             foreign signature, ABI, and effect set faithfully describe the \
             foreign symbol. See design.md § FFI > `unsafe extern { ... }` \
             block requirement.",
        );
        // Recovery: consume tokens until we cleanly close the offending
        // declaration. Two shapes to handle: `extern "ABI" fn ...;`
        // (semicolon-terminated) and `extern "ABI" { ... }` (block).
        self.advance(); // consume `extern`
        let mut brace_depth = 0_i32;
        while !self.is_at_end() {
            match self.peek_token() {
                Token::LeftBrace => {
                    brace_depth += 1;
                    self.advance();
                }
                Token::RightBrace => {
                    self.advance();
                    brace_depth -= 1;
                    if brace_depth <= 0 {
                        return;
                    }
                }
                Token::Semicolon if brace_depth == 0 => {
                    self.advance();
                    return;
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    // ── Type Aliases ─────────────────────────────────────────────

    fn parse_type_alias(&mut self, is_pub: bool, is_private: bool) -> Option<TypeAliasDef> {
        let start = self.current_span();
        self.expect(&Token::Type)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "type alias", name_span);
        let generic_params = self.parse_optional_generic_params();
        self.expect(&Token::Equal)?;
        let ty = self.parse_type()?;
        let refinement = if self.eat(&Token::Where) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        self.expect(&Token::Semicolon)?;
        let doc_comment = self.take_pending_doc();
        Some(TypeAliasDef {
            span: self.span_from(&start),
            doc_comment,
            is_pub,
            is_private,
            name,
            generic_params,
            ty,
            refinement,
        })
    }

    // ── Distinct Types ─────────────────────────────────────────────

    fn parse_distinct_type(
        &mut self,
        attributes: Vec<Attribute>,
        is_pub: bool,
        is_private: bool,
    ) -> Option<DistinctTypeDef> {
        let start = self.current_span();
        self.expect(&Token::Distinct)?;
        if !self.eat(&Token::Type) {
            self.error("Expected 'type' after 'distinct'");
            return None;
        }
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "distinct type", name_span);
        let generic_params = self.parse_optional_generic_params();
        self.expect(&Token::Equal)?;
        let base_type = self.parse_type()?;
        let refinement = if self.eat(&Token::Where) {
            Some(self.parse_expression()?)
        } else {
            None
        };
        self.expect(&Token::Semicolon)?;
        let doc_comment = self.take_pending_doc();
        Some(DistinctTypeDef {
            span: self.span_from(&start),
            attributes,
            doc_comment,
            is_pub,
            is_private,
            name,
            generic_params,
            base_type,
            refinement,
        })
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

    // ── Expressions (Pratt Parser) ───────────────────────────────

    fn parse_expression(&mut self) -> Option<Expr> {
        self.parse_expr_bp(0)
    }

    /// Parse an expression in statement context. Differs from
    /// `parse_expression` only when the prefix is a block-like expression
    /// (see `is_block_like_prefix`): in that case, postfix operators are
    /// not consumed, so the closing `}` ends the statement and the next
    /// token starts a fresh one.
    fn parse_expression_stmt(&mut self) -> Option<Expr> {
        self.parse_expr_bp_with_ctx(0, true)
    }

    fn parse_expr_bp(&mut self, min_bp: u8) -> Option<Expr> {
        self.parse_expr_bp_with_ctx(min_bp, false)
    }

    fn parse_expr_bp_with_ctx(&mut self, min_bp: u8, stmt_ctx: bool) -> Option<Expr> {
        let mut lhs = self.parse_prefix()?;

        if stmt_ctx && Self::is_block_like_prefix(&lhs) {
            return Some(lhs);
        }

        loop {
            // Check for postfix operators first
            match self.peek_token() {
                Token::Question => {
                    self.advance();
                    lhs = Expr {
                        span: lhs.span.clone(),
                        kind: ExprKind::Question(Box::new(lhs)),
                    };
                    continue;
                }
                Token::QuestionDot => {
                    self.advance();
                    let field_or_method = self.expect_identifier()?;
                    let args = if self.check(&Token::LeftParen) {
                        self.advance();
                        let a = self.parse_arg_list()?;
                        self.expect(&Token::RightParen)?;
                        Some(a)
                    } else {
                        None
                    };
                    lhs = Expr {
                        span: lhs.span.clone(),
                        kind: ExprKind::OptionalChain {
                            object: Box::new(lhs),
                            field_or_method,
                            args,
                        },
                    };
                    continue;
                }
                Token::As => {
                    let (l_bp, _) = (23, 24); // Between multiplicative and unary
                    if l_bp < min_bp {
                        break;
                    }
                    self.advance();
                    let ty = self.parse_type()?;
                    lhs = Expr {
                        span: lhs.span.clone(),
                        kind: ExprKind::Cast {
                            expr: Box::new(lhs),
                            ty,
                        },
                    };
                    continue;
                }
                Token::Dot => {
                    self.advance();
                    // Field access, tuple index, or method call
                    match self.peek_token() {
                        Token::Integer(idx, _) => {
                            self.advance();
                            lhs = Expr {
                                span: lhs.span.clone(),
                                kind: ExprKind::TupleIndex {
                                    object: Box::new(lhs),
                                    index: idx as u64,
                                },
                            };
                        }
                        Token::Identifier { .. } => {
                            let method = self.expect_identifier()?;
                            let turbofish = None;
                            if self.check(&Token::LeftParen) {
                                // Method call
                                self.advance();
                                let args = self.parse_arg_list()?;
                                self.expect(&Token::RightParen)?;
                                lhs = Expr {
                                    span: lhs.span.clone(),
                                    kind: ExprKind::MethodCall {
                                        object: Box::new(lhs),
                                        method,
                                        turbofish,
                                        args,
                                    },
                                };
                            } else {
                                // Field access
                                lhs = Expr {
                                    span: lhs.span.clone(),
                                    kind: ExprKind::FieldAccess {
                                        object: Box::new(lhs),
                                        field: method,
                                    },
                                };
                            }
                        }
                        _ => {
                            self.error("Expected field name or tuple index after '.'");
                            return None;
                        }
                    }
                    continue;
                }
                Token::LeftParen => {
                    // Function call
                    self.advance();
                    let args = self.parse_arg_list()?;
                    self.expect(&Token::RightParen)?;
                    lhs = Expr {
                        span: lhs.span.clone(),
                        kind: ExprKind::Call {
                            callee: Box::new(lhs),
                            args,
                        },
                    };
                    continue;
                }
                Token::LeftBracket => {
                    // Generic-args call disambiguation (const generics
                    // slice 1b): `Identifier[T1, T2, ...](...)` shapes
                    // where the bracket contents contain a `,` separator
                    // and the matching `]` is immediately followed by
                    // `(` route through `ExprKind::Path` so the call site
                    // carries explicit type/const generic args. Single-arg
                    // brackets stay as `Index` to preserve the existing
                    // `callbacks[0]()` (indexed-function-call) shape;
                    // single-arg-with-explicit-generic-args is a future
                    // slice's territory.
                    if self.lookahead_generic_args_call(&lhs) {
                        let segments = match &lhs.kind {
                            ExprKind::Identifier(n) => vec![n.clone()],
                            ExprKind::Path {
                                segments,
                                generic_args: None,
                            } => segments.clone(),
                            _ => unreachable!(),
                        };
                        let gen_args = self.parse_generic_type_args()?;
                        lhs = Expr {
                            span: lhs.span.clone(),
                            kind: ExprKind::Path {
                                segments,
                                generic_args: Some(gen_args),
                            },
                        };
                        continue;
                    }
                    // Index
                    self.advance();
                    let index = self.parse_expression()?;
                    self.expect(&Token::RightBracket)?;
                    lhs = Expr {
                        span: lhs.span.clone(),
                        kind: ExprKind::Index {
                            object: Box::new(lhs),
                            index: Box::new(index),
                        },
                    };
                    continue;
                }
                _ => {}
            }

            // Pipe operator |> — lowest binary precedence
            if self.check(&Token::PipeArrow) {
                let pipe_bp = 2; // lowest
                if pipe_bp < min_bp {
                    break;
                }
                self.advance();
                let rhs = self.parse_expr_bp(pipe_bp + 1)?;
                lhs = Expr {
                    span: lhs.span.clone(),
                    kind: ExprKind::Pipe {
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                    },
                };
                continue;
            }

            // Nil-coalesce ?? — between pipe and ||
            if self.check(&Token::QuestionQuestion) {
                let nil_bp = 4; // above pipe (2), below || (6)
                if nil_bp < min_bp {
                    break;
                }
                self.advance();
                let rhs = self.parse_expr_bp(nil_bp + 1)?;
                lhs = Expr {
                    span: lhs.span.clone(),
                    kind: ExprKind::NilCoalesce {
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                    },
                };
                continue;
            }

            // Range operators — low precedence (above nil-coalesce)
            if self.check(&Token::DotDot) || self.check(&Token::DotDotEq) {
                let range_bp = 5;
                if range_bp < min_bp {
                    break;
                }
                let inclusive = self.check(&Token::DotDotEq);
                self.advance();
                // `expr..` with no RHS → RangeFrom; only when next token cannot
                // start an expression (semicolon, right-brace, comma, right-bracket,
                // right-paren, or end-of-input).
                let has_rhs = !self.is_at_end()
                    && !matches!(
                        self.peek_token(),
                        Token::Semicolon
                            | Token::RightBrace
                            | Token::Comma
                            | Token::RightBracket
                            | Token::RightParen
                    );
                let end = if has_rhs {
                    Some(Box::new(self.parse_expr_bp(range_bp + 1)?))
                } else {
                    None
                };
                lhs = Expr {
                    span: lhs.span.clone(),
                    kind: ExprKind::Range {
                        start: Some(Box::new(lhs)),
                        end,
                        inclusive,
                    },
                };
                continue;
            }

            // Check for infix (binary) operators
            let (op, l_bp, r_bp) = match self.peek_token() {
                Token::Or => (BinOp::Or, 6, 7),
                Token::And => (BinOp::And, 8, 9),
                Token::PipePipe => {
                    self.error("the `||` operator is not used in Kāra; use `or` instead");
                    (BinOp::Or, 6, 7)
                }
                Token::AmpAmp => {
                    self.error("the `&&` operator is not used in Kāra; use `and` instead");
                    (BinOp::And, 8, 9)
                }
                Token::EqualEqual => (BinOp::Eq, 10, 11),
                Token::BangEqual => (BinOp::NotEq, 10, 11),
                Token::LessThan => (BinOp::Lt, 10, 11),
                Token::LessThanOrEqual => (BinOp::LtEq, 10, 11),
                Token::GreaterThan => (BinOp::Gt, 10, 11),
                Token::GreaterThanOrEqual => (BinOp::GtEq, 10, 11),
                Token::Pipe => (BinOp::BitOr, 12, 13),
                Token::Caret => (BinOp::BitXor, 14, 15),
                Token::Amp => (BinOp::BitAnd, 16, 17),
                Token::LessLess => (BinOp::Shl, 18, 19),
                Token::GreaterGreater => (BinOp::Shr, 18, 19),
                Token::Plus => (BinOp::Add, 20, 21),
                Token::Minus => (BinOp::Sub, 20, 21),
                Token::Star => (BinOp::Mul, 22, 23),
                Token::Slash => (BinOp::Div, 22, 23),
                Token::Percent => (BinOp::Mod, 22, 23),
                _ => break,
            };

            if l_bp < min_bp {
                break;
            }

            self.advance();
            let rhs = self.parse_expr_bp(r_bp)?;

            // Span the full operator expression (lhs start → rhs end) so the
            // Binary node has a unique SpanKey distinct from its LHS. This
            // matters for the lowering pass and any side-table that keys on
            // expression spans.
            let span = Span {
                line: lhs.span.line,
                column: lhs.span.column,
                offset: lhs.span.offset,
                length: (rhs.span.offset + rhs.span.length).saturating_sub(lhs.span.offset),
            };
            lhs = Expr {
                span,
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                },
            };
        }

        Some(lhs)
    }

    fn parse_prefix(&mut self) -> Option<Expr> {
        let start = self.current_span();

        match self.peek_token() {
            // Unary operators
            Token::Minus => {
                self.advance();
                let operand = self.parse_expr_bp(24)?; // Unary has high precedence
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Unary {
                        op: UnaryOp::Neg,
                        operand: Box::new(operand),
                    },
                })
            }
            Token::Not => {
                self.advance();
                let operand = self.parse_expr_bp(24)?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Unary {
                        op: UnaryOp::Not,
                        operand: Box::new(operand),
                    },
                })
            }
            Token::Bang => {
                self.error("the `!` operator is not used in Kāra; use `not` instead");
                self.advance();
                let operand = self.parse_expr_bp(24)?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Unary {
                        op: UnaryOp::Not,
                        operand: Box::new(operand),
                    },
                })
            }
            Token::Tilde => {
                self.advance();
                let operand = self.parse_expr_bp(24)?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Unary {
                        op: UnaryOp::BitNot,
                        operand: Box::new(operand),
                    },
                })
            }
            Token::Star => {
                self.advance();
                let operand = self.parse_expr_bp(24)?; // same precedence as other unary ops
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Unary {
                        op: UnaryOp::Deref,
                        operand: Box::new(operand),
                    },
                })
            }
            // Half-open ranges in prefix (start-less) position:
            //   `..expr`  → RangeTo[T]
            //   `..=expr` → RangeToInclusive[T]
            //   `..`      → RangeFull (when nothing follows)
            Token::DotDotEq => {
                self.advance();
                let end = self.parse_expr_bp(6)?; // range_bp + 1
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Range {
                        start: None,
                        end: Some(Box::new(end)),
                        inclusive: true,
                    },
                })
            }
            Token::DotDot => {
                self.advance();
                let has_rhs = !self.is_at_end()
                    && !matches!(
                        self.peek_token(),
                        Token::Semicolon
                            | Token::RightBrace
                            | Token::Comma
                            | Token::RightBracket
                            | Token::RightParen
                    );
                let end = if has_rhs {
                    Some(Box::new(self.parse_expr_bp(6)?))
                } else {
                    None
                };
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Range {
                        start: None,
                        end,
                        inclusive: false,
                    },
                })
            }

            // Literals
            Token::Integer(n, sfx) => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Integer(n, sfx),
                })
            }
            Token::Float(n, sfx) => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Float(n, sfx),
                })
            }
            Token::CharLiteral(c) => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::CharLit(c),
                })
            }
            Token::StringLiteral(s) => {
                let s = s.clone();
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::StringLit(s),
                })
            }
            Token::MultiStringLiteral(s) => {
                let s = s.clone();
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::MultiStringLit(s),
                })
            }
            Token::InterpolatedStringLiteral(raw_parts) => {
                let raw_parts = raw_parts.clone();
                self.advance();
                let mut parsed_parts = Vec::with_capacity(raw_parts.len());
                for part in raw_parts {
                    match part {
                        crate::token::InterpolationPart::Text(t) => {
                            parsed_parts.push(crate::ast::ParsedInterpolationPart::Text(t));
                        }
                        crate::token::InterpolationPart::Expr(raw) => {
                            let wrapper = format!("fn __interp__() {{ {}; }}", raw);
                            let result = crate::parse(&wrapper);
                            let expr = result.program.items.into_iter().find_map(|item| {
                                if let crate::ast::Item::Function(f) = item {
                                    f.body.stmts.into_iter().find_map(|s| {
                                        if let crate::ast::StmtKind::Expr(e) = s.kind {
                                            Some(e)
                                        } else {
                                            None
                                        }
                                    })
                                } else {
                                    None
                                }
                            });
                            if let Some(e) = expr {
                                parsed_parts
                                    .push(crate::ast::ParsedInterpolationPart::Expr(Box::new(e)));
                            } else {
                                parsed_parts.push(crate::ast::ParsedInterpolationPart::Text(
                                    format!("{{{}}}", raw),
                                ));
                            }
                        }
                    }
                }
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::InterpolatedStringLit(parsed_parts),
                })
            }
            Token::True => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Bool(true),
                })
            }
            Token::False => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Bool(false),
                })
            }

            // self / Self
            Token::SelfValue => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::SelfValue,
                })
            }
            Token::SelfType => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::SelfType,
                })
            }

            // Block expression
            Token::LeftBrace => {
                let block = self.parse_block()?;
                Some(Expr {
                    span: block.span.clone(),
                    kind: ExprKind::Block(block),
                })
            }

            // If expression
            Token::If => self.parse_if_expr(),

            // Match expression
            Token::Match => self.parse_match_expr(),

            // While expression
            Token::While => self.parse_while_expr(),

            // For expression
            Token::For => self.parse_for_expr(),

            // Loop expression
            Token::Loop => {
                self.advance();
                let body = self.parse_block()?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Loop { label: None, body },
                })
            }

            // Return
            Token::Return => {
                self.advance();
                let value = if !self.check(&Token::Semicolon)
                    && !self.check(&Token::RightBrace)
                    && !self.check(&Token::Comma)
                {
                    Some(Box::new(self.parse_expression()?))
                } else {
                    None
                };
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Return(value),
                })
            }

            // Break
            Token::Break => {
                self.advance();
                // break label [expr] | break [expr]
                let (label, value) = self.parse_break_args();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Break { label, value },
                })
            }

            // Continue
            Token::Continue => {
                self.advance();
                // continue label | continue
                let label = self.parse_continue_label();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Continue { label },
                })
            }

            // Unsafe block
            Token::Unsafe => {
                self.advance();
                let block = self.parse_block()?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Unsafe(block),
                })
            }

            // Try block (`try { ... }`) — v60 item 42 / design.md § Error
            // Handling > Try Blocks. v1 parses the form; the typechecker
            // pipeline (`?`-retargeting against the block, error-type
            // unification, From-chain coercion) lands in P1.
            Token::Try => {
                self.advance();
                let block = self.parse_block()?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Try(block),
                })
            }

            // Seq block
            Token::Seq => {
                self.advance();
                let block = self.parse_block()?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Seq(block),
                })
            }

            // Par block
            Token::Par => {
                self.advance();
                let block = self.parse_block()?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Par(block),
                })
            }

            // Lock block
            Token::Lock => {
                self.advance();
                let mutex = self.expect_identifier()?;
                let alias = if !self.check(&Token::LeftBrace) {
                    Some(self.expect_identifier()?)
                } else {
                    None
                };
                let body = self.parse_block()?;
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Lock { mutex, alias, body },
                })
            }

            // `providers { R => e, ... } in { body }` is parsed as a
            // contextual keyword: see `parse_identifier_expr`'s
            // name == "providers" dispatch. The lexer no longer reserves
            // `providers` as a token, so module names / function names /
            // variable bindings can use the bareword freely.
            //
            // Pipe placeholder: _ in expression position (for use in pipe argument lists)
            Token::Underscore => {
                self.advance();
                Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::PipePlaceholder,
                })
            }

            // Closure: |params| expr — bare form runs per-capture-path
            // inference (Rule 2½). The three explicit prefixes (`own` / `ref`
            // / `mut ref`) pin every captured path to the declared mode.
            Token::Pipe | Token::PipePipe => self.parse_closure(None, None),

            // `own |...|` — explicit capture-by-value prefix (Rule 2½). Same
            // lookahead rule as the borrow prefixes: only consume `own` when a
            // closure follows, since `own` may appear elsewhere in v2+.
            Token::Own if matches!(self.peek_token_at(1), Token::Pipe | Token::PipePipe) => {
                let prefix_start = self.current_span();
                self.advance();
                let prefix_span = self.span_from(&prefix_start);
                self.parse_closure(Some(CaptureMode::Own), Some(prefix_span))
            }

            // `ref |...|` — explicit borrow-mode prefix (Rule 2½). The prefix
            // is closure-only; in any non-closure position the `ref` keyword
            // retains its existing meaning and is not consumed here. We peek
            // for a following `|` / `||` before committing.
            Token::Ref if matches!(self.peek_token_at(1), Token::Pipe | Token::PipePipe) => {
                let prefix_start = self.current_span();
                self.advance();
                let prefix_span = self.span_from(&prefix_start);
                self.parse_closure(Some(CaptureMode::Ref), Some(prefix_span))
            }

            // `mut ref |...|` — explicit mutable-borrow prefix (Rule 2½).
            // Same lookahead rule as `ref`: only consume the `mut` token when
            // the full `mut ref |` / `mut ref ||` shape is visible.
            Token::Mut
                if matches!(self.peek_token_at(1), Token::Ref)
                    && matches!(self.peek_token_at(2), Token::Pipe | Token::PipePipe) =>
            {
                let prefix_start = self.current_span();
                self.advance(); // mut
                self.advance(); // ref
                let prefix_span = self.span_from(&prefix_start);
                self.parse_closure(Some(CaptureMode::MutRef), Some(prefix_span))
            }

            // `move |...|` is reserved against Rust's idiom but not active in
            // Kāra — the same role is played by `own |...|`. Emit a focused
            // redirect diagnostic and parse the rest as `own` for recovery.
            Token::Move if matches!(self.peek_token_at(1), Token::Pipe | Token::PipePipe) => {
                self.error("the `move` keyword is not used in Kāra; use `own |...|` for closure capture-by-value");
                let prefix_start = self.current_span();
                self.advance();
                let prefix_span = self.span_from(&prefix_start);
                self.parse_closure(Some(CaptureMode::Own), Some(prefix_span))
            }

            // Parenthesized expression or tuple
            Token::LeftParen => self.parse_paren_or_tuple(),

            // Array literal: [expr, expr, ...]
            Token::LeftBracket => self.parse_array_literal(),

            // Identifier (possibly path, struct literal, or function call)
            Token::Identifier { .. } => self.parse_identifier_expr(),

            _ => {
                self.error(&format!(
                    "Expected expression, found {:?}",
                    self.peek_token()
                ));
                None
            }
        }
    }

    fn parse_if_expr(&mut self) -> Option<Expr> {
        let start = self.current_span();
        self.expect(&Token::If)?;

        // if let pattern = value { ... }
        if self.check(&Token::Let) {
            self.advance();
            let pattern = self.parse_pattern()?;
            self.expect(&Token::Equal)?;
            let value = self.parse_expression()?;
            let then_block = self.parse_block()?;
            let else_branch = if self.eat(&Token::Else) {
                if self.check(&Token::If) {
                    Some(Box::new(self.parse_if_expr()?))
                } else {
                    let block = self.parse_block()?;
                    Some(Box::new(Expr {
                        span: block.span.clone(),
                        kind: ExprKind::Block(block),
                    }))
                }
            } else {
                None
            };
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::IfLet {
                    pattern,
                    value: Box::new(value),
                    then_block,
                    else_branch,
                },
            });
        }

        let condition = self.parse_expression()?;
        let then_block = self.parse_block()?;
        let else_branch = if self.eat(&Token::Else) {
            if self.check(&Token::If) {
                Some(Box::new(self.parse_if_expr()?))
            } else {
                let block = self.parse_block()?;
                Some(Box::new(Expr {
                    span: block.span.clone(),
                    kind: ExprKind::Block(block),
                }))
            }
        } else {
            None
        };
        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::If {
                condition: Box::new(condition),
                then_block,
                else_branch,
            },
        })
    }

    fn parse_match_expr(&mut self) -> Option<Expr> {
        let start = self.current_span();
        self.expect(&Token::Match)?;
        let scrutinee = self.parse_expression()?;
        self.expect(&Token::LeftBrace)?;

        let mut arms = Vec::new();
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            let arm_start = self.current_span();
            let pattern = self.parse_pattern()?;

            let guard = if self.eat(&Token::If) {
                Some(self.parse_expression()?)
            } else {
                None
            };

            self.expect(&Token::FatArrow)?;

            let body_is_block = self.check(&Token::LeftBrace);
            let body = if body_is_block {
                let block = self.parse_block()?;
                Expr {
                    span: block.span.clone(),
                    kind: ExprKind::Block(block),
                }
            } else {
                self.parse_expression()?
            };

            arms.push(MatchArm {
                pattern,
                guard,
                body,
                span: self.span_from(&arm_start),
            });

            // Comma is required after non-block arm bodies, optional after
            // block-bodied arms (mirrors Rust's match arm grammar).
            if !self.eat(&Token::Comma) && !body_is_block {
                break;
            }
        }
        self.expect(&Token::RightBrace)?;

        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
        })
    }

    fn parse_while_expr(&mut self) -> Option<Expr> {
        self.parse_while_expr_with_label(None)
    }

    fn parse_while_expr_with_label(&mut self, label: Option<String>) -> Option<Expr> {
        let start = self.current_span();
        self.expect(&Token::While)?;

        if let Some(ref l) = label {
            self.loop_labels.push((l.clone(), LabelKind::Loop));
        }

        // while let pattern = expr { ... }
        if self.check(&Token::Let) {
            self.advance();
            let pattern = self.parse_pattern()?;
            self.expect(&Token::Equal)?;
            let value = self.parse_expression()?;
            let body = self.parse_block()?;
            if label.is_some() {
                self.loop_labels.pop();
            }
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::WhileLet {
                    label,
                    pattern,
                    value: Box::new(value),
                    body,
                },
            });
        }

        let condition = self.parse_expression()?;
        let body = self.parse_block()?;
        if label.is_some() {
            self.loop_labels.pop();
        }
        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::While {
                label,
                condition: Box::new(condition),
                body,
            },
        })
    }

    fn parse_for_expr(&mut self) -> Option<Expr> {
        self.parse_for_expr_with_label(None)
    }

    fn parse_for_expr_with_label(&mut self, label: Option<String>) -> Option<Expr> {
        let start = self.current_span();
        self.expect(&Token::For)?;
        if let Some(ref l) = label {
            self.loop_labels.push((l.clone(), LabelKind::Loop));
        }
        let pattern = self.parse_pattern()?;
        self.expect(&Token::In)?;
        let iterable = self.parse_expression()?;
        let body = self.parse_block()?;
        if label.is_some() {
            self.loop_labels.pop();
        }
        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::For {
                label,
                pattern,
                iterable: Box::new(iterable),
                body,
            },
        })
    }

    fn parse_closure(
        &mut self,
        capture_mode: Option<CaptureMode>,
        prefix_span: Option<Span>,
    ) -> Option<Expr> {
        let start = self.current_span();
        let mut params = Vec::new();

        if self.eat(&Token::PipePipe) {
            // || — no params
        } else {
            self.expect(&Token::Pipe)?;
            while !self.check(&Token::Pipe) && !self.is_at_end() {
                let pstart = self.current_span();
                let pattern = self.parse_param_pattern()?;
                let ty = if self.eat(&Token::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push(ClosureParam {
                    pattern,
                    ty,
                    span: self.span_from(&pstart),
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::Pipe)?;
        }

        let body = if self.check(&Token::LeftBrace) {
            let block = self.parse_block()?;
            Expr {
                span: block.span.clone(),
                kind: ExprKind::Block(block),
            }
        } else {
            self.parse_expression()?
        };

        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::Closure {
                params,
                capture_mode,
                prefix_span,
                body: Box::new(body),
            },
        })
    }

    fn parse_paren_or_tuple(&mut self) -> Option<Expr> {
        let start = self.current_span();
        self.advance(); // consume (

        // () — unit value (empty tuple)
        if self.check(&Token::RightParen) {
            self.advance();
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::Tuple(Vec::new()),
            });
        }

        let first = self.parse_expression()?;

        if self.eat(&Token::Comma) {
            // Tuple
            let mut elements = vec![first];
            while !self.check(&Token::RightParen) && !self.is_at_end() {
                elements.push(self.parse_expression()?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RightParen)?;
            Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::Tuple(elements),
            })
        } else {
            // Parenthesized expression
            self.expect(&Token::RightParen)?;
            Some(first)
        }
    }

    fn parse_array_literal(&mut self) -> Option<Expr> {
        let start = self.current_span();
        self.advance(); // consume [

        // [] — empty array
        if self.check(&Token::RightBracket) {
            self.advance();
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::ArrayLiteral(Vec::new()),
            });
        }

        let first = self.parse_expression()?;

        // [key: val, ...] — map literal (disambiguate from array by colon after first expr)
        if self.eat(&Token::Colon) {
            let first_val = self.parse_expression()?;
            let mut entries = vec![(first, first_val)];
            while self.eat(&Token::Comma) {
                if self.check(&Token::RightBracket) {
                    break; // trailing comma
                }
                let key = self.parse_expression()?;
                self.expect(&Token::Colon)?;
                let val = self.parse_expression()?;
                entries.push((key, val));
            }
            self.expect(&Token::RightBracket)?;
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::MapLiteral(entries),
            });
        }

        // [value; count] — repeat literal.
        if self.eat(&Token::Semicolon) {
            let count = self.parse_expression()?;
            self.expect(&Token::RightBracket)?;
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::RepeatLiteral {
                    type_name: None,
                    value: Box::new(first),
                    count: Box::new(count),
                },
            });
        }

        // [expr, ...] — array literal
        let mut elements = vec![first];
        while self.eat(&Token::Comma) {
            if self.check(&Token::RightBracket) {
                break; // trailing comma
            }
            elements.push(self.parse_expression()?);
        }
        self.expect(&Token::RightBracket)?;
        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::ArrayLiteral(elements),
        })
    }

    fn parse_identifier_expr(&mut self) -> Option<Expr> {
        let start = self.current_span();
        let name = self.expect_identifier()?;

        // Contextual keyword: `providers { R => e, ... } in { body }`.
        // The lexer emits `providers` as a regular identifier (so module
        // names / functions / variable bindings can use the bareword);
        // the parser dispatches on the `Identifier("providers")` + `{`
        // shape. Unambiguous against other identifier-then-`{` forms in
        // expression position because struct literals require uppercase
        // leading and there's no other identifier-followed-by-block
        // shape today. See design.md § `providers { } in { }` Block.
        if name == "providers" && self.check(&Token::LeftBrace) {
            return self.parse_providers_block(start);
        }

        // Contextual keyword: `offset_of[T](field.path)` — compile-time
        // byte offset of a field from a value of type `T`. The bracket
        // contents are a `TypeExpr` (not an `Expr`), and the paren
        // contents are an identifier-only field path (not value
        // arguments) per design.md § Field Offsets. Recognised here at
        // the bareword + `[` shape so the call site lowers as a single
        // `ExprKind::OffsetOf` AST node, bypassing the regular call
        // routing entirely.
        if name == "offset_of" && self.check(&Token::LeftBracket) {
            return self.parse_offset_of_special_form(start);
        }

        // Check for labeled loop / labeled block: `label: while/for/loop`
        // or `label: { ... }`. `is_loop_label` accepts both shapes.
        if self.check(&Token::Colon) && self.is_loop_label() {
            self.advance(); // consume ':'
            match self.peek_token() {
                Token::While => return self.parse_while_expr_with_label(Some(name)),
                Token::For => return self.parse_for_expr_with_label(Some(name)),
                Token::Loop => {
                    self.advance();
                    self.loop_labels.push((name.clone(), LabelKind::Loop));
                    let body = self.parse_block()?;
                    self.loop_labels.pop();
                    return Some(Expr {
                        span: self.span_from(&start),
                        kind: ExprKind::Loop {
                            label: Some(name),
                            body,
                        },
                    });
                }
                Token::LeftBrace => {
                    // Labeled block: `label: { ... }`. Use the label
                    // identifier's span (`start`) for diagnostic span fidelity
                    // (LB hard-stop default fallback: label_span on
                    // LabeledBlock only; loop-side parity is v1.x polish).
                    let label_span = start.clone();
                    self.loop_labels.push((name.clone(), LabelKind::Block));
                    let body = self.parse_block()?;
                    self.loop_labels.pop();
                    return Some(Expr {
                        span: self.span_from(&start),
                        kind: ExprKind::LabeledBlock {
                            label: name,
                            label_span,
                            body,
                        },
                    });
                }
                _ => unreachable!(),
            }
        }

        // Check for path: Name.Name2.... Type/Const-class idents (uppercase leading)
        // root a path here; Value-class idents fall through to the postfix loop,
        // which handles `.` as field/method access.
        if self.check(&Token::Dot) && starts_upper(&name) {
            let mut path = vec![name];
            while self.eat(&Token::Dot) {
                match self.peek_token() {
                    Token::Identifier { .. } => {
                        path.push(self.expect_identifier()?);
                    }
                    _ => break,
                }
            }

            // Check for struct literal: Path { field: value }
            if self.check(&Token::LeftBrace) && self.looks_like_struct_literal() {
                return self.parse_struct_literal_body(path, &start);
            }

            // Check for function call: Path(args)
            if self.check(&Token::LeftParen) {
                self.advance();
                let args = self.parse_arg_list()?;
                self.expect(&Token::RightParen)?;
                return Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::Call {
                        callee: Box::new(Expr {
                            span: self.span_from(&start),
                            kind: ExprKind::Path {
                                segments: path,
                                generic_args: None,
                            },
                        }),
                        args,
                    },
                });
            }

            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::Path {
                    segments: path,
                    generic_args: None,
                },
            });
        }

        // Check for struct literal: `Name { field: value }`. Only valid
        // when the leading identifier is Type-class — `n { ..0 => x }` in
        // a `match n { ... }` arm-list start would otherwise misparse as
        // a struct update of a struct named `n` (looks_like_struct_literal
        // matches `{ .. }` for the struct-update form). Value-class names
        // never form struct literals, so the `{` here belongs to whatever
        // wraps this expression (a match scrutinee's arm list, a loop's
        // body, etc.).
        if starts_upper(&name) && self.check(&Token::LeftBrace) && self.looks_like_struct_literal()
        {
            return self.parse_struct_literal_body(vec![name], &start);
        }

        // Concrete-type UFCS: `TypeName[Type, ...].method(...)`. When an
        // uppercase identifier is followed by `[…]` and immediately by
        // `.identifier(`, the `[…]` carries generic type arguments rather
        // than a collection literal. Emit `Path { segments, generic_args }`
        // so the postfix `.method(...)` chain produces a `MethodCall` whose
        // object resolves through `find_method_with_args`.
        if starts_upper(&name)
            && self.check(&Token::LeftBracket)
            && self.lookahead_concrete_type_ufcs()
        {
            // Reuse the same routing as `parse_generic_type_args` so the
            // UFCS path accepts mixed type / const args (const generics
            // slice 1b — call-site const-arg binding).
            let args = self.parse_generic_type_args()?;
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::Path {
                    segments: vec![name],
                    generic_args: Some(args),
                },
            });
        }

        // Prefix collection literal: `Vec[e1, e2, ...]` / `Array[e1, e2, ...]`
        // / `Vec[v; n]` / `Array[v; n]`. Intercept before the postfix `[` index
        // loop so the bracket is consumed as part of the literal, not as a
        // subscript.
        if self.check(&Token::LeftBracket)
            && matches!(name.as_str(), "Vec" | "Array" | "Set" | "Map")
        {
            self.advance(); // consume [
            let mut items = Vec::new();
            // Empty literal: `Vec[]` etc.
            if self.check(&Token::RightBracket) {
                self.advance();
                return Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::PrefixCollectionLiteral {
                        type_name: name,
                        items,
                    },
                });
            }
            let first = self.parse_expression()?;
            // `Vec[v; n]` / `Array[v; n]` — repeat form. Only meaningful for
            // sequence types; the typechecker rejects `Set[v; n]` / `Map[v; n]`.
            if self.eat(&Token::Semicolon) {
                let count = self.parse_expression()?;
                self.expect(&Token::RightBracket)?;
                return Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::RepeatLiteral {
                        type_name: Some(name),
                        value: Box::new(first),
                        count: Box::new(count),
                    },
                });
            }
            // `Map[k: v, k2: v2, ...]` — prefix-literal map form. The first
            // expression is a key, followed by `:`, then the value. Switch to
            // key-value parsing and emit `MapLiteral` (the same AST shape the
            // bare `["k": v]` form produces, so the existing typechecker case
            // applies unchanged).
            if name == "Map" && self.eat(&Token::Colon) {
                let first_val = self.parse_expression()?;
                let mut entries = vec![(first, first_val)];
                while self.eat(&Token::Comma) {
                    if self.check(&Token::RightBracket) {
                        break;
                    }
                    let k = self.parse_expression()?;
                    self.expect(&Token::Colon)?;
                    let v = self.parse_expression()?;
                    entries.push((k, v));
                }
                self.expect(&Token::RightBracket)?;
                return Some(Expr {
                    span: self.span_from(&start),
                    kind: ExprKind::MapLiteral(entries),
                });
            }
            items.push(first);
            while self.eat(&Token::Comma) {
                if self.check(&Token::RightBracket) {
                    break;
                }
                items.push(self.parse_expression()?);
            }
            self.expect(&Token::RightBracket)?;
            return Some(Expr {
                span: self.span_from(&start),
                kind: ExprKind::PrefixCollectionLiteral {
                    type_name: name,
                    items,
                },
            });
        }

        Some(Expr {
            span: self.span_from(&start),
            kind: ExprKind::Identifier(name),
        })
    }

    /// Concrete-type UFCS lookahead — `self.pos` must point at the `[`
    /// following an uppercase identifier. Returns `true` when the bracket
    /// pair encloses a type-shaped expression (first inner token starts a
    /// type, per [`Self::starts_type`]) AND is immediately followed by
    /// `.identifier(` — i.e., the whole prefix forms `TypeName[…].method(`.
    /// Balanced-bracket scan handles nested generics like `Vec[Map[K, V]]`.
    /// On any other shape (collection literal, value-shaped subscript,
    /// trailing `.field` field-access without parens) returns `false` so
    /// the caller falls through to the existing parse paths.
    /// Generic-args-call lookahead (const generics slice 1b). Triggers
    /// at a postfix `[` when `lhs` is a bare identifier or path-without-
    /// generic-args, the bracket contents contain a `,` separator
    /// (disambiguating from `arr[i]` indexing), and the matching `]` is
    /// immediately followed by `(`. The bracket contents then parse as
    /// `Vec<GenericArg>` via the shared `parse_generic_type_args` so the
    /// call site carries mixed type / const args.
    fn lookahead_generic_args_call(&self, lhs: &Expr) -> bool {
        if !matches!(
            &lhs.kind,
            ExprKind::Identifier(_)
                | ExprKind::Path {
                    generic_args: None,
                    ..
                }
        ) {
            return false;
        }
        // Balanced bracket scan from the opening `[` at self.pos.
        let mut depth: usize = 0;
        let mut saw_top_comma = false;
        let mut i = self.pos;
        while i < self.tokens.len() {
            match &self.tokens[i].token {
                Token::LeftBracket => depth += 1,
                Token::Comma if depth == 1 => saw_top_comma = true,
                Token::RightBracket => {
                    depth -= 1;
                    if depth == 0 {
                        let next = self.tokens.get(i + 1).map(|t| &t.token);
                        return saw_top_comma && matches!(next, Some(Token::LeftParen));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn lookahead_concrete_type_ufcs(&self) -> bool {
        // Require at least one token inside `[…]` and check it is a
        // type-start; rejects `Vec[1, 2].push(3)` (integer literal start)
        // before any bracket scan.
        let inner_start = self.pos + 1;
        if inner_start >= self.tokens.len() {
            return false;
        }
        if !Self::starts_type(&self.tokens[inner_start].token) {
            return false;
        }
        // Balanced-bracket scan from the opening `[`.
        let mut depth: usize = 0;
        let mut i = self.pos;
        while i < self.tokens.len() {
            match &self.tokens[i].token {
                Token::LeftBracket => depth += 1,
                Token::RightBracket => {
                    depth -= 1;
                    if depth == 0 {
                        // After matching `]`, expect `.IDENT (` for UFCS
                        // dispatch.
                        let a = i + 1;
                        let b = i + 2;
                        let c = i + 3;
                        if c >= self.tokens.len() {
                            return false;
                        }
                        return self.tokens[a].token == Token::Dot
                            && matches!(self.tokens[b].token, Token::Identifier { .. })
                            && self.tokens[c].token == Token::LeftParen;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// First-token heuristic for "looks like the start of a type expression"
    /// — used by [`Self::lookahead_concrete_type_ufcs`] to reject value-
    /// shaped collection contents (integer literals, string literals, etc.)
    /// before committing to the UFCS branch.
    fn starts_type(tok: &Token) -> bool {
        matches!(
            tok,
            Token::Identifier { .. }
                | Token::SelfType
                | Token::Ref
                | Token::Mut
                | Token::Weak
                | Token::Star
                | Token::LeftParen
        )
    }

    fn looks_like_struct_literal(&self) -> bool {
        // Heuristic to disambiguate `Name { ... }` (struct literal) from
        // `expr { ... }` (expression followed by block).
        //
        // `{ ident :` — struct literal (explicit field)
        // `{ ident ,` — shorthand struct literal (at least 2 fields)
        // `{ }` — empty struct literal
        // `{ ..` — struct update
        //
        // NOTE: `{ ident }` is intentionally NOT matched as struct literal
        // because it's ambiguous with a block containing a single expression
        // (e.g., `if a > b { x }` would misparse `b { x }` as a struct).
        if self.pos + 2 >= self.tokens.len() {
            return false;
        }
        let next = &self.tokens[self.pos + 1].token;
        let after = &self.tokens[self.pos + 2].token;
        matches!(
            (next, after),
            (Token::Identifier { .. }, Token::Colon)
                | (Token::Identifier { .. }, Token::Comma)
                | (Token::RightBrace, _)
                | (Token::DotDot, _)
        )
    }

    fn parse_struct_literal_body(&mut self, path: Vec<String>, start: &Span) -> Option<Expr> {
        self.expect(&Token::LeftBrace)?;
        let mut fields = Vec::new();
        let mut spread = None;
        while !self.check(&Token::RightBrace) && !self.is_at_end() {
            // Check for struct update: ..expr
            if self.eat(&Token::DotDot) {
                spread = Some(Box::new(self.parse_expression()?));
                // Spread must be last
                self.eat(&Token::Comma); // optional trailing comma
                break;
            }
            let fs = self.current_span();
            let name = self.expect_identifier()?;
            if self.eat(&Token::Colon) {
                // Explicit field: name: value
                let value = self.parse_expression()?;
                fields.push(FieldInit {
                    name,
                    value,
                    shorthand: false,
                    span: self.span_from(&fs),
                });
            } else {
                // Shorthand field: `Point { x }` — name is also the value
                let span = self.span_from(&fs);
                fields.push(FieldInit {
                    value: Expr {
                        kind: ExprKind::Identifier(name.clone()),
                        span: span.clone(),
                    },
                    name,
                    shorthand: true,
                    span,
                });
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RightBrace)?;
        Some(Expr {
            span: self.span_from(start),
            kind: ExprKind::StructLiteral {
                path,
                fields,
                spread,
            },
        })
    }

    fn parse_arg_list(&mut self) -> Option<Vec<CallArg>> {
        let mut args = Vec::new();
        while !self.check(&Token::RightParen) && !self.is_at_end() {
            let arg_start = self.current_span();
            // Lookahead: Identifier followed by Colon => labeled argument
            let label = if self.is_labeled_arg() {
                let name = self.expect_identifier()?;
                self.expect(&Token::Colon)?;
                Some(name)
            } else {
                None
            };

            // Reject `ref <expr>` at call sites (design.md Part 1½ Rule 4).
            // The keyword `ref` is never legal in argument position; the
            // parameter's mode is declared on the callee's signature.
            if self.check(&Token::Ref) {
                self.error(
                    "`ref` is not written at call sites. \
                     The parameter's mode is declared on the callee's signature. \
                     Remove the `ref` keyword.",
                );
                self.advance(); // consume `ref` and parse the rest of the arg
            }

            // Optional call-site `mut` marker. Disambiguate from `mut ref`
            // (also rejected) and from `mut <ident> = ...` (never valid in
            // argument position, but parse_expression will handle it).
            let mut_marker = if self.check(&Token::Mut) {
                // Look ahead: if the next token is `ref`, the user wrote
                // `mut ref <expr>` — reject with a suggesting diagnostic.
                if self.pos + 1 < self.tokens.len()
                    && matches!(self.tokens[self.pos + 1].token, Token::Ref)
                {
                    self.error(
                        "`mut ref` is not written at call sites. \
                         Use `mut <expr>` instead — the `ref` is implied by \
                         the callee's signature.",
                    );
                    self.advance(); // consume `mut`
                    self.advance(); // consume `ref`
                    true
                } else {
                    self.advance(); // consume `mut`
                    true
                }
            } else {
                false
            };

            let value = self.parse_expression()?;
            args.push(CallArg {
                label,
                mut_marker,
                value,
                span: self.span_from(&arg_start),
            });
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        Some(args)
    }

    /// Check if the current position looks like a labeled argument: `identifier:`
    fn is_labeled_arg(&self) -> bool {
        if self.pos + 1 >= self.tokens.len() {
            return false;
        }
        matches!(&self.tokens[self.pos].token, Token::Identifier { .. })
            && matches!(&self.tokens[self.pos + 1].token, Token::Colon)
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

    // ── Label Helpers ─────────────────────────────────────────────

    /// Check if current position is `ident:` followed by a loop keyword
    /// (labeled loop) or `{` (labeled block — design.md § Loops > "Labeled
    /// blocks", syntax.md §5.3). Both forms share the `IDENT ":"` prefix
    /// and route through `parse_identifier_expr` to the appropriate sub-parser.
    fn is_loop_label(&self) -> bool {
        if self.pos + 1 >= self.tokens.len() {
            return false;
        }
        // Current token must be `:`, next must be while/for/loop or `{`.
        matches!(&self.tokens[self.pos].token, Token::Colon)
            && matches!(
                &self.tokens[self.pos + 1].token,
                Token::While | Token::For | Token::Loop | Token::LeftBrace
            )
    }

    /// Parse break arguments: `break [label] [expr]`
    fn parse_break_args(&mut self) -> (Option<String>, Option<Box<Expr>>) {
        if self.check(&Token::Semicolon)
            || self.check(&Token::RightBrace)
            || self.check(&Token::Comma)
        {
            return (None, None);
        }
        if let Token::Identifier { ref name, .. } = self.peek_token() {
            let name = name.clone();
            let is_known_label = self.loop_labels.iter().any(|(n, _)| n == &name);
            if self.pos + 1 < self.tokens.len() {
                let after = &self.tokens[self.pos + 1].token;
                if is_known_label
                    && matches!(after, Token::Semicolon | Token::RightBrace | Token::Comma)
                {
                    // `break label;` / `break label,` — known loop label, no value
                    self.advance();
                    return (Some(name), None);
                }
                // `break label expr` — identifier NOT followed by ; , or }
                // means label + value (only if it's a known label)
                if is_known_label
                    && !matches!(after, Token::Semicolon | Token::RightBrace | Token::Comma)
                {
                    self.advance();
                    let value = self.parse_expression().map(Box::new);
                    return (Some(name), value);
                }
            }
        }
        // Parse as value expression (covers `break expr;` and `break ident;`)
        let value = self.parse_expression().map(Box::new);
        (None, value)
    }

    /// Parse continue label: `continue [label]`
    fn parse_continue_label(&mut self) -> Option<String> {
        if self.check(&Token::Semicolon) || self.check(&Token::RightBrace) {
            return None;
        }
        if let Token::Identifier { name, .. } = self.peek_token() {
            self.advance();
            Some(name)
        } else {
            None
        }
    }

    // ── Compound Assignment Helper ────────────────────────────────

    fn try_compound_op(&mut self) -> Option<CompoundOp> {
        let op = match self.peek_token() {
            Token::PlusEqual => CompoundOp::Add,
            Token::MinusEqual => CompoundOp::Sub,
            Token::StarEqual => CompoundOp::Mul,
            Token::SlashEqual => CompoundOp::Div,
            Token::PercentEqual => CompoundOp::Mod,
            Token::AmpEqual => CompoundOp::BitAnd,
            Token::PipeEqual => CompoundOp::BitOr,
            Token::CaretEqual => CompoundOp::BitXor,
            Token::LessLessEqual => CompoundOp::Shl,
            Token::GreaterGreaterEqual => CompoundOp::Shr,
            _ => return None,
        };
        self.advance();
        Some(op)
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
fn render_type_for_diagnostic(ty: &TypeExpr) -> String {
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
