//! Extern-block parsing — `unsafe extern "ABI" { ... }` and the
//! per-item forms (extern fn declaration, opaque type declaration).
//! Also houses the diagnostic for the rejected bare module-scope
//! `extern "ABI" fn ...;` shape.

use crate::ast::*;
use crate::lexer::IdentClass;
use crate::token::{Span, Token};

use super::FnContext;

impl super::Parser {
    // ── Extern Functions (FFI) ───────────────────────────────────

    /// Parse an `unsafe extern "ABI" { ... }` block at module scope.
    /// The leading `unsafe` keyword has not yet been consumed; the
    /// `pending_doc` slot holds any `///` lines that preceded the
    /// block. `block_attributes` is the set of `#[...]` / `@...`
    /// attributes the caller already parsed — they live on the
    /// `ExternBlock` itself; downstream consumers (effectchecker,
    /// codegen) read both the block-level set and per-item set so the
    /// formatter can round-trip the block-level position faithfully.
    pub(super) fn parse_unsafe_extern_block(
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
        if !attributes.iter().any(|a| a.is_bare("kara_name")) {
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
        if !attributes.iter().any(|a| a.is_bare("kara_name")) {
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
    pub(super) fn error_bare_extern_at_module_scope(&mut self) {
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
}
