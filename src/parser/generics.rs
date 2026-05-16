//! Generics, trait bounds, `where`/`requires`/`ensures` clauses,
//! generic-type-args, and the struct body grammar.
//!
//! Houses `parse_optional_generic_params` / `parse_generic_params`
//! (the `[T, const N: i64, with E]` form), `parse_trait_bound`
//! (the `T: Trait + 'a`-style bound recognizer),
//! `parse_optional_where_clause` (the trailing `where` clause for
//! item signatures), `parse_requires_clauses` /
//! `parse_ensures_clauses` (Hoare-style pre/post conditions),
//! `parse_struct_body` (struct fields + invariants), and
//! `parse_generic_type_args` (the call-site `[T1, T2, …]` shape).
//!
//! Lives in a sibling `impl super::Parser` block.

use crate::ast::*;
use crate::lexer::IdentClass;
use crate::token::Token;

impl super::Parser {
    pub(crate) fn parse_optional_generic_params(&mut self) -> Option<GenericParams> {
        if !self.check(&Token::LeftBracket) {
            return None;
        }
        self.parse_generic_params()
    }

    pub(crate) fn parse_generic_params(&mut self) -> Option<GenericParams> {
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

    pub(crate) fn parse_trait_bound(&mut self) -> Option<TraitBound> {
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

    pub(crate) fn parse_optional_where_clause(&mut self) -> Option<WhereClause> {
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

    pub(crate) fn parse_requires_clauses(&mut self) -> Vec<Expr> {
        let mut clauses = Vec::new();
        while self.eat(&Token::Requires) {
            if let Some(expr) = self.parse_expression() {
                clauses.push(expr);
            }
        }
        clauses
    }

    pub(crate) fn parse_ensures_clauses(&mut self) -> Vec<EnsuresClause> {
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

    pub(crate) fn parse_struct_body(&mut self) -> Option<(Vec<StructField>, Vec<Expr>)> {
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

    pub(crate) fn parse_generic_type_args(&mut self) -> Option<Vec<GenericArg>> {
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
}
