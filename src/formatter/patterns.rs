//! Pattern printing for the canonical formatter.

use crate::ast::*;
use std::fmt::Write;

use super::{escape_string, float_suffix_str, int_suffix_str};

impl super::Formatter {
    pub(super) fn format_pattern(&mut self, pat: &Pattern) {
        match &pat.kind {
            PatternKind::Wildcard => self.write_str("_"),
            PatternKind::Binding(name) => self.write_ident(name),
            PatternKind::Literal(lit) => self.format_literal_pattern(lit),
            PatternKind::RangePattern {
                start,
                end,
                inclusive,
            } => {
                if let Some(s) = start {
                    self.format_literal_pattern(s);
                }
                if *inclusive {
                    self.write_str("..=");
                } else {
                    self.write_str("..");
                }
                if let Some(e) = end {
                    self.format_literal_pattern(e);
                }
            }
            PatternKind::AtBinding { name, pattern } => {
                self.write_ident(name);
                self.write_str(" @ ");
                self.format_pattern(pattern);
            }
            PatternKind::Struct { path, fields } => {
                self.write_path(path);
                self.write_str(" { ");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.write_ident(&f.name);
                    if let Some(ref p) = f.pattern {
                        self.write_str(": ");
                        self.format_pattern(p);
                    }
                }
                self.write_str(" }");
            }
            PatternKind::TupleVariant { path, patterns } => {
                self.write_path(path);
                self.write_str("(");
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_pattern(p);
                }
                self.write_str(")");
            }
            PatternKind::Tuple(patterns) => {
                self.write_str("(");
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_pattern(p);
                }
                self.write_str(")");
            }
            PatternKind::Or(alts) => {
                for (i, p) in alts.iter().enumerate() {
                    if i > 0 {
                        self.write_str(" | ");
                    }
                    self.format_pattern(p);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                self.write_str("[");
                let mut first = true;
                for p in prefix {
                    if !first {
                        self.write_str(", ");
                    }
                    self.format_pattern(p);
                    first = false;
                }
                if let Some(r) = rest {
                    if !first {
                        self.write_str(", ");
                    }
                    match r {
                        RestPattern::Ignored => self.write_str(".."),
                        RestPattern::Bound(name) => {
                            self.write_str("..");
                            self.write_ident(name);
                        }
                    }
                    first = false;
                }
                for p in suffix {
                    if !first {
                        self.write_str(", ");
                    }
                    self.format_pattern(p);
                    first = false;
                }
                self.write_str("]");
            }
        }
    }

    pub(super) fn format_literal_pattern(&mut self, lit: &LiteralPattern) {
        match lit {
            LiteralPattern::Integer(n, sfx) => {
                write!(self.output, "{n}").unwrap();
                if let Some(s) = sfx {
                    self.write_str(int_suffix_str(*s));
                }
            }
            LiteralPattern::Float(f, sfx) => {
                write!(self.output, "{f}").unwrap();
                if let Some(s) = sfx {
                    self.write_str(float_suffix_str(*s));
                }
            }
            LiteralPattern::Char(c) => write!(self.output, "'{c}'").unwrap(),
            LiteralPattern::String(s) => {
                self.write_str("\"");
                self.write_str(&escape_string(s));
                self.write_str("\"");
            }
            LiteralPattern::Bool(b) => self.write_str(if *b { "true" } else { "false" }),
        }
    }
}
