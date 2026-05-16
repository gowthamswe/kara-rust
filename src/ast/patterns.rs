//! Pattern AST — patterns, range patterns, slice patterns, literal
//! patterns, match arms.

use crate::token::{FloatSuffix, IntSuffix, Span};

use super::Expr;

// ── Patterns ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

impl Pattern {
    /// Collect all binding names from this pattern.
    pub fn binding_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        self.collect_bindings(&mut names);
        names
    }

    fn collect_bindings(&self, out: &mut Vec<String>) {
        match &self.kind {
            PatternKind::Binding(name) => out.push(name.clone()),
            PatternKind::Tuple(patterns) => {
                for p in patterns {
                    p.collect_bindings(out);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for f in fields {
                    if let Some(ref sub) = f.pattern {
                        sub.collect_bindings(out);
                    } else {
                        out.push(f.name.clone());
                    }
                }
            }
            PatternKind::TupleVariant { patterns, .. } => {
                for p in patterns {
                    p.collect_bindings(out);
                }
            }
            PatternKind::Or(alts) => {
                if let Some(first) = alts.first() {
                    first.collect_bindings(out);
                }
            }
            PatternKind::AtBinding { name, pattern } => {
                out.push(name.clone());
                pattern.collect_bindings(out);
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for p in prefix {
                    p.collect_bindings(out);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    out.push(name.clone());
                }
                for p in suffix {
                    p.collect_bindings(out);
                }
            }
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
        }
    }
}

#[derive(Debug, Clone)]
pub enum PatternKind {
    Wildcard,
    Binding(String),
    Literal(LiteralPattern),
    // `a..=b` → start=Some, end=Some
    // `..=b`  → start=None, end=Some
    // `a..`   → start=Some, end=None
    // bare `..` is rejected (not a valid pattern; use `_`)
    RangePattern {
        start: Option<LiteralPattern>,
        end: Option<LiteralPattern>,
        inclusive: bool,
    },
    AtBinding {
        name: String,
        pattern: Box<Pattern>,
    },
    Struct {
        path: Vec<String>,
        fields: Vec<FieldPattern>,
    },
    TupleVariant {
        path: Vec<String>,
        patterns: Vec<Pattern>,
    },
    Tuple(Vec<Pattern>),
    Or(Vec<Pattern>),
    /// `[p1, p2, ..rest, p_n-1, p_n]` — `prefix`/`suffix` are leading/trailing
    /// element patterns, `rest` is the optional `..` or `..name` marker. At
    /// most one `..` is permitted per slice pattern (enforced at parse time).
    /// Sub-item 1 of the slice/array patterns entry (phase 5.2): AST shape
    /// lands now; typechecker rejects with a focused stub diagnostic until
    /// sub-item 2 lands.
    Slice {
        prefix: Vec<Pattern>,
        rest: Option<RestPattern>,
        suffix: Vec<Pattern>,
    },
}

/// Rest marker inside a slice pattern. `..` is `Ignored`; `..name` is
/// `Bound(name)` and introduces a fresh binding into the arm scope.
#[derive(Debug, Clone)]
pub enum RestPattern {
    Ignored,
    Bound(String),
}

#[derive(Debug, Clone)]
pub enum LiteralPattern {
    Integer(i64, Option<IntSuffix>),
    Float(f64, Option<FloatSuffix>),
    Char(char),
    String(String),
    Bool(bool),
}

#[derive(Debug, Clone)]
pub struct FieldPattern {
    pub name: String,
    pub pattern: Option<Pattern>,
    pub span: Span,
}

// ── Match ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}
