//! Trait and impl-block parsing — marker traits, regular trait
//! definitions, trait aliases, trait methods, impl blocks, and
//! associated-type declarations / bindings.

use crate::ast::*;
use crate::lexer::IdentClass;
use crate::token::{Span, Token};

use super::{FnContext, ParseError};

impl super::Parser {
    // ── Traits ───────────────────────────────────────────────────

    /// Parse `marker trait NAME[GENERICS] [: SUPERTRAITS] [where ...]
    /// (";" | "{" "}")` per syntax.md §3.4 / design.md § Marker Traits.
    /// The body must be empty — methods, associated types, and
    /// associated consts inside the body are rejected with a focused
    /// diagnostic. `body_brace` records whether the user wrote `{ }`
    /// (so the formatter can round-trip) or the canonical `;`.
    pub(super) fn parse_marker_trait(
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

        let deprecation = self.scan_deprecated_attr(&attributes);
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
            deprecation,
        })
    }

    /// Top-level dispatch for the `trait` keyword. Reads the trait header
    /// (name + optional generic params), then peeks the next token: `=`
    /// enters the trait-alias path (`trait NAME = bounds;` per v60 item
    /// 40 / design.md § Trait Aliases); anything else falls through to
    /// the regular trait-def path.
    pub(super) fn parse_trait_or_alias(
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
                items.push(TraitItem::AssocType(Box::new(item)));
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

        let deprecation = self.scan_deprecated_attr(&attributes);
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
            deprecation,
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

        let deprecation = self.scan_deprecated_attr(&attributes);
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
            deprecation,
        })
    }

    fn parse_assoc_type_decl(&mut self) -> Option<AssocTypeDecl> {
        let start = self.current_span();
        self.expect(&Token::Type)?;
        let name = self.expect_identifier()?;
        let name_span = self.span_from(&start);
        self.check_ident_class(&name, IdentClass::Type, "associated type", name_span);
        // GAT-shaped declaration `type Name[P1, P2, ...]` — optional
        // generic parameter list. Per design.md § Generic associated
        // types, effect-polymorphic GATs (`type Mapped[U, with E]`) are
        // out of v1 scope; the carrying method takes the `with E`
        // parameter instead. Reject post-parse so the parameter list
        // itself parses (preserving the rest of the trait body).
        let generic_params = self.parse_optional_generic_params();
        self.reject_gat_effect_params(generic_params.as_ref(), "declaration");
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
        let where_clause = self.parse_optional_where_clause();
        self.expect(&Token::Semicolon)?;
        Some(AssocTypeDecl {
            span: self.span_from(&start),
            name,
            generic_params,
            bounds,
            where_clause,
        })
    }

    /// Reject effect-parameter clauses on a GAT's generic-parameter
    /// list. The carrying *method* takes the `with E` parameter, not
    /// the associated type itself — see design.md § GATs "Out of v1
    /// scope" bullet. Emits one diagnostic per offending effect
    /// parameter, anchored at the generic-params span (the parser
    /// doesn't retain per-effect-param spans). Suggestion text steers
    /// the author at the carrying-method form.
    fn reject_gat_effect_params(&mut self, gp: Option<&GenericParams>, role: &str) {
        let Some(gp) = gp else { return };
        if gp.effect_params.is_empty() {
            return;
        }
        let joined = gp.effect_params.join(", ");
        self.errors.push(ParseError {
            message: format!(
                "error[E_GAT_EFFECT_PARAM]: effect parameter `with {joined}` is not \
                 permitted on a generic associated type {role}; GATs are over types \
                 only — move the `with {joined}` clause to the carrying method's \
                 generic-parameter list instead",
            ),
            span: gp.span.clone(),
        });
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

    pub(super) fn parse_impl_block(&mut self, attributes: Vec<Attribute>) -> Option<ImplBlock> {
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
                items.push(ImplItem::AssocType(Box::new(binding)));
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
        // GAT binding `type Name[P1, P2, ...] = TypeExpr` mirrors the
        // declaration's parameter-list shape. Same effect-polymorphism
        // rejection rule as the trait-side declaration.
        let generic_params = self.parse_optional_generic_params();
        self.reject_gat_effect_params(generic_params.as_ref(), "binding");
        self.expect(&Token::Equal)?;
        let ty = self.parse_type()?;
        let where_clause = self.parse_optional_where_clause();
        self.expect(&Token::Semicolon)?;
        Some(AssocTypeBinding {
            span: self.span_from(&start),
            name,
            generic_params,
            ty,
            where_clause,
        })
    }
}
