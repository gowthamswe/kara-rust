//! Pattern parsing — refutable + irrefutable forms for `let`, `match`,
//! function parameters, `if let`, `while let`, and `for` heads.
//!
//! Houses `parse_pattern` (the alternation `p1 | p2 | …` wrapper),
//! `parse_single_pattern` (the big PatternKind dispatch: identifier
//! bindings, literals + ranges, tuple / struct / enum-variant
//! destructuring, slice patterns with `..` rest, wildcards, etc.),
//! plus the pattern-literal helpers (`starts_literal_pattern` /
//! `parse_literal_pattern`) used to disambiguate the range-pattern
//! end position.
//!
//! Lives in a sibling `impl super::Parser` block.

use crate::ast::*;
use crate::lexer::IdentClass;
use crate::token::Token;

use super::{starts_upper, ParseError};

impl super::Parser {
    // ── Patterns ─────────────────────────────────────────────────

    pub(crate) fn parse_pattern(&mut self) -> Option<Pattern> {
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

    pub(crate) fn parse_single_pattern(&mut self) -> Option<Pattern> {
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
    // ── Pattern Helpers ───────────────────────────────────────────

    /// Parse a literal for use in range patterns (integer or char).
    /// True when `tok` starts a literal pattern (integer or char). Used
    /// by the range-pattern parser to disambiguate the bounded-exclusive
    /// form `lo..hi` from the half-open form `lo..` — only the former
    /// has a literal in end position.
    fn starts_literal_pattern(tok: &Token) -> bool {
        matches!(tok, Token::Integer(..) | Token::CharLiteral(_))
    }

    pub(crate) fn parse_literal_pattern(&mut self) -> Option<LiteralPattern> {
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
}
