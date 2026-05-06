//! Canonical formatter for Kāra source code.
//!
//! Parses AST and reprints in deterministic canonical form.
//! Scope: syntactic canonicalization (like gofmt/rustfmt), not semantic normalization.

use crate::ast::*;
use crate::token::{FloatSuffix, IntSuffix};
use std::fmt::Write;

const INDENT: &str = "    ";

pub fn format_program(program: &Program) -> String {
    let mut f = Formatter::new();
    f.format_program(program);
    f.output
}

struct Formatter {
    output: String,
    indent: usize,
}

impl Formatter {
    fn new() -> Self {
        Formatter {
            output: String::new(),
            indent: 0,
        }
    }

    fn push_indent(&mut self) {
        self.indent += 1;
    }

    fn pop_indent(&mut self) {
        self.indent -= 1;
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.push_str(INDENT);
        }
    }

    fn writeln(&mut self, s: &str) {
        self.write_indent();
        self.output.push_str(s);
        self.output.push('\n');
    }

    fn write_str(&mut self, s: &str) {
        self.output.push_str(s);
    }

    /// Emit an AST-level identifier name, prepending `r#` when the bare name
    /// would otherwise lex as a keyword or reserved-for-future-use word — i.e.
    /// when round-tripping requires the raw-identifier escape (design.md §
    /// Raw Identifiers). Structural markers (`self`/`Self`/`_`/etc.) are
    /// rejected at lex time and never reach the formatter as plain `name`s.
    fn write_ident(&mut self, name: &str) {
        if needs_raw_escape(name) {
            self.output.push_str("r#");
        }
        self.output.push_str(name);
    }

    /// Emit a dotted path, escaping each segment independently.
    fn write_path(&mut self, segments: &[String]) {
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                self.output.push('.');
            }
            self.write_ident(seg);
        }
    }

    /// Emit a `, `-separated list of identifiers, escaping each independently.
    fn write_ident_list(&mut self, names: &[String]) {
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.write_ident(n);
        }
    }

    /// Emit the visibility keyword (`pub ` / `private ` / `""`) for items
    /// that carry the three-level `Visibility`.
    fn write_visibility(&mut self, v: Visibility) {
        match v {
            Visibility::Pub => self.write_str("pub "),
            Visibility::Private => self.write_str("private "),
            Visibility::Default => {}
        }
    }

    // ── Program ─────────────────────────────────────────────────

    fn format_program(&mut self, program: &Program) {
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

    fn format_item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => self.format_function(f),
            Item::StructDef(s) => self.format_struct(s),
            Item::EnumDef(e) => self.format_enum(e),
            Item::TraitDef(t) => self.format_trait(t),
            Item::TraitAlias(t) => self.format_trait_alias(t),
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
            Item::TypeAlias(t) => self.format_type_alias(t),
            Item::DistinctType(d) => self.format_distinct_type(d),
        }
    }

    // ── Attributes ──────────────────────────────────────────────

    fn format_attributes(&mut self, attrs: &[Attribute]) {
        for attr in attrs {
            self.write_indent();
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

    fn format_function(&mut self, f: &Function) {
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

    fn format_fn_params(&mut self, self_param: &Option<SelfParam>, params: &[Param]) {
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

    fn format_effects(&mut self, effects: &Option<EffectList>) {
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

    fn format_requires(&mut self, requires: &[Expr]) {
        for r in requires {
            self.write_str("\n");
            self.write_indent();
            self.write_str("    requires ");
            self.format_expr(r);
        }
    }

    fn format_ensures(&mut self, ensures: &[EnsuresClause]) {
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

    fn format_where_clause(&mut self, wc: &Option<WhereClause>) {
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
            }
        }
    }

    // ── Structs ─────────────────────────────────────────────────

    fn format_struct(&mut self, s: &StructDef) {
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

    fn format_enum(&mut self, e: &EnumDef) {
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

    fn format_trait_alias(&mut self, t: &TraitAliasDef) {
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

    fn format_trait(&mut self, t: &TraitDef) {
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

    fn format_trait_method(&mut self, m: &TraitMethod) {
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

    fn format_impl(&mut self, imp: &ImplBlock) {
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

    fn format_effect_resource(&mut self, e: &EffectResourceDecl) {
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

    fn format_effect_group(&mut self, e: &EffectGroupDecl) {
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

    fn format_effect_verb_decl(&mut self, e: &EffectVerbDecl) {
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

    fn format_layout(&mut self, l: &LayoutDef) {
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

    fn format_const(&mut self, c: &ConstDecl) {
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

    fn format_extern_fn(&mut self, e: &ExternFunction) {
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

    // ── Type Alias / Distinct ───────────────────────────────────

    fn format_type_alias(&mut self, t: &TypeAliasDef) {
        self.write_indent();
        self.write_visibility(t.visibility());
        self.write_str("type ");
        self.write_ident(&t.name);
        self.format_generic_params(&t.generic_params);
        self.write_str(" = ");
        self.format_type_expr(&t.ty);
        self.write_str(";\n");
    }

    fn format_distinct_type(&mut self, d: &DistinctTypeDef) {
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

    // ── Generics ────────────────────────────────────────────────

    fn format_generic_params(&mut self, gp: &Option<GenericParams>) {
        let gp = match gp {
            Some(g) => g,
            None => return,
        };
        self.write_str("[");
        let mut first = true;
        for p in &gp.params {
            if !first {
                self.write_str(", ");
            }
            first = false;
            if p.is_const {
                self.write_str("const ");
            }
            self.write_ident(&p.name);
            if let Some(ref ct) = p.const_type {
                self.write_str(": ");
                self.format_type_expr(ct);
            } else if !p.bounds.is_empty() {
                self.write_str(": ");
                for (i, b) in p.bounds.iter().enumerate() {
                    if i > 0 {
                        self.write_str(" + ");
                    }
                    self.write_path(&b.path);
                    self.format_generic_args_opt(&b.generic_args);
                }
            }
        }
        for ep in &gp.effect_params {
            if !first {
                self.write_str(", ");
            }
            first = false;
            self.write_str("effect ");
            self.write_ident(ep);
        }
        self.write_str("]");
    }

    fn format_generic_args_opt(&mut self, args: &Option<Vec<GenericArg>>) {
        let args = match args {
            Some(a) => a,
            None => return,
        };
        self.write_str("[");
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            match arg {
                GenericArg::Type(t) => self.format_type_expr(t),
                GenericArg::Const(e) => self.format_expr(e),
            }
        }
        self.write_str("]");
    }

    // ── Types ───────────────────────────────────────────────────

    fn format_type_expr(&mut self, ty: &TypeExpr) {
        match &ty.kind {
            TypeKind::Path(p) => {
                self.write_path(&p.segments);
                self.format_generic_args_opt(&p.generic_args);
            }
            TypeKind::Tuple(types) => {
                self.write_str("(");
                for (i, t) in types.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_type_expr(t);
                }
                self.write_str(")");
            }
            TypeKind::Array { element, size } => {
                self.write_str("[");
                self.format_type_expr(element);
                self.write_str("; ");
                self.format_expr(size);
                self.write_str("]");
            }
            TypeKind::Pointer { is_mut, inner } => {
                if *is_mut {
                    self.write_str("*mut ");
                } else {
                    self.write_str("*");
                }
                self.format_type_expr(inner);
            }
            TypeKind::FnType {
                params,
                return_type,
                ..
            } => {
                self.write_str("fn(");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_type_expr(p);
                }
                self.write_str(")");
                if let Some(ref rt) = return_type {
                    self.write_str(" -> ");
                    self.format_type_expr(rt);
                }
            }
            TypeKind::Ref(inner) => {
                self.write_str("ref ");
                self.format_type_expr(inner);
            }
            TypeKind::MutRef(inner) => {
                self.write_str("mut ref ");
                self.format_type_expr(inner);
            }
            TypeKind::MutSlice(element) => {
                self.write_str("mut Slice[");
                self.format_type_expr(element);
                self.write_str("]");
            }
            TypeKind::Weak(inner) => {
                self.write_str("weak ");
                self.format_type_expr(inner);
            }
            TypeKind::Unit => self.write_str("()"),
            TypeKind::Error => self.write_str("/* error */"),
        }
    }

    // ── Blocks ──────────────────────────────────────────────────

    fn format_block(&mut self, block: &Block) {
        if block.stmts.is_empty() && block.final_expr.is_none() {
            self.write_str("{}");
            return;
        }
        self.write_str("{\n");
        self.push_indent();
        for stmt in &block.stmts {
            self.format_stmt(stmt);
        }
        if let Some(ref expr) = block.final_expr {
            self.write_indent();
            self.format_expr(expr);
            self.output.push('\n');
        }
        self.pop_indent();
        self.write_indent();
        self.write_str("}");
    }

    // ── Statements ──────────────────────────────────────────────

    fn format_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                is_mut,
                pattern,
                ty,
                value,
            } => {
                self.write_indent();
                self.write_str("let ");
                if *is_mut {
                    self.write_str("mut ");
                }
                self.format_pattern(pattern);
                if let Some(ref t) = ty {
                    self.write_str(": ");
                    self.format_type_expr(t);
                }
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(";\n");
            }
            StmtKind::LetUninit {
                is_mut, name, ty, ..
            } => {
                self.write_indent();
                self.write_str("let ");
                if *is_mut {
                    self.write_str("mut ");
                }
                self.write_ident(name);
                self.write_str(": ");
                self.format_type_expr(ty);
                self.write_str(";\n");
            }
            StmtKind::LetElse {
                pattern,
                ty,
                value,
                else_block,
            } => {
                self.write_indent();
                self.write_str("let ");
                self.format_pattern(pattern);
                if let Some(ref t) = ty {
                    self.write_str(": ");
                    self.format_type_expr(t);
                }
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(" else ");
                self.format_block(else_block);
                self.write_str(";\n");
            }
            StmtKind::Defer { body } => {
                self.write_indent();
                self.write_str("defer ");
                self.format_block(body);
                self.output.push('\n');
            }
            StmtKind::ErrDefer { binding, body } => {
                self.write_indent();
                self.write_str("errdefer");
                if let Some(ref b) = binding {
                    self.write_str("(");
                    self.write_ident(b);
                    self.write_str(")");
                }
                self.write_str(" ");
                self.format_block(body);
                self.output.push('\n');
            }
            StmtKind::Assign { target, value } => {
                self.write_indent();
                self.format_expr(target);
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(";\n");
            }
            StmtKind::CompoundAssign { target, op, value } => {
                self.write_indent();
                self.format_expr(target);
                self.write_str(match op {
                    CompoundOp::Add => " += ",
                    CompoundOp::Sub => " -= ",
                    CompoundOp::Mul => " *= ",
                    CompoundOp::Div => " /= ",
                    CompoundOp::Mod => " %= ",
                    CompoundOp::BitAnd => " &= ",
                    CompoundOp::BitOr => " |= ",
                    CompoundOp::BitXor => " ^= ",
                    CompoundOp::Shl => " <<= ",
                    CompoundOp::Shr => " >>= ",
                });
                self.format_expr(value);
                self.write_str(";\n");
            }
            StmtKind::Expr(expr) => {
                self.write_indent();
                self.format_expr(expr);
                self.write_str(";\n");
            }
        }
    }

    // ── Expressions ─────────────────────────────────────────────

    fn format_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Integer(n, sfx) => {
                write!(self.output, "{n}").unwrap();
                if let Some(s) = sfx {
                    self.write_str(int_suffix_str(*s));
                }
            }
            ExprKind::Float(f, sfx) => {
                write!(self.output, "{f}").unwrap();
                if let Some(s) = sfx {
                    self.write_str(float_suffix_str(*s));
                }
            }
            ExprKind::CharLit(c) => write!(self.output, "'{c}'").unwrap(),
            ExprKind::StringLit(s) => {
                self.write_str("\"");
                self.write_str(&escape_string(s));
                self.write_str("\"");
            }
            ExprKind::MultiStringLit(s) => {
                // Multi-line strings keep their format
                self.write_str("\"\"\"");
                self.write_str(s);
                self.write_str("\"\"\"");
            }
            ExprKind::InterpolatedStringLit(parts) => {
                self.write_str("f\"");
                for part in parts {
                    match part {
                        crate::ast::ParsedInterpolationPart::Text(s) => {
                            self.write_str(&escape_string(s))
                        }
                        crate::ast::ParsedInterpolationPart::Expr(e) => {
                            self.write_str("{");
                            self.format_expr(e);
                            self.write_str("}");
                        }
                    }
                }
                self.write_str("\"");
            }
            ExprKind::Bool(b) => self.write_str(if *b { "true" } else { "false" }),
            ExprKind::Identifier(name) => self.write_ident(name),
            ExprKind::Path(segments) => self.write_path(segments),
            ExprKind::SelfValue => self.write_str("self"),
            ExprKind::SelfType => self.write_str("Self"),

            ExprKind::Binary { op, left, right } => {
                self.format_expr(left);
                self.write_str(match op {
                    BinOp::Add => " + ",
                    BinOp::Sub => " - ",
                    BinOp::Mul => " * ",
                    BinOp::Div => " / ",
                    BinOp::Mod => " % ",
                    BinOp::Eq => " == ",
                    BinOp::NotEq => " != ",
                    BinOp::Lt => " < ",
                    BinOp::LtEq => " <= ",
                    BinOp::Gt => " > ",
                    BinOp::GtEq => " >= ",
                    BinOp::And => " and ",
                    BinOp::Or => " or ",
                    BinOp::BitAnd => " & ",
                    BinOp::BitOr => " | ",
                    BinOp::BitXor => " ^ ",
                    BinOp::Shl => " << ",
                    BinOp::Shr => " >> ",
                    BinOp::Range => "..",
                    BinOp::RangeInclusive => "..=",
                });
                self.format_expr(right);
            }
            ExprKind::Unary { op, operand } => {
                self.write_str(match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "not ",
                    UnaryOp::BitNot => "~",
                    UnaryOp::Deref => "*",
                });
                self.format_expr(operand);
            }
            ExprKind::Question(inner) => {
                self.format_expr(inner);
                self.write_str("?");
            }
            ExprKind::OptionalChain {
                object,
                field_or_method,
                args,
            } => {
                self.format_expr(object);
                self.write_str("?.");
                self.write_ident(field_or_method);
                if let Some(ref a) = args {
                    self.write_str("(");
                    self.format_call_args(a);
                    self.write_str(")");
                }
            }
            ExprKind::NilCoalesce { left, right } => {
                self.format_expr(left);
                self.write_str(" ?? ");
                self.format_expr(right);
            }
            ExprKind::Call { callee, args } => {
                self.format_expr(callee);
                self.write_str("(");
                self.format_call_args(args);
                self.write_str(")");
            }
            ExprKind::MethodCall {
                object,
                method,
                turbofish,
                args,
            } => {
                self.format_expr(object);
                self.write_str(".");
                self.write_ident(method);
                if let Some(ref tf) = turbofish {
                    self.write_str("[");
                    for (i, t) in tf.iter().enumerate() {
                        if i > 0 {
                            self.write_str(", ");
                        }
                        self.format_type_expr(t);
                    }
                    self.write_str("]");
                }
                self.write_str("(");
                self.format_call_args(args);
                self.write_str(")");
            }
            ExprKind::FieldAccess { object, field } => {
                self.format_expr(object);
                self.write_str(".");
                self.write_ident(field);
            }
            ExprKind::TupleIndex { object, index } => {
                self.format_expr(object);
                write!(self.output, ".{index}").unwrap();
            }
            ExprKind::Index { object, index } => {
                self.format_expr(object);
                self.write_str("[");
                self.format_expr(index);
                self.write_str("]");
            }
            ExprKind::Block(block) => self.format_block(block),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.write_str("if ");
                self.format_expr(condition);
                self.write_str(" ");
                self.format_block(then_block);
                if let Some(ref eb) = else_branch {
                    self.write_str(" else ");
                    match &eb.kind {
                        ExprKind::If { .. } | ExprKind::IfLet { .. } => self.format_expr(eb),
                        ExprKind::Block(block) => self.format_block(block),
                        _ => self.format_expr(eb),
                    }
                }
            }
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.write_str("if let ");
                self.format_pattern(pattern);
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(" ");
                self.format_block(then_block);
                if let Some(ref eb) = else_branch {
                    self.write_str(" else ");
                    match &eb.kind {
                        ExprKind::Block(block) => self.format_block(block),
                        _ => self.format_expr(eb),
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.write_str("match ");
                self.format_expr(scrutinee);
                self.write_str(" {\n");
                self.push_indent();
                for arm in arms {
                    self.write_indent();
                    self.format_pattern(&arm.pattern);
                    if let Some(ref guard) = arm.guard {
                        self.write_str(" if ");
                        self.format_expr(guard);
                    }
                    self.write_str(" => ");
                    self.format_expr(&arm.body);
                    self.write_str(",\n");
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("}");
            }
            ExprKind::While {
                label,
                condition,
                body,
            } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("while ");
                self.format_expr(condition);
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::WhileLet {
                label,
                pattern,
                value,
                body,
            } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("while let ");
                self.format_pattern(pattern);
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::For {
                label,
                pattern,
                iterable,
                body,
            } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("for ");
                self.format_pattern(pattern);
                self.write_str(" in ");
                self.format_expr(iterable);
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::Loop { label, body } => {
                if let Some(ref l) = label {
                    write!(self.output, "'{l}: ").unwrap();
                }
                self.write_str("loop ");
                self.format_block(body);
            }
            ExprKind::Closure {
                params,
                capture_mode,
                prefix_span: _,
                body,
            } => {
                match capture_mode {
                    Some(CaptureMode::Own) => self.write_str("own "),
                    Some(CaptureMode::Ref) => self.write_str("ref "),
                    Some(CaptureMode::MutRef) => self.write_str("mut ref "),
                    None => {}
                }
                self.write_str("|");
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_pattern(&p.pattern);
                    if let Some(ref t) = p.ty {
                        self.write_str(": ");
                        self.format_type_expr(t);
                    }
                }
                self.write_str("| ");
                self.format_expr(body);
            }
            ExprKind::Return(val) => {
                self.write_str("return");
                if let Some(ref v) = val {
                    self.write_str(" ");
                    self.format_expr(v);
                }
            }
            ExprKind::Break { label, value } => {
                self.write_str("break");
                if let Some(ref l) = label {
                    write!(self.output, " '{l}").unwrap();
                }
                if let Some(ref v) = value {
                    self.write_str(" ");
                    self.format_expr(v);
                }
            }
            ExprKind::Continue { label } => {
                self.write_str("continue");
                if let Some(ref l) = label {
                    write!(self.output, " '{l}").unwrap();
                }
            }
            ExprKind::Tuple(elems) => {
                self.write_str("(");
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_expr(e);
                }
                if elems.len() == 1 {
                    self.write_str(",");
                }
                self.write_str(")");
            }
            ExprKind::ArrayLiteral(elems) => {
                self.write_str("[");
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_expr(e);
                }
                self.write_str("]");
            }
            ExprKind::PrefixCollectionLiteral { type_name, items } => {
                self.write_str(type_name);
                self.write_str("[");
                for (i, e) in items.iter().enumerate() {
                    if i > 0 {
                        self.write_str(", ");
                    }
                    self.format_expr(e);
                }
                self.write_str("]");
            }
            ExprKind::RepeatLiteral {
                type_name,
                value,
                count,
            } => {
                if let Some(name) = type_name {
                    self.write_ident(name);
                }
                self.write_str("[");
                self.format_expr(value);
                self.write_str("; ");
                self.format_expr(count);
                self.write_str("]");
            }
            ExprKind::MapLiteral(entries) => {
                if entries.is_empty() {
                    self.write_str("{:}");
                    return;
                }
                self.write_str("{\n");
                self.push_indent();
                for (k, v) in entries {
                    self.write_indent();
                    self.format_expr(k);
                    self.write_str(": ");
                    self.format_expr(v);
                    self.write_str(",\n");
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("}");
            }
            ExprKind::StructLiteral {
                path,
                fields,
                spread,
            } => {
                self.write_path(path);
                self.write_str(" {\n");
                self.push_indent();
                for fi in fields {
                    self.write_indent();
                    if fi.shorthand {
                        self.write_ident(&fi.name);
                    } else {
                        self.write_ident(&fi.name);
                        self.write_str(": ");
                        self.format_expr(&fi.value);
                    }
                    self.write_str(",\n");
                }
                if let Some(ref s) = spread {
                    self.write_indent();
                    self.write_str("..");
                    self.format_expr(s);
                    self.output.push('\n');
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("}");
            }
            ExprKind::Pipe { left, right } => {
                self.format_expr(left);
                self.write_str(" |> ");
                self.format_expr(right);
            }
            ExprKind::PipePlaceholder => self.write_str("_"),
            ExprKind::Cast { expr, ty } => {
                self.format_expr(expr);
                self.write_str(" as ");
                self.format_type_expr(ty);
            }
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                if let Some(s) = start {
                    self.format_expr(s);
                }
                if *inclusive {
                    self.write_str("..=");
                } else {
                    self.write_str("..");
                }
                if let Some(e) = end {
                    self.format_expr(e);
                }
            }
            ExprKind::Unsafe(block) => {
                self.write_str("unsafe ");
                self.format_block(block);
            }
            ExprKind::Try(block) => {
                self.write_str("try ");
                self.format_block(block);
            }
            ExprKind::Seq(block) => {
                self.write_str("seq ");
                self.format_block(block);
            }
            ExprKind::Par(block) => {
                self.write_str("par ");
                self.format_block(block);
            }
            ExprKind::Lock { mutex, alias, body } => {
                self.write_str("lock ");
                self.write_str(mutex);
                if let Some(ref a) = alias {
                    self.write_str(" as ");
                    self.write_str(a);
                }
                self.write_str(" ");
                self.format_block(body);
            }
            ExprKind::Providers { bindings, body } => {
                self.write_str("providers {\n");
                self.push_indent();
                for b in bindings {
                    self.write_indent();
                    self.write_ident(&b.resource);
                    self.write_str(" => ");
                    self.format_expr(&b.value);
                    self.write_str(",\n");
                }
                self.pop_indent();
                self.write_indent();
                self.write_str("} in ");
                self.format_block(body);
            }
            ExprKind::Error => self.write_str("/* error */"),
        }
    }

    fn format_call_args(&mut self, args: &[CallArg]) {
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            if let Some(ref label) = arg.label {
                self.write_ident(label);
                self.write_str(": ");
            }
            if arg.mut_marker {
                self.write_str("mut ");
            }
            self.format_expr(&arg.value);
        }
    }

    // ── Patterns ────────────────────────────────────────────────

    fn format_pattern(&mut self, pat: &Pattern) {
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
        }
    }

    fn format_literal_pattern(&mut self, lit: &LiteralPattern) {
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

// ── Helpers ─────────────────────────────────────────────────────

/// String-returning equivalent of `Formatter::write_ident`. Used when the
/// caller is composing output via `format!(...)` and can't take `&mut self`.
fn ident_str(name: &str) -> String {
    if needs_raw_escape(name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// String-returning equivalent of `Formatter::write_path`.
fn path_str(segments: &[String]) -> String {
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
fn needs_raw_escape(name: &str) -> bool {
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

fn format_effect_verb_kind(v: &EffectVerbKind) -> String {
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

fn impl_item_name(item: &ImplItem) -> &str {
    match item {
        ImplItem::Method(m) => &m.name,
        ImplItem::AssocType(a) => &a.name,
    }
}

fn int_suffix_str(s: IntSuffix) -> &'static str {
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

fn float_suffix_str(s: FloatSuffix) -> &'static str {
    match s {
        FloatSuffix::F32 => "f32",
        FloatSuffix::F64 => "f64",
    }
}

fn escape_string(s: &str) -> String {
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
