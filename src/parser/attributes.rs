//! Attribute parsing — both the `#[name(args)]` and the `@name(args)`
//! forms, plus the `#[unsafe(...)]` wrapper.
//!
//! Houses `parse_attributes` (the leading-attribute loop called by
//! `parse_item` and `parse_struct_body`), `parse_at_attribute`
//! (the `@name(args)` form), `parse_attribute` (the `#[name(args)]`
//! form — the main attribute grammar with positional / named
//! arguments, string values, and the `unsafe` keyword wrap), and
//! `parse_unsafe_wrapped_attribute` (the `#[unsafe(...)]` body).
//!
//! Lives in a sibling `impl super::Parser` block.

use crate::ast::*;
use crate::token::{Span, Token};

impl super::Parser {
    // ── Attributes ───────────────────────────────────────────────

    pub(crate) fn parse_attributes(&mut self) -> Vec<Attribute> {
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

    /// Parse `@name` shorthand attribute (e.g. `@no_rc`, `@noblock`). The
    /// `@`-form is a Kāra compiler shorthand for bare-name attributes; it
    /// never carries a namespace path, so the resulting `path` is always a
    /// single segment.
    fn parse_at_attribute(&mut self) -> Option<Attribute> {
        let start = self.current_span();
        self.expect(&Token::At)?;
        let name = self.expect_identifier()?;
        Some(Attribute {
            span: self.span_from(&start),
            path: vec![name],
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

        // Parse the attribute path — `IDENT ("::" IDENT)*` per syntax.md §8.
        // Bare-name attributes (`#[allow]`) produce a single-segment path;
        // namespaced ones (`#[diagnostic::on_unimplemented]`) produce
        // multi-segment paths that downstream namespace dispatch reads.
        let first = self.expect_identifier()?;
        let mut path = vec![first];
        while self.eat(&Token::ColonColon) {
            path.push(self.expect_identifier()?);
        }
        // `name` retained as the leading segment for the existing bare-name
        // guards below; multi-segment paths bypass those guards via the
        // `path.len() == 1` checks.
        let name = path[0].clone();

        // Bare `#[no_mangle]` / `#[link_section(...)]` — reject with a
        // focused diagnostic suggesting the `#[unsafe(...)]` wrap. The
        // rejection is scoped to single-segment paths because the linker
        // attributes never appear inside a namespace.
        // Errors do NOT bail; we keep parsing so error recovery remains
        // useful and the rest of the file still type-checks.
        if path.len() == 1 && name == "no_mangle" {
            self.error(
                "bare `#[no_mangle]` is not allowed — write `#[unsafe(no_mangle)]` \
                 instead. The `unsafe(...)` wrap is a visual trust-boundary marker: \
                 disabling name mangling can collide with foreign symbols and is an \
                 obligation the compiler cannot verify. \
                 See design.md § Linker Control Attributes.",
            );
        } else if path.len() == 1 && name == "link_section" {
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

        // Slice 6 of the lint-level-attributes entry
        // (`docs/implementation_checklist/phase-5-diagnostics.md` §
        // "Lint level attributes" slice 6) + slice 5 of the
        // `unsafe_op_in_unsafe_fn` epic. `unsafe_op_in_unsafe_fn` is a
        // *hard rule*, not a lint — the central lint registry in
        // `src/lints.rs` intentionally excludes it. Kāra is greenfield
        // and there is no migration story, so all four lint-level
        // attributes (`#[allow]` / `#[warn]` / `#[deny]` / `#[expect]`)
        // are rejected uniformly on the rule with
        // `error[E_LINT_LEVEL_ON_HARD_RULE]`. The diagnostic redirects
        // the author to the actual fix: wrap the offending operation
        // in an `unsafe { ... }` block (with a `// Safety:` comment
        // per the `undocumented_unsafe` lint).
        //
        // Recognised in two surface forms — positional
        // (`#[allow(unsafe_op_in_unsafe_fn)]`, the form anyone would
        // write) and named (`#[allow(name = unsafe_op_in_unsafe_fn)]`,
        // theoretical only) — both are caught. Matching the four
        // attribute names by bare-string keeps the rejection
        // synchronous with the parser surface even though the lint
        // registry is the authoritative list elsewhere — we cannot
        // call `LintLevel::from_attr_name` from inside `ast::*`
        // expression-walk code without a circular import, and the
        // four names are a fixed v1 surface per the spec.
        if path.len() == 1
            && matches!(name.as_str(), "allow" | "warn" | "deny" | "expect")
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
            self.error(&format!(
                "error[E_LINT_LEVEL_ON_HARD_RULE]: \
                 `#[{name}(unsafe_op_in_unsafe_fn)]` is not accepted — \
                 `unsafe_op_in_unsafe_fn` is a hard rule, not a lint, \
                 so none of `#[allow]` / `#[warn]` / `#[deny]` / \
                 `#[expect]` apply (Kāra is greenfield; there is no \
                 migration story). Wrap the offending operation in an \
                 `unsafe {{ ... }}` block instead, and add a \
                 `// Safety: ...` comment above the block per the \
                 `undocumented_unsafe` lint."
            ));
        }

        Some(Attribute {
            span: self.span_from(&start),
            path,
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
            path: vec![inner_name],
            args: Vec::new(),
            string_value: attr_string_value,
        })
    }
}
