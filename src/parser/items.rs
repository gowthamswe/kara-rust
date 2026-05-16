//! Item-level parsing: the `parse_item` dispatcher + every concrete
//! item form.
//!
//! Houses the full Item grammar: functions (`parse_function`,
//! `parse_fn_params`, `parse_param`, `parse_param_pattern`), data
//! types (`parse_struct_def`, `parse_struct_fields`, `parse_enum_def`,
//! `parse_variant`, `parse_layout_def`, `parse_layout_field_list`),
//! trait surface (`parse_marker_trait`, `parse_trait_or_alias`,
//! `parse_trait_def_tail`, `parse_trait_alias_tail`,
//! `parse_assoc_type_decl`, `parse_trait_method`, `parse_impl_block`,
//! `parse_assoc_type_binding`), effect declarations
//! (`parse_effect_decl`, `parse_effect_group_body`,
//! `parse_optional_effect_list`, `parse_effect_list`,
//! `parse_resource`), module surface (`parse_use_decl`,
//! `parse_import_decl`, `parse_import_item_list`,
//! `parse_const_decl`, `parse_alias_decl`,
//! `parse_independent_decl`, `parse_type_alias`,
//! `parse_distinct_type`), and extern blocks
//! (`parse_unsafe_extern_block`, `parse_extern_block_item_fn`,
//! `parse_extern_block_item_opaque_type`).
//!
//! Lives in a sibling `impl super::Parser` block.

use crate::ast::*;
use crate::lexer::IdentClass;
use crate::token::{Span, Token};

use super::{render_type_for_diagnostic, FnContext, ParseError};

impl super::Parser {
    // ── Items ────────────────────────────────────────────────────

    pub(crate) fn parse_item(&mut self) -> Option<Item> {
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
    pub(crate) fn parse_param_pattern(&mut self) -> Option<Pattern> {
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

    pub(crate) fn parse_effect_list(&mut self, effect_vars: &[String]) -> Option<EffectList> {
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
}
