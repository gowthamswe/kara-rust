//! Canonical formatter for Kāra source code.
//!
//! Parses AST and reprints in deterministic canonical form.
//! Scope: syntactic canonicalization (like gofmt/rustfmt), not semantic normalization.

use crate::ast::*;
use crate::token::{FloatSuffix, IntSuffix};
use std::fmt::Write;

const INDENT: &str = "    ";

mod exprs;
mod patterns;
mod stmts;
mod types;

pub fn format_program(program: &Program) -> String {
    let mut f = Formatter::new();
    f.format_program(program);
    f.output
}

pub(super) struct Formatter {
    pub(super) output: String,
    pub(super) indent: usize,
}

impl Formatter {
    pub(super) fn new() -> Self {
        Formatter {
            output: String::new(),
            indent: 0,
        }
    }

    pub(super) fn push_indent(&mut self) {
        self.indent += 1;
    }

    pub(super) fn pop_indent(&mut self) {
        self.indent -= 1;
    }

    pub(super) fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.push_str(INDENT);
        }
    }

    pub(super) fn writeln(&mut self, s: &str) {
        self.write_indent();
        self.output.push_str(s);
        self.output.push('\n');
    }

    pub(super) fn write_str(&mut self, s: &str) {
        self.output.push_str(s);
    }

    /// Emit an AST-level identifier name, prepending `r#` when the bare name
    /// would otherwise lex as a keyword or reserved-for-future-use word — i.e.
    /// when round-tripping requires the raw-identifier escape (design.md §
    /// Raw Identifiers). Structural markers (`self`/`Self`/`_`/etc.) are
    /// rejected at lex time and never reach the formatter as plain `name`s.
    pub(super) fn write_ident(&mut self, name: &str) {
        if needs_raw_escape(name) {
            self.output.push_str("r#");
        }
        self.output.push_str(name);
    }

    /// Emit a dotted path, escaping each segment independently.
    pub(super) fn write_path(&mut self, segments: &[String]) {
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                self.output.push('.');
            }
            self.write_ident(seg);
        }
    }

    /// Emit a `, `-separated list of identifiers, escaping each independently.
    pub(super) fn write_ident_list(&mut self, names: &[String]) {
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.write_ident(n);
        }
    }

    /// Emit the visibility keyword (`pub ` / `private ` / `""`) for items
    /// that carry the three-level `Visibility`.
    pub(super) fn write_visibility(&mut self, v: Visibility) {
        match v {
            Visibility::Pub => self.write_str("pub "),
            Visibility::Private => self.write_str("private "),
            Visibility::Default => {}
        }
    }

    // ── Program ─────────────────────────────────────────────────

    pub(super) fn format_program(&mut self, program: &Program) {
        // Sort: use / import decls, then rest (preserving relative order within categories)
        let mut uses = Vec::new();
        let mut rest = Vec::new();

        for item in &program.items {
            match item {
                Item::UseDecl(_) | Item::Import(_) => uses.push(item),
                _ => rest.push(item),
            }
        }

        // Sort use / import decls alphabetically by path.
        uses.sort_by(|a, b| {
            let path_a = match a {
                Item::UseDecl(u) => u.path.clone(),
                Item::Import(i) => {
                    let mut p = i.path.clone();
                    if let Some(first) = i.items.first() {
                        p.push(first.name.clone());
                    }
                    p
                }
                _ => unreachable!(),
            };
            let path_b = match b {
                Item::UseDecl(u) => u.path.clone(),
                Item::Import(i) => {
                    let mut p = i.path.clone();
                    if let Some(first) = i.items.first() {
                        p.push(first.name.clone());
                    }
                    p
                }
                _ => unreachable!(),
            };
            path_a.cmp(&path_b)
        });

        for item in &uses {
            self.format_item(item);
        }
        if !uses.is_empty() && !rest.is_empty() {
            self.output.push('\n');
        }

        let mut first = true;
        for item in &rest {
            if !first {
                self.output.push('\n');
            }
            first = false;
            self.format_item(item);
        }
    }

    // ── Items ───────────────────────────────────────────────────

    pub(super) fn format_item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => self.format_function(f),
            Item::StructDef(s) => self.format_struct(s),
            Item::EnumDef(e) => self.format_enum(e),
            Item::TraitDef(t) => self.format_trait(t),
            Item::TraitAlias(t) => self.format_trait_alias(t),
            Item::MarkerTrait(t) => self.format_marker_trait(t),
            Item::ImplBlock(i) => self.format_impl(i),
            Item::EffectResource(e) => self.format_effect_resource(e),
            Item::EffectGroup(e) => self.format_effect_group(e),
            Item::EffectVerbDecl(e) => self.format_effect_verb_decl(e),
            Item::LayoutDef(l) => self.format_layout(l),
            Item::UseDecl(u) => {
                let vis = if u.is_pub { "pub " } else { "" };
                self.writeln(&format!("{vis}use {};", path_str(&u.path)));
            }
            Item::Import(i) => {
                let vis = if i.is_pub { "pub " } else { "" };
                let prefix = path_str(&i.path);
                let rendered = if i.items.len() == 1 {
                    let only = &i.items[0];
                    let base = if prefix.is_empty() {
                        ident_str(&only.name)
                    } else {
                        format!("{prefix}.{}", ident_str(&only.name))
                    };
                    match &only.alias {
                        Some(a) => format!("{base} as {}", ident_str(a)),
                        None => base,
                    }
                } else {
                    let parts: Vec<String> = i
                        .items
                        .iter()
                        .map(|it| match &it.alias {
                            Some(a) => format!("{} as {}", ident_str(&it.name), ident_str(a)),
                            None => ident_str(&it.name),
                        })
                        .collect();
                    if prefix.is_empty() {
                        format!("{{{}}}", parts.join(", "))
                    } else {
                        format!("{prefix}.{{{}}}", parts.join(", "))
                    }
                };
                self.writeln(&format!("{vis}import {rendered};"));
            }
            Item::ConstDecl(c) => self.format_const(c),
            Item::AliasDecl(a) => {
                self.writeln(&format!(
                    "alias {} = {};",
                    path_str(&a.left),
                    path_str(&a.right)
                ));
            }
            Item::IndependentDecl(i) => {
                self.writeln(&format!(
                    "independent {}, {};",
                    path_str(&i.left),
                    path_str(&i.right)
                ));
            }
            Item::ExternFunction(e) => self.format_extern_fn(e),
            Item::ExternBlock(b) => self.format_extern_block(b),
            Item::TypeAlias(t) => self.format_type_alias(t),
            Item::DistinctType(d) => self.format_distinct_type(d),
        }
    }

    // ── Attributes ──────────────────────────────────────────────

    pub(super) fn format_attributes(&mut self, attrs: &[Attribute]) {
        for attr in attrs {
            self.write_indent();
            // Linker attributes that carry trust-boundary obligations
            // re-emit with their `#[unsafe(...)]` wrap (per design.md
            // § Linker Control Attributes). Internal storage strips the
            // wrap so downstream consumers stay simple; the formatter
            // restores it for round-trip idempotence and for the visual
            // grep-for-`#[unsafe(` review pattern the spec describes.
            let unsafe_wrapped = matches!(attr.name.as_str(), "no_mangle" | "link_section");
            if unsafe_wrapped {
                self.write_str("#[unsafe(");
                self.write_ident(&attr.name);
                if let Some(ref s) = attr.string_value {
                    self.write_str("(\"");
                    self.write_str(s);
                    self.write_str("\")");
                }
                self.write_str(")]\n");
                continue;
            }

            self.write_str("#[");
            self.write_ident(&attr.name);
            if !attr.args.is_empty() {
                self.write_str("(");
                for (i, arg) in attr.args.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    match (&arg.name, &arg.value) {
                        (Some(n), Some(v)) => {
                            self.write_ident(n);
                            self.write_str(" = ");
                            self.format_expr(v);
                        }
                        (Some(n), None) => self.write_ident(n),
                        (None, Some(v)) => self.format_expr(v),
                        (None, None) => {}
                    }
                }
                self.write_str(")");
            }
            if let Some(ref s) = attr.string_value {
                self.write_str("(\"");
                self.write_str(s);
                self.write_str("\")");
            }
            self.write_str("]\n");
        }
    }

    // ── Functions ───────────────────────────────────────────────

    pub(super) fn format_function(&mut self, f: &Function) {
        self.format_attributes(&f.attributes);
        self.write_indent();
        self.write_visibility(f.visibility());
        self.write_str("fn ");
        self.write_ident(&f.name);
        self.format_generic_params(&f.generic_params);
        self.write_str("(");
        self.format_fn_params(&f.self_param, &f.params);
        self.write_str(")");
        if let Some(ref rt) = f.return_type {
            self.write_str(" -> ");
            self.format_type_expr(rt);
        }
        self.format_effects(&f.effects);
        self.format_where_clause(&f.where_clause);
        self.format_requires(&f.requires);
        self.format_ensures(&f.ensures);
        self.write_str(" ");
        self.format_block(&f.body);
        self.output.push('\n');
    }

    pub(super) fn format_fn_params(&mut self, self_param: &Option<SelfParam>, params: &[Param]) {
        let mut first = true;
        if let Some(ref sp) = self_param {
            first = false;
            match sp {
                SelfParam::Owned => self.write_str("self"),
                SelfParam::Ref => self.write_str("ref self"),
                SelfParam::MutRef => self.write_str("mut ref self"),
            }
        }
        for p in params {
            if !first {
                self.write_str(", ");
            }
            first = false;
            self.format_pattern(&p.pattern);
            self.write_str(": ");
            self.format_type_expr(&p.ty);
            if let Some(ref dv) = p.default_value {
                self.write_str(" = ");
                self.format_expr(dv);
            }
        }
    }

    pub(super) fn format_effects(&mut self, effects: &Option<EffectList>) {
        let effects = match effects {
            Some(e) => e,
            None => return,
        };
        self.write_str(" with ");
        for (i, item) in effects.items.iter().enumerate() {
            if i > 0 {
                self.write_str(" ");
            }
            match item {
                EffectItem::Verb(v) => {
                    self.write_str(&format_effect_verb_kind(&v.kind));
                    self.write_str("(");
                    for (j, r) in v.resources.iter().enumerate() {
                        if j > 0 {
                            self.write_str(", ");
                        }
                        self.write_path(&r.path);
                    }
                    self.write_str(")");
                }
                EffectItem::Group(g) => self.write_ident(g),
                EffectItem::Polymorphic => self.write_str("_"),
                EffectItem::Variable(v) => self.write_ident(v),
            }
        }
    }

    pub(super) fn format_requires(&mut self, requires: &[Expr]) {
        for r in requires {
            self.write_str("\n");
            self.write_indent();
            self.write_str("    requires ");
            self.format_expr(r);
        }
    }

    pub(super) fn format_ensures(&mut self, ensures: &[EnsuresClause]) {
        for e in ensures {
            self.write_str("\n");
            self.write_indent();
            self.write_str("    ensures ");
            if let Some(ref p) = e.param {
                self.write_ident(p);
                self.write_str(" ");
            }
            self.format_expr(&e.body);
        }
    }

    pub(super) fn format_where_clause(&mut self, wc: &Option<WhereClause>) {
        let wc = match wc {
            Some(w) => w,
            None => return,
        };
        self.write_str("\n");
        self.write_indent();
        self.write_str("where ");
        for (i, c) in wc.constraints.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            match c {
                WhereConstraint::TypeBound {
                    type_name, bounds, ..
                } => {
                    self.write_ident(type_name);
                    self.write_str(": ");
                    for (j, b) in bounds.iter().enumerate() {
                        if j > 0 {
                            self.write_str(" + ");
                        }
                        self.write_path(&b.path);
                        self.format_generic_args_opt(&b.generic_args);
                    }
                }
                WhereConstraint::AssocTypeEq {
                    type_name,
                    assoc_name,
                    ty,
                    ..
                } => {
                    self.write_ident(type_name);
                    self.write_str(".");
                    self.write_ident(assoc_name);
                    self.write_str(" = ");
                    self.format_type_expr(ty);
                }
                WhereConstraint::ConstPredicate { expr, .. } => {
                    self.format_expr(expr);
                }
            }
        }
    }

    // ── Structs ─────────────────────────────────────────────────

    pub(super) fn format_struct(&mut self, s: &StructDef) {
        self.format_attributes(&s.attributes);
        self.write_indent();
        self.write_visibility(s.visibility());
        if s.is_shared {
            self.write_str("shared ");
        }
        self.write_str("struct ");
        self.write_ident(&s.name);
        self.format_generic_params(&s.generic_params);
        self.format_where_clause(&s.where_clause);
        self.write_str(" {\n");
        self.push_indent();
        for field in &s.fields {
            self.write_indent();
            if field.is_pub {
                self.write_str("pub ");
            }
            if field.is_mut {
                self.write_str("mut ");
            }
            self.write_ident(&field.name);
            self.write_str(": ");
            self.format_type_expr(&field.ty);
            self.write_str(",\n");
        }
        for inv in &s.invariants {
            self.write_indent();
            self.write_str("invariant ");
            self.format_expr(inv);
            self.write_str("\n");
        }
        self.pop_indent();
        self.writeln("}");
    }

    // ── Enums ───────────────────────────────────────────────────

    pub(super) fn format_enum(&mut self, e: &EnumDef) {
        self.format_attributes(&e.attributes);
        self.write_indent();
        self.write_visibility(e.visibility());
        if e.is_shared {
            self.write_str("shared ");
        }
        self.write_str("enum ");
        self.write_ident(&e.name);
        self.format_generic_params(&e.generic_params);
        self.format_where_clause(&e.where_clause);
        self.write_str(" {\n");
        self.push_indent();
        for variant in &e.variants {
            self.write_indent();
            self.write_ident(&variant.name);
            match &variant.kind {
                VariantKind::Unit => {}
                VariantKind::Tuple(types) => {
                    self.write_str("(");
                    for (i, ty) in types.iter().enumerate() {
                        if i > 0 {
                            self.write_str(", ");
                        }
                        self.format_type_expr(ty);
                    }
                    self.write_str(")");
                }
                VariantKind::Struct(fields) => {
                    self.write_str(" {\n");
                    self.push_indent();
                    for field in fields {
                        self.write_indent();
                        self.write_ident(&field.name);
                        self.write_str(": ");
                        self.format_type_expr(&field.ty);
                        self.write_str(",\n");
                    }
                    self.pop_indent();
                    self.write_indent();
                    self.write_str("}");
                }
            }
            self.write_str(",\n");
        }
        self.pop_indent();
        self.writeln("}");
    }

    // ── Traits ──────────────────────────────────────────────────

    pub(super) fn format_marker_trait(&mut self, t: &MarkerTraitDef) {
        self.format_attributes(&t.attributes);
        self.write_indent();
        if t.is_pub {
            self.write_str("pub ");
        } else if t.is_private {
            self.write_str("private ");
        }
        self.write_str("marker trait ");
        self.write_ident(&t.name);
        self.format_generic_params(&t.generic_params);
        if !t.supertraits.is_empty() {
            self.write_str(": ");
            for (i, bound) in t.supertraits.iter().enumerate() {
                if i > 0 {
                    self.write_str(" + ");
                }
                self.write_path(&bound.path);
            }
        }
        self.format_where_clause(&t.where_clause);
        if t.body_brace {
            self.write_str(" { }\n");
        } else {
            self.write_str(";\n");
        }
    }

    pub(super) fn format_trait_alias(&mut self, t: &TraitAliasDef) {
        self.format_attributes(&t.attributes);
        self.write_indent();
        if t.is_pub {
            self.write_str("pub ");
        } else if t.is_private {
            self.write_str("private ");
        }
        self.write_str("trait ");
        self.write_ident(&t.name);
        self.format_generic_params(&t.generic_params);
        self.write_str(" = ");
        for (i, bound) in t.bounds.iter().enumerate() {
            if i > 0 {
                self.write_str(" + ");
            }
            self.write_path(&bound.path);
        }
        self.format_where_clause(&t.where_clause);
        self.write_str(";\n");
    }

    pub(super) fn format_trait(&mut self, t: &TraitDef) {
        self.format_attributes(&t.attributes);
        self.write_indent();
        self.write_visibility(t.visibility());
        self.write_str("trait ");
        self.write_ident(&t.name);
        self.format_generic_params(&t.generic_params);
        if !t.supertraits.is_empty() {
            self.write_str(": ");
            for (i, bound) in t.supertraits.iter().enumerate() {
                if i > 0 {
                    self.write_str(" + ");
                }
                self.write_path(&bound.path);
            }
        }
        self.format_effects(&t.trait_effects);
        self.format_where_clause(&t.where_clause);
        self.write_str(" {\n");
        self.push_indent();
        for item in &t.items {
            match item {
                TraitItem::Method(m) => self.format_trait_method(m),
                TraitItem::AssocType(a) => {
                    self.write_indent();
                    self.write_str("type ");
                    self.write_ident(&a.name);
                    if !a.bounds.is_empty() {
                        self.write_str(": ");
                        for (i, b) in a.bounds.iter().enumerate() {
                            if i > 0 {
                                self.write_str(" + ");
                            }
                            self.write_path(&b.path);
                        }
                    }
                    self.write_str(";\n");
                }
            }
        }
        self.pop_indent();
        self.writeln("}");
    }

    pub(super) fn format_trait_method(&mut self, m: &TraitMethod) {
        self.write_indent();
        self.write_str("fn ");
        self.write_ident(&m.name);
        self.format_generic_params(&m.generic_params);
        self.write_str("(");
        self.format_fn_params(&m.self_param, &m.params);
        self.write_str(")");
        if let Some(ref rt) = m.return_type {
            self.write_str(" -> ");
            self.format_type_expr(rt);
        }
        self.format_effects(&m.effects);
        self.format_where_clause(&m.where_clause);
        match &m.body {
            Some(body) => {
                self.write_str(" ");
                self.format_block(body);
                self.output.push('\n');
            }
            None => self.write_str(";\n"),
        }
    }

    // ── Impl Blocks ─────────────────────────────────────────────

    pub(super) fn format_impl(&mut self, imp: &ImplBlock) {
        self.format_attributes(&imp.attributes);
        self.write_indent();
        self.write_str("impl");
        self.format_generic_params(&imp.generic_params);
        if let Some(ref trait_name) = imp.trait_name {
            self.write_str(" ");
            self.write_path(&trait_name.segments);
            self.format_generic_args_opt(&trait_name.generic_args);
            self.write_str(" for");
        }
        self.write_str(" ");
        self.format_type_expr(&imp.target_type);
        self.format_where_clause(&imp.where_clause);
        self.write_str(" {\n");
        self.push_indent();

        // Sort methods alphabetically for canonical output
        let mut methods: Vec<&ImplItem> = imp.items.iter().collect();
        methods.sort_by(|a, b| {
            let name_a = impl_item_name(a);
            let name_b = impl_item_name(b);
            name_a.cmp(name_b)
        });

        let mut first = true;
        for item in &methods {
            if !first {
                self.output.push('\n');
            }
            first = false;
            match item {
                ImplItem::Method(m) => self.format_function(m),
                ImplItem::AssocType(a) => {
                    self.write_indent();
                    self.write_str("type ");
                    self.write_ident(&a.name);
                    self.write_str(" = ");
                    self.format_type_expr(&a.ty);
                    self.write_str(";\n");
                }
            }
        }
        self.pop_indent();
        self.writeln("}");
    }

    // ── Effect Declarations ─────────────────────────────────────

    pub(super) fn format_effect_resource(&mut self, e: &EffectResourceDecl) {
        self.write_indent();
        self.write_str("effect resource ");
        self.write_ident(&e.name);
        self.format_generic_params(&e.generic_params);
        if let Some(ref pt) = e.provider_trait {
            self.write_str(": ");
            self.write_ident(pt);
        }
        self.write_str(";\n");
    }

    pub(super) fn format_effect_group(&mut self, e: &EffectGroupDecl) {
        self.write_indent();
        if e.is_pub {
            self.write_str("pub ");
        }
        if e.is_stable {
            self.write_str("stable ");
        }
        self.write_str("effect ");
        self.write_ident(&e.name);
        self.write_str(" = ");
        for (i, term) in e.body.iter().enumerate() {
            if i > 0 {
                self.write_str(" ");
            }
            match term {
                EffectGroupTerm::Verb(v) => {
                    self.write_str(&format_effect_verb_kind(&v.kind));
                    self.write_str("(");
                    for (j, r) in v.resources.iter().enumerate() {
                        if j > 0 {
                            self.write_str(", ");
                        }
                        self.write_path(&r.path);
                    }
                    self.write_str(")");
                }
                EffectGroupTerm::GroupRef(g) => self.write_ident(g),
            }
        }
        self.write_str(";\n");
    }

    pub(super) fn format_effect_verb_decl(&mut self, e: &EffectVerbDecl) {
        self.write_indent();
        if e.is_pub {
            self.write_str("pub ");
        }
        if e.is_transparent {
            self.write_str("transparent ");
        }
        self.write_str("effect verb ");
        self.write_ident(&e.verb_name);
        self.write_str(";\n");
    }

    // ── Layout ──────────────────────────────────────────────────

    pub(super) fn format_layout(&mut self, l: &LayoutDef) {
        self.write_indent();
        self.write_str("layout ");
        self.write_ident(&l.name);
        self.write_str(" for ");
        self.format_type_expr(&l.collection_type);
        self.write_str(" {\n");
        self.push_indent();
        for item in &l.items {
            match item {
                LayoutItem::Group {
                    name,
                    fields,
                    align,
                    ..
                } => {
                    self.write_indent();
                    self.write_str("group ");
                    self.write_ident(name);
                    self.write_str(" { ");
                    self.write_ident_list(fields);
                    self.write_str(" }");
                    if let Some(n) = align {
                        self.write_str(&format!(" align({})", n));
                    }
                    self.write_str("\n");
                }
                LayoutItem::Cold { fields, .. } => {
                    self.write_indent();
                    self.write_str("cold { ");
                    self.write_ident_list(fields);
                    self.write_str(" }\n");
                }
                LayoutItem::SplitByVariant(_) => {
                    self.writeln("split_by_variant;");
                }
            }
        }
        self.pop_indent();
        self.writeln("}");
    }

    // ── Const ───────────────────────────────────────────────────

    pub(super) fn format_const(&mut self, c: &ConstDecl) {
        self.write_indent();
        self.write_visibility(c.visibility());
        self.write_str("const ");
        self.write_ident(&c.name);
        self.write_str(": ");
        self.format_type_expr(&c.ty);
        self.write_str(" = ");
        self.format_expr(&c.value);
        self.write_str(";\n");
    }

    // ── Extern ──────────────────────────────────────────────────

    pub(super) fn format_extern_fn(&mut self, e: &ExternFunction) {
        self.write_indent();
        self.write_visibility(e.visibility());
        write!(self.output, "extern \"{}\" fn ", e.abi).unwrap();
        self.write_ident(&e.name);
        self.write_str("(");
        for (i, p) in e.params.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            self.format_pattern(&p.pattern);
            self.write_str(": ");
            self.format_type_expr(&p.ty);
        }
        self.write_str(")");
        if let Some(ref rt) = e.return_type {
            self.write_str(" -> ");
            self.format_type_expr(rt);
        }
        self.format_effects(&e.effects);
        self.write_str(";\n");
    }

    pub(super) fn format_extern_block(&mut self, b: &ExternBlock) {
        // Block-level attributes are stored on the block (not pre-merged
        // into per-item attributes) so the formatter renders them at the
        // block-header position, preserving round-trip idempotence:
        // `@noblock unsafe extern "C" { fn a; }` formats back to itself.
        self.format_attributes(&b.attributes);
        self.write_indent();
        writeln!(self.output, "unsafe extern \"{}\" {{", b.abi).unwrap();
        self.indent += 1;
        for item in &b.items {
            match item {
                ExternItem::Function(f) => self.format_extern_block_item_fn(f),
                ExternItem::OpaqueType(o) => self.format_extern_block_item_opaque_type(o),
            }
        }
        self.indent -= 1;
        self.write_indent();
        self.write_str("}\n");
    }

    pub(super) fn format_extern_block_item_fn(&mut self, e: &ExternFunction) {
        self.format_attributes(&e.attributes);
        self.write_indent();
        self.write_visibility(e.visibility());
        self.write_str("fn ");
        self.write_ident(&e.name);
        self.write_str("(");
        for (i, p) in e.params.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            self.format_pattern(&p.pattern);
            self.write_str(": ");
            self.format_type_expr(&p.ty);
        }
        self.write_str(")");
        if let Some(ref rt) = e.return_type {
            self.write_str(" -> ");
            self.format_type_expr(rt);
        }
        self.format_effects(&e.effects);
        self.write_str(";\n");
    }

    pub(super) fn format_extern_block_item_opaque_type(&mut self, o: &OpaqueTypeDecl) {
        self.format_attributes(&o.attributes);
        self.write_indent();
        self.write_visibility(o.visibility());
        self.write_str("type ");
        self.write_ident(&o.name);
        self.write_str(";\n");
    }

    // ── Type Alias / Distinct ───────────────────────────────────

    pub(super) fn format_type_alias(&mut self, t: &TypeAliasDef) {
        self.write_indent();
        self.write_visibility(t.visibility());
        self.write_str("type ");
        self.write_ident(&t.name);
        self.format_generic_params(&t.generic_params);
        self.write_str(" = ");
        self.format_type_expr(&t.ty);
        self.write_str(";\n");
    }

    pub(super) fn format_distinct_type(&mut self, d: &DistinctTypeDef) {
        self.format_attributes(&d.attributes);
        self.write_indent();
        self.write_visibility(d.visibility());
        self.write_str("distinct type ");
        self.write_ident(&d.name);
        self.format_generic_params(&d.generic_params);
        self.write_str(" = ");
        self.format_type_expr(&d.base_type);
        self.write_str(";\n");
    }
}

// ── Helpers ─────────────────────────────────────────────────────

/// String-returning equivalent of `Formatter::write_ident`. Used when the
/// caller is composing output via `format!(...)` and can't take `&mut self`.
pub(super) fn ident_str(name: &str) -> String {
    if needs_raw_escape(name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// String-returning equivalent of `Formatter::write_path`.
pub(super) fn path_str(segments: &[String]) -> String {
    segments
        .iter()
        .map(|s| ident_str(s))
        .collect::<Vec<_>>()
        .join(".")
}

/// True iff emitting `name` bare would lex as a keyword / reserved-future-use
/// word (i.e. anything other than `Token::Identifier`). The list mirrors the
/// keyword table in `src/lexer.rs::identifier()` plus the reserved-future-use
/// set; structural markers (`self`/`Self`/`_`/...) are excluded because they
/// cannot reach the formatter as a plain `name` — the lexer rejects raw
/// escapes for them.
pub(super) fn needs_raw_escape(name: &str) -> bool {
    matches!(
        name,
        // Declarations
        "fn" | "struct" | "enum" | "trait" | "impl" | "mod" | "use" | "import"
        | "const" | "type" | "distinct"
        // Visibility
        | "pub" | "private"
        // Control flow
        | "if" | "else" | "match" | "while" | "for" | "in" | "loop"
        | "return" | "break" | "continue"
        | "defer" | "errdefer" | "asm" | "global_asm"
        // Bindings
        | "let" | "mut"
        // Logical (keyword forms)
        | "and" | "or" | "not"
        // Ownership
        | "own" | "ref" | "weak" | "lock" | "move"
        // Effects
        | "effect" | "resource" | "verb"
        | "reads" | "writes" | "sends" | "receives" | "allocates" | "panics"
        | "blocks" | "suspends"
        | "with" | "transparent" | "stable" | "seq" | "par" | "yield"
        // Type system
        | "as" | "where" | "dyn"
        // Contracts
        | "requires" | "ensures" | "invariant"
        // Safety
        | "unsafe" | "extern"
        // Shared / layout
        | "shared" | "layout" | "group"
        // Bool literals
        | "true" | "false"
        // Providers / misc
        | "providers" | "alias" | "independent"
        // Reserved-for-future-use numeric types
        | "f16" | "bf16"
        // Reserved-for-future-use keywords
        | "gen" | "become" | "do" | "final" | "override" | "priv" | "try"
        | "typeof" | "virtual" | "async" | "await" | "comptime" | "pure" | "box"
    )
}

pub(super) fn format_effect_verb_kind(v: &EffectVerbKind) -> String {
    match v {
        EffectVerbKind::Reads => "reads".to_string(),
        EffectVerbKind::Writes => "writes".to_string(),
        EffectVerbKind::Sends => "sends".to_string(),
        EffectVerbKind::Receives => "receives".to_string(),
        EffectVerbKind::Allocates => "allocates".to_string(),
        EffectVerbKind::Panics => "panics".to_string(),
        EffectVerbKind::Blocks => "blocks".to_string(),
        EffectVerbKind::Suspends => "suspends".to_string(),
        EffectVerbKind::UserDefined(s) => s.clone(),
    }
}

pub(super) fn impl_item_name(item: &ImplItem) -> &str {
    match item {
        ImplItem::Method(m) => &m.name,
        ImplItem::AssocType(a) => &a.name,
    }
}

pub(super) fn int_suffix_str(s: IntSuffix) -> &'static str {
    match s {
        IntSuffix::I8 => "i8",
        IntSuffix::I16 => "i16",
        IntSuffix::I32 => "i32",
        IntSuffix::I64 => "i64",
        IntSuffix::I128 => "i128",
        IntSuffix::U8 => "u8",
        IntSuffix::U16 => "u16",
        IntSuffix::U32 => "u32",
        IntSuffix::U64 => "u64",
        IntSuffix::U128 => "u128",
    }
}

pub(super) fn float_suffix_str(s: FloatSuffix) -> &'static str {
    match s {
        FloatSuffix::F32 => "f32",
        FloatSuffix::F64 => "f64",
    }
}

pub(super) fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::format_program;
    use crate::parse;

    fn fmt_ok(source: &str) -> String {
        let result = parse(source);
        assert!(
            result.errors.is_empty(),
            "parse errors: {:?}",
            result.errors
        );
        format_program(&result.program)
    }

    #[test]
    fn closure_ref_capture_mode_prefix_roundtrips() {
        let out = fmt_ok("fn main() { let f = ref |x| x + 1; }");
        assert!(
            out.contains("ref |x|"),
            "expected `ref |x|` in formatted output, got:\n{out}"
        );
    }

    #[test]
    fn closure_mut_ref_capture_mode_prefix_roundtrips() {
        let out = fmt_ok("fn main() { let f = mut ref |x| x + 1; }");
        assert!(
            out.contains("mut ref |x|"),
            "expected `mut ref |x|` in formatted output, got:\n{out}"
        );
    }

    #[test]
    fn closure_no_prefix_does_not_emit_capture_mode() {
        let out = fmt_ok("fn main() { let f = |x| x + 1; }");
        assert!(
            !out.contains("ref |") && !out.contains("mut ref |"),
            "bare closure must not emit a capture-mode prefix, got:\n{out}"
        );
    }
}
