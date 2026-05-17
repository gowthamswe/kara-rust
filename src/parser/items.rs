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

    pub(super) fn parse_function(
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

        // `#[track_caller]` is a bare type-level attribute on `fn`
        // items. Set the flag at parse so codegen / runtime slices can
        // consult one bool. The attribute MUST take no arguments —
        // `#[track_caller(...)]` or `#[track_caller = "..."]` is a
        // focused diagnostic at parse time so users see the malformed
        // shape immediately rather than discovering it through silent
        // ignore. Placement validation (must be on `fn`, rejected on
        // structs / enums / traits / impl blocks / etc.) lives in the
        // resolver — mirrors the `#[non_exhaustive]` parser/resolver
        // split, so downstream tools that re-parse without a resolver
        // still see the captured attribute.
        let is_track_caller = self.scan_track_caller_attr(&attributes);
        let deprecation = self.scan_deprecated_attr(&attributes);

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
            deprecation,
            is_track_caller,
        })
    }

    /// Scan `attributes` for `#[track_caller]`. Sets the flag when
    /// the attribute is present (in any number of occurrences — a
    /// duplicate is not a parser error, it is idempotent). Emits a
    /// focused `E_TRACK_CALLER_ARGS_NOT_PERMITTED` parse diagnostic
    /// for every offending occurrence that carries args or a string
    /// value — the attribute is bare. Diagnostic is anchored at the
    /// attribute's own span so the user can navigate directly to the
    /// malformed `#[...]`.
    fn scan_track_caller_attr(&mut self, attributes: &[Attribute]) -> bool {
        let mut present = false;
        for attr in attributes {
            if attr.name != "track_caller" {
                continue;
            }
            present = true;
            if !attr.args.is_empty() || attr.string_value.is_some() {
                self.errors.push(super::ParseError {
                    message: "error[E_TRACK_CALLER_ARGS_NOT_PERMITTED]: \
                              `#[track_caller]` takes no arguments — \
                              the attribute redirects the panic-site \
                              location and has no configurable shape; \
                              remove the arguments"
                        .to_string(),
                    span: attr.span.clone(),
                });
            }
        }
        present
    }

    /// Scan `attributes` for `#[deprecated]` and produce the
    /// `Deprecation` payload (slice 1/2 of design.md §
    /// `#[deprecated]` for Item Deprecation). Recognises three
    /// forms:
    /// - bare `#[deprecated]` → `Deprecation { since: None, note: None }`
    /// - shorthand `#[deprecated = "note"]` → `note` populated
    /// - long form `#[deprecated(since: "...", note: "...")]` —
    ///   both fields optional, accepted as `name = value` AND
    ///   `name: value` (the attribute-arg parser already accepts
    ///   both shapes; downstream stays uniform via `AttrArg.name`).
    ///
    /// Unknown keys (`#[deprecated(authored_by: "...")]`) emit
    /// `E_DEPRECATED_UNKNOWN_FIELD` enumerating the accepted set
    /// (`since`, `note`). Non-string values (`#[deprecated(since: 1)]`)
    /// emit `E_DEPRECATED_FIELD_NOT_STRING` naming the offending key.
    /// Positional args (`#[deprecated("oops")]`) emit
    /// `E_DEPRECATED_POSITIONAL_ARG` — the long form requires named
    /// fields, the only legal unnamed form is the shorthand
    /// `= "string"`.
    ///
    /// Multiple `#[deprecated]` attributes on the same item: only the
    /// first one wins; subsequent ones produce
    /// `E_DEPRECATED_DUPLICATE`. Same idempotency rule as Rust.
    pub(crate) fn scan_deprecated_attr(&mut self, attributes: &[Attribute]) -> Option<Deprecation> {
        let mut first: Option<Deprecation> = None;
        for attr in attributes {
            if attr.name != "deprecated" {
                continue;
            }
            if first.is_some() {
                self.errors.push(super::ParseError {
                    message: "error[E_DEPRECATED_DUPLICATE]: \
                              multiple `#[deprecated]` attributes on the \
                              same item; keep one and remove the rest"
                        .to_string(),
                    span: attr.span.clone(),
                });
                continue;
            }
            // Shorthand: `#[deprecated = "note"]`
            if let Some(s) = &attr.string_value {
                if !attr.args.is_empty() {
                    // Shouldn't happen — the attribute parser uses
                    // string_value XOR args, but pin the assumption.
                    self.errors.push(super::ParseError {
                        message: "error[E_DEPRECATED_DUPLICATE]: \
                                  `#[deprecated = \"...\"]` cannot also \
                                  carry parenthesised arguments"
                            .to_string(),
                        span: attr.span.clone(),
                    });
                    continue;
                }
                first = Some(Deprecation {
                    span: attr.span.clone(),
                    since: None,
                    note: Some(s.clone()),
                });
                continue;
            }
            // Bare or long form: `#[deprecated]` / `#[deprecated(...)]`
            let mut since: Option<String> = None;
            let mut note: Option<String> = None;
            for arg in &attr.args {
                let Some(name) = &arg.name else {
                    self.errors.push(super::ParseError {
                        message: "error[E_DEPRECATED_POSITIONAL_ARG]: \
                                  `#[deprecated(...)]` requires named \
                                  arguments — use `since: \"...\"` and/or \
                                  `note: \"...\"`, or the shorthand \
                                  `#[deprecated = \"note\"]`"
                            .to_string(),
                        span: arg.span.clone(),
                    });
                    continue;
                };
                let field_slot = match name.as_str() {
                    "since" => &mut since,
                    "note" => &mut note,
                    other => {
                        self.errors.push(super::ParseError {
                            message: format!(
                                "error[E_DEPRECATED_UNKNOWN_FIELD]: \
                                 `#[deprecated]` does not accept field \
                                 `{other}`; the accepted fields are \
                                 `since` and `note`",
                            ),
                            span: arg.span.clone(),
                        });
                        continue;
                    }
                };
                let Some(value_expr) = &arg.value else {
                    self.errors.push(super::ParseError {
                        message: format!(
                            "error[E_DEPRECATED_FIELD_NOT_STRING]: \
                             `#[deprecated]` field `{name}` requires a \
                             string-literal value",
                        ),
                        span: arg.span.clone(),
                    });
                    continue;
                };
                let ExprKind::StringLit(s) = &value_expr.kind else {
                    self.errors.push(super::ParseError {
                        message: format!(
                            "error[E_DEPRECATED_FIELD_NOT_STRING]: \
                             `#[deprecated]` field `{name}` requires a \
                             string-literal value",
                        ),
                        span: arg.span.clone(),
                    });
                    continue;
                };
                *field_slot = Some(s.clone());
            }
            first = Some(Deprecation {
                span: attr.span.clone(),
                since,
                note,
            });
        }
        first
    }

    pub(super) fn parse_fn_params(&mut self) -> Option<(Option<SelfParam>, Vec<Param>)> {
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

    pub(super) fn parse_param(&mut self) -> Option<Param> {
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
        // `#[non_exhaustive]` is a bare type-level attribute. Set the
        // flag at parse so the resolver / typechecker / exhaustiveness
        // pass can consult it without re-walking attributes. Placement
        // validation (must be on `pub`, rejected on traits / fns /
        // variants / impl blocks / type aliases) lives in the resolver
        // — mirrors the `#[compiler_builtin]` split between parser-
        // captures-flag and resolver-validates-placement.
        let is_non_exhaustive = attributes.iter().any(|a| a.name == "non_exhaustive");
        let deprecation = self.scan_deprecated_attr(&attributes);

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
            deprecation,
            is_non_exhaustive,
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

        let is_non_exhaustive = attributes.iter().any(|a| a.name == "non_exhaustive");
        let deprecation = self.scan_deprecated_attr(&attributes);

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
            deprecation,
            is_non_exhaustive,
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
        let deprecation = self.scan_deprecated_attr(&attributes);
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
            deprecation,
        })
    }
}
