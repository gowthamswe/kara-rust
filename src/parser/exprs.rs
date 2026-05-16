//! Pratt-parser expression parsing.
//!
//! Houses the full expression parser: the binding-power-driven loop
//! (`parse_expr_bp` / `parse_expr_bp_with_ctx`), the prefix dispatch
//! (`parse_prefix`), the control-flow expression heads (`if`, `match`,
//! `while`, `for`, `loop`), atoms (paren / tuple, array literal,
//! identifier-expr, struct-literal body), call-argument lists, break /
//! continue argument parsing, and the lookahead helpers
//! (`looks_like_struct_literal`, `lookahead_generic_args_call`,
//! `lookahead_concrete_type_ufcs`, `is_labeled_arg`, `is_loop_label`).
//!
//! Lives in a sibling `impl super::Parser` block.

use crate::ast::*;
use crate::token::{Span, Token};

use super::starts_upper;

impl super::Parser {
    // ── Expressions (Pratt Parser) ───────────────────────────────

    pub(crate) fn parse_expression(&mut self) -> Option<Expr> {
        self.parse_expr_bp(0)
    }

    /// Parse an expression in statement context. Differs from
    /// `parse_expression` only when the prefix is a block-like expression
    /// (see `is_block_like_prefix`): in that case, postfix operators are
    /// not consumed, so the closing `}` ends the statement and the next
    /// token starts a fresh one.
    pub(crate) fn parse_expression_stmt(&mut self) -> Option<Expr> {
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

    pub(crate) fn try_compound_op(&mut self) -> Option<CompoundOp> {
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
}
