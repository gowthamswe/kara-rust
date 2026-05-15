// src/span_visitor.rs

//! AST span visitor + a per-module `Span → file` lookup table.
//!
//! Used by the multi-file project-mode codegen path so late-phase
//! diagnostics (effect / ownership / concurrency / codegen / link)
//! can recover file-of-origin context. The super-program is built
//! by concatenating all module items; spans keep their original
//! `(offset, length, line, column)`, but the original `PathBuf` is
//! discarded by the concat. We rebuild it: at concat time we walk
//! every item's spans and record which module index owns each key;
//! at format time the diagnostic emitter looks the span up and
//! prefixes the message with `file:line:col`.
//!
//! Collision handling: if the same `(offset, length, line, column)`
//! key appears in two or more modules — which can happen when
//! distinct files have identical leading bytes, or when baked
//! stdlib items are spliced into multiple modules — the lookup
//! returns `None` and the caller falls back to the file-less
//! `line:col` form. Graceful degradation; never an error.

use crate::ast::{
    Block, CallArg, ClosureParam, EnsuresClause, Expr, ExprKind, ExternItem, FieldInit,
    FieldPattern, Function, ImplBlock, ImplItem, Item, MatchArm, Param, ParsedInterpolationPart,
    Pattern, PatternKind, ProviderBinding, Stmt, StmtKind, TypeExpr,
};
use crate::token::Span;

/// Recursively visit every `Span` reachable from `item`, calling
/// `visit` once per encountered span. Coverage targets the surface
/// most likely to host a late-phase diagnostic — function bodies,
/// statements, expressions, patterns — but the outer span of every
/// item variant is always visited so even item-level errors map.
pub fn visit_item_spans(item: &Item, visit: &mut impl FnMut(&Span)) {
    match item {
        Item::Function(f) => visit_function(f, visit),
        Item::StructDef(s) => {
            visit(&s.span);
            for field in &s.fields {
                visit(&field.span);
                visit_type(&field.ty, visit);
            }
            for inv in &s.invariants {
                visit_expr(inv, visit);
            }
        }
        Item::EnumDef(e) => {
            visit(&e.span);
            for v in &e.variants {
                visit(&v.span);
                match &v.kind {
                    crate::ast::VariantKind::Unit => {}
                    crate::ast::VariantKind::Tuple(tys) => {
                        for t in tys {
                            visit_type(t, visit);
                        }
                    }
                    crate::ast::VariantKind::Struct(fields) => {
                        for f in fields {
                            visit(&f.span);
                            visit_type(&f.ty, visit);
                        }
                    }
                }
            }
        }
        Item::TraitDef(t) => {
            visit(&t.span);
            for ti in &t.items {
                match ti {
                    crate::ast::TraitItem::Method(m) => {
                        visit(&m.span);
                        for p in &m.params {
                            visit_param(p, visit);
                        }
                        if let Some(rt) = &m.return_type {
                            visit_type(rt, visit);
                        }
                        for req in &m.requires {
                            visit_expr(req, visit);
                        }
                        for ens in &m.ensures {
                            visit_ensures(ens, visit);
                        }
                        if let Some(body) = &m.body {
                            visit_block(body, visit);
                        }
                    }
                    crate::ast::TraitItem::AssocType(a) => {
                        visit(&a.span);
                    }
                }
            }
        }
        Item::TraitAlias(a) => visit(&a.span),
        Item::MarkerTrait(m) => visit(&m.span),
        Item::ImplBlock(b) => visit_impl_block(b, visit),
        Item::EffectResource(r) => visit(&r.span),
        Item::EffectGroup(g) => visit(&g.span),
        Item::EffectVerbDecl(v) => visit(&v.span),
        Item::LayoutDef(l) => visit(&l.span),
        Item::UseDecl(u) => visit(&u.span),
        Item::Import(i) => visit(&i.span),
        Item::ConstDecl(c) => {
            visit(&c.span);
            visit_type(&c.ty, visit);
            visit_expr(&c.value, visit);
        }
        Item::AliasDecl(a) => visit(&a.span),
        Item::IndependentDecl(i) => visit(&i.span),
        Item::ExternFunction(e) => {
            visit(&e.span);
            for p in &e.params {
                visit_param(p, visit);
            }
            if let Some(rt) = &e.return_type {
                visit_type(rt, visit);
            }
        }
        Item::ExternBlock(b) => {
            visit(&b.span);
            for it in &b.items {
                match it {
                    ExternItem::Function(e) => {
                        visit(&e.span);
                        for p in &e.params {
                            visit_param(p, visit);
                        }
                        if let Some(rt) = &e.return_type {
                            visit_type(rt, visit);
                        }
                    }
                    ExternItem::OpaqueType(o) => {
                        visit(&o.span);
                    }
                }
            }
        }
        Item::TypeAlias(a) => {
            visit(&a.span);
            visit_type(&a.ty, visit);
            if let Some(r) = &a.refinement {
                visit_expr(r, visit);
            }
        }
        Item::DistinctType(d) => {
            visit(&d.span);
            visit_type(&d.base_type, visit);
            if let Some(r) = &d.refinement {
                visit_expr(r, visit);
            }
        }
    }
}

fn visit_function(f: &Function, visit: &mut impl FnMut(&Span)) {
    visit(&f.span);
    for p in &f.params {
        visit_param(p, visit);
    }
    if let Some(rt) = &f.return_type {
        visit_type(rt, visit);
    }
    for req in &f.requires {
        visit_expr(req, visit);
    }
    for ens in &f.ensures {
        visit_ensures(ens, visit);
    }
    visit_block(&f.body, visit);
}

fn visit_impl_block(b: &ImplBlock, visit: &mut impl FnMut(&Span)) {
    visit(&b.span);
    visit_type(&b.target_type, visit);
    for ii in &b.items {
        match ii {
            ImplItem::Method(m) => visit_function(m, visit),
            ImplItem::AssocType(a) => {
                visit(&a.span);
                visit_type(&a.ty, visit);
            }
        }
    }
}

fn visit_param(p: &Param, visit: &mut impl FnMut(&Span)) {
    visit(&p.span);
    visit_pattern(&p.pattern, visit);
    visit_type(&p.ty, visit);
    if let Some(d) = &p.default_value {
        visit_expr(d, visit);
    }
}

fn visit_ensures(e: &EnsuresClause, visit: &mut impl FnMut(&Span)) {
    visit(&e.span);
    visit_expr(&e.body, visit);
}

fn visit_type(t: &TypeExpr, visit: &mut impl FnMut(&Span)) {
    // The outer span suffices for v1 — late-phase diagnostics rarely
    // pin a sub-region of a type expression. Deeper coverage (per
    // `TypeKind` variant) is a nice-to-have follow-up.
    visit(&t.span);
}

fn visit_block(b: &Block, visit: &mut impl FnMut(&Span)) {
    visit(&b.span);
    for s in &b.stmts {
        visit_stmt(s, visit);
    }
    if let Some(fe) = &b.final_expr {
        visit_expr(fe, visit);
    }
}

fn visit_stmt(s: &Stmt, visit: &mut impl FnMut(&Span)) {
    visit(&s.span);
    match &s.kind {
        StmtKind::Let {
            pattern, ty, value, ..
        } => {
            visit_pattern(pattern, visit);
            if let Some(t) = ty {
                visit_type(t, visit);
            }
            visit_expr(value, visit);
        }
        StmtKind::LetUninit { name_span, ty, .. } => {
            visit(name_span);
            visit_type(ty, visit);
        }
        StmtKind::LetElse {
            pattern,
            ty,
            value,
            else_block,
            ..
        } => {
            visit_pattern(pattern, visit);
            if let Some(t) = ty {
                visit_type(t, visit);
            }
            visit_expr(value, visit);
            visit_block(else_block, visit);
        }
        StmtKind::Defer { body } => visit_block(body, visit),
        StmtKind::ErrDefer { body, .. } => visit_block(body, visit),
        StmtKind::Assign { target, value } => {
            visit_expr(target, visit);
            visit_expr(value, visit);
        }
        StmtKind::CompoundAssign { target, value, .. } => {
            visit_expr(target, visit);
            visit_expr(value, visit);
        }
        StmtKind::Expr(e) => visit_expr(e, visit),
    }
}

fn visit_expr(e: &Expr, visit: &mut impl FnMut(&Span)) {
    visit(&e.span);
    match &e.kind {
        ExprKind::Integer(_, _)
        | ExprKind::Float(_, _)
        | ExprKind::CharLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::MultiStringLit(_)
        | ExprKind::Bool(_)
        | ExprKind::Identifier(_)
        | ExprKind::Path { .. }
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::PipePlaceholder
        | ExprKind::Continue { .. }
        | ExprKind::Error => {}
        ExprKind::InterpolatedStringLit(parts) => {
            for p in parts {
                if let ParsedInterpolationPart::Expr(inner) = p {
                    visit_expr(inner, visit);
                }
            }
        }
        ExprKind::Binary { left, right, .. } => {
            visit_expr(left, visit);
            visit_expr(right, visit);
        }
        ExprKind::Unary { operand, .. } => visit_expr(operand, visit),
        ExprKind::Question(inner) => visit_expr(inner, visit),
        ExprKind::OptionalChain { object, args, .. } => {
            visit_expr(object, visit);
            if let Some(a) = args {
                for arg in a {
                    visit_call_arg(arg, visit);
                }
            }
        }
        ExprKind::NilCoalesce { left, right } => {
            visit_expr(left, visit);
            visit_expr(right, visit);
        }
        ExprKind::Call { callee, args } => {
            visit_expr(callee, visit);
            for a in args {
                visit_call_arg(a, visit);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            visit_expr(object, visit);
            for a in args {
                visit_call_arg(a, visit);
            }
        }
        ExprKind::FieldAccess { object, .. } => visit_expr(object, visit),
        ExprKind::TupleIndex { object, .. } => visit_expr(object, visit),
        ExprKind::Index { object, index } => {
            visit_expr(object, visit);
            visit_expr(index, visit);
        }
        ExprKind::Block(b) => visit_block(b, visit),
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            visit_expr(condition, visit);
            visit_block(then_block, visit);
            if let Some(e) = else_branch {
                visit_expr(e, visit);
            }
        }
        ExprKind::IfLet {
            pattern,
            value,
            then_block,
            else_branch,
        } => {
            visit_pattern(pattern, visit);
            visit_expr(value, visit);
            visit_block(then_block, visit);
            if let Some(e) = else_branch {
                visit_expr(e, visit);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            visit_expr(scrutinee, visit);
            for arm in arms {
                visit_match_arm(arm, visit);
            }
        }
        ExprKind::While {
            condition, body, ..
        } => {
            visit_expr(condition, visit);
            visit_block(body, visit);
        }
        ExprKind::WhileLet {
            pattern,
            value,
            body,
            ..
        } => {
            visit_pattern(pattern, visit);
            visit_expr(value, visit);
            visit_block(body, visit);
        }
        ExprKind::For {
            pattern,
            iterable,
            body,
            ..
        } => {
            visit_pattern(pattern, visit);
            visit_expr(iterable, visit);
            visit_block(body, visit);
        }
        ExprKind::Loop { body, .. } => visit_block(body, visit),
        ExprKind::LabeledBlock {
            label_span, body, ..
        } => {
            visit(label_span);
            visit_block(body, visit);
        }
        ExprKind::Closure {
            params,
            prefix_span,
            body,
            ..
        } => {
            if let Some(ps) = prefix_span {
                visit(ps);
            }
            for cp in params {
                visit_closure_param(cp, visit);
            }
            visit_expr(body, visit);
        }
        ExprKind::Return(opt) => {
            if let Some(inner) = opt {
                visit_expr(inner, visit);
            }
        }
        ExprKind::Break { value, .. } => {
            if let Some(v) = value {
                visit_expr(v, visit);
            }
        }
        ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
            for x in exprs {
                visit_expr(x, visit);
            }
        }
        ExprKind::PrefixCollectionLiteral { items, .. } => {
            for x in items {
                visit_expr(x, visit);
            }
        }
        ExprKind::RepeatLiteral { value, count, .. } => {
            visit_expr(value, visit);
            visit_expr(count, visit);
        }
        ExprKind::MapLiteral(pairs) => {
            for (k, v) in pairs {
                visit_expr(k, visit);
                visit_expr(v, visit);
            }
        }
        ExprKind::StructLiteral { fields, spread, .. } => {
            for f in fields {
                visit_field_init(f, visit);
            }
            if let Some(sp) = spread {
                visit_expr(sp, visit);
            }
        }
        ExprKind::Pipe { left, right } => {
            visit_expr(left, visit);
            visit_expr(right, visit);
        }
        ExprKind::Cast { expr, ty } => {
            visit_expr(expr, visit);
            visit_type(ty, visit);
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                visit_expr(s, visit);
            }
            if let Some(e) = end {
                visit_expr(e, visit);
            }
        }
        ExprKind::Unsafe(b) | ExprKind::Try(b) | ExprKind::Seq(b) | ExprKind::Par(b) => {
            visit_block(b, visit);
        }
        ExprKind::Lock { body, .. } => visit_block(body, visit),
        ExprKind::Providers { bindings, body } => {
            for pb in bindings {
                visit_provider_binding(pb, visit);
            }
            visit_block(body, visit);
        }
    }
}

fn visit_call_arg(a: &CallArg, visit: &mut impl FnMut(&Span)) {
    visit(&a.span);
    visit_expr(&a.value, visit);
}

fn visit_match_arm(a: &MatchArm, visit: &mut impl FnMut(&Span)) {
    visit(&a.span);
    visit_pattern(&a.pattern, visit);
    if let Some(g) = &a.guard {
        visit_expr(g, visit);
    }
    visit_expr(&a.body, visit);
}

fn visit_closure_param(cp: &ClosureParam, visit: &mut impl FnMut(&Span)) {
    visit(&cp.span);
    visit_pattern(&cp.pattern, visit);
    if let Some(t) = &cp.ty {
        visit_type(t, visit);
    }
}

fn visit_field_init(f: &FieldInit, visit: &mut impl FnMut(&Span)) {
    visit(&f.span);
    visit_expr(&f.value, visit);
}

fn visit_provider_binding(pb: &ProviderBinding, visit: &mut impl FnMut(&Span)) {
    visit(&pb.resource_span);
    visit_expr(&pb.value, visit);
}

fn visit_pattern(p: &Pattern, visit: &mut impl FnMut(&Span)) {
    visit(&p.span);
    match &p.kind {
        PatternKind::Wildcard
        | PatternKind::Binding(_)
        | PatternKind::Literal(_)
        | PatternKind::RangePattern { .. } => {}
        PatternKind::AtBinding { pattern, .. } => visit_pattern(pattern, visit),
        PatternKind::Struct { fields, .. } => {
            for fp in fields {
                visit_field_pattern(fp, visit);
            }
        }
        PatternKind::TupleVariant { patterns, .. } | PatternKind::Tuple(patterns) => {
            for inner in patterns {
                visit_pattern(inner, visit);
            }
        }
        PatternKind::Or(alts) => {
            for inner in alts {
                visit_pattern(inner, visit);
            }
        }
        PatternKind::Slice {
            prefix,
            rest: _,
            suffix,
        } => {
            for inner in prefix.iter().chain(suffix.iter()) {
                visit_pattern(inner, visit);
            }
        }
    }
}

fn visit_field_pattern(fp: &FieldPattern, visit: &mut impl FnMut(&Span)) {
    visit(&fp.span);
    if let Some(inner) = &fp.pattern {
        visit_pattern(inner, visit);
    }
}

// ── ModuleSpanTable ────────────────────────────────────────────

/// Lookup key for cross-module span → file resolution. Pulls all four
/// `Span` fields rather than just `offset` because the super-program
/// concat preserves the per-module offsets verbatim, which means two
/// distinct modules can have spans with identical offsets but different
/// `(line, column)` — and vice versa. Hashing on all four narrows the
/// collision rate without ever fully eliminating it; collisions degrade
/// gracefully via `ModuleSpanTable::lookup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanLookupKey {
    pub offset: usize,
    pub length: usize,
    pub line: usize,
    pub column: usize,
}

impl SpanLookupKey {
    pub fn from_span(s: &Span) -> Self {
        SpanLookupKey {
            offset: s.offset,
            length: s.length,
            line: s.line,
            column: s.column,
        }
    }
}

/// Cross-module diagnostic file resolution table built at multi-file
/// codegen concat. Lookup returns `Some(file)` only when the span maps
/// to exactly one module — ambiguous keys (collision across modules)
/// resolve to `None` and the caller falls back to a file-less
/// diagnostic.
#[derive(Default)]
pub struct ModuleSpanTable {
    /// Module file paths, indexed by concat order.
    pub files: Vec<std::path::PathBuf>,
    /// Per-key, the module indices that emitted a span matching the key.
    /// Stored as `Vec` to detect collisions; 1-element vecs are the
    /// unambiguous case (>99% in practice).
    by_span: std::collections::HashMap<SpanLookupKey, Vec<usize>>,
}

impl ModuleSpanTable {
    pub fn new() -> Self {
        ModuleSpanTable::default()
    }

    /// Register a module file and return its index. Call once per
    /// module *before* feeding its items through `record_item`.
    pub fn register_module(&mut self, file: std::path::PathBuf) -> usize {
        let idx = self.files.len();
        self.files.push(file);
        idx
    }

    /// Walk every span reachable from `item` and record the module
    /// index that owns it.
    pub fn record_item(&mut self, module_idx: usize, item: &Item) {
        visit_item_spans(item, &mut |s| {
            let key = SpanLookupKey::from_span(s);
            let entry = self.by_span.entry(key).or_default();
            if !entry.contains(&module_idx) {
                entry.push(module_idx);
            }
        });
    }

    /// Resolve a span to its owning file. Returns `None` if the span
    /// was never seen (e.g., a synthetic span minted post-concat) or
    /// if multiple modules registered the same key (collision —
    /// degrade gracefully to file-less diagnostic).
    pub fn lookup(&self, span: &Span) -> Option<&std::path::Path> {
        let key = SpanLookupKey::from_span(span);
        let mods = self.by_span.get(&key)?;
        if mods.len() == 1 {
            Some(self.files[mods[0]].as_path())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn unique_span_resolves_to_its_module() {
        let mut table = ModuleSpanTable::new();
        let a = parse("fn alpha() -> i64 { 1 }\n");
        let b = parse("fn beta() -> i64 { 2 }\n");
        let idx_a = table.register_module(std::path::PathBuf::from("a.kara"));
        let idx_b = table.register_module(std::path::PathBuf::from("b.kara"));
        for it in &a.program.items {
            table.record_item(idx_a, it);
        }
        for it in &b.program.items {
            table.record_item(idx_b, it);
        }
        // Pick the body's only stmt span from b — it should resolve.
        let Item::Function(fb) = &b.program.items[0] else {
            panic!()
        };
        let body_span = &fb.body.span;
        assert_eq!(
            table
                .lookup(body_span)
                .map(|p| p.to_string_lossy().into_owned()),
            Some("b.kara".to_string()),
        );
    }

    #[test]
    fn collision_returns_none() {
        // Identical source bytes → identical `(offset, length, line, col)`
        // across two distinct modules. Lookup must degrade to `None`.
        let mut table = ModuleSpanTable::new();
        let a = parse("fn dup() -> i64 { 7 }\n");
        let b = parse("fn dup() -> i64 { 7 }\n");
        let idx_a = table.register_module(std::path::PathBuf::from("a.kara"));
        let idx_b = table.register_module(std::path::PathBuf::from("b.kara"));
        for it in &a.program.items {
            table.record_item(idx_a, it);
        }
        for it in &b.program.items {
            table.record_item(idx_b, it);
        }
        let Item::Function(fa) = &a.program.items[0] else {
            panic!()
        };
        assert!(table.lookup(&fa.span).is_none());
    }
}
