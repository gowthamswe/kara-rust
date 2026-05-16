//! REPL helper utilities — cell classification, source-level parsing
//! tweaks, item-name extraction, brace-balance probe, etc. These are
//! free functions kept out of `Session` so the impl block stays focused
//! on the read-eval-print loop.

/// Rewrite the consume-site of `binding` inside a cell-history entry so
/// `:save` exports the auto-clone'd form. The rewrite is positional: find
/// the first occurrence of the bare identifier `binding` whose immediate
/// suffix is *not* already `.clone()` (idempotent w.r.t. repeat
/// rewrites — the second auto-clone for the same site is a no-op), and
/// splice `.clone()` after it. Matches identifier boundaries so that
/// substring overlap (e.g. `binding="s"` against `consumed`) doesn't trip
/// the search. Idempotent under repeat invocations: a cell that already
/// reads `consume(s.clone())` is returned unchanged.
pub(super) fn rewrite_cell_history_consume(cell_src: &mut String, binding: &str) {
    if binding.is_empty() {
        return;
    }
    let bytes = cell_src.as_bytes();
    let blen = binding.len();
    let mut i = 0usize;
    while i + blen <= bytes.len() {
        // Skip past string-literal contents — a binding name embedded in
        // a string isn't a use-site. Cheap coarse-tracking matches the
        // probe in `is_balanced` (good enough for the v1 source-replay
        // model's input).
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i = i.saturating_add(1);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        // Identifier-boundary match: the byte before `binding` must not
        // be an identifier continuation; ditto the byte after.
        let prev_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
        let after = i + blen;
        let next_ok = after >= bytes.len() || !is_ident_continue(bytes[after]);
        if prev_ok && next_ok && &bytes[i..after] == binding.as_bytes() {
            // Already-cloned guard — skip if `.clone(` immediately
            // follows the identifier.
            let already_cloned = bytes[after..].starts_with(b".clone(");
            if !already_cloned {
                cell_src.insert_str(after, ".clone()");
                return;
            }
            i = after + ".clone()".len();
            continue;
        }
        i += 1;
    }
}

pub(super) fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// One-line preview of a cell's source for the notebook-aware
/// `UseAfterMove` tail. Trims whitespace, collapses interior newlines to
/// spaces, and truncates with an ellipsis past `MAX` chars so a long
/// multi-line cell doesn't blow out the diagnostic. Returned form is safe
/// to embed in backticks: any embedded backticks are replaced with single
/// quotes so the surrounding code-fence stays well-formed.
pub(super) fn cell_preview(src: &str) -> String {
    const MAX: usize = 60;
    let mut out = String::with_capacity(src.len().min(MAX + 1));
    let mut last_was_space = true;
    for ch in src.chars() {
        let mapped = match ch {
            '\n' | '\r' | '\t' => ' ',
            '`' => '\'',
            c => c,
        };
        if mapped == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        if out.chars().count() >= MAX {
            out.push('…');
            break;
        }
        out.push(mapped);
    }
    let trimmed = out.trim().to_string();
    trimmed
}

/// Minimal TOML basic-string escaper. Only the characters that need
/// escaping inside a `"..."` literal are touched; everything else passes
/// through. Used to round-trip a `:dep` version string through the stored
/// representation without depending on `toml`'s serializer.
pub(super) fn toml_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                write!(out, "\\u{:04X}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out
}

pub(super) fn print_help() {
    println!(
        "Kāra REPL meta-commands:
  :help              show this help
  :quit / :q         exit the REPL
  :type <expr>       print the inferred type of <expr>
  :effects           print effects accumulated by session items
  :save <file.kara>  write the cell history to <file.kara>
  :reset             clear persistent `let` bindings (items + history kept)
  :dep <name> = ...  register a dependency for the current session
                     (RHS is the same shape as `[dependencies]` in
                     `kara.toml`: a quoted version string or an inline
                     table). Resolution lands in v1.1; v1 records the
                     entry but the package's symbols are NOT yet in scope.

Cell semantics (v1 MVP):
  - Top-level items (fn, struct, enum, trait, impl, const, type, distinct)
    accumulate across cells. A later definition shadows an earlier one with
    the same name.
  - Each statement-style cell runs as the body of an implicit `fn main()`,
    so `?` is legal.
  - `let` / `let mut` bindings persist across statement cells: cell N's
    `let x = 5;` is in scope when cell N+1 evaluates `println(x);`. v1
    uses source-replay — the RHS re-runs each cell — so side-effecting
    bindings (`let log = read_file(...)`) recompute every time. Use
    `:reset` to drop the replay buffer if it gets expensive.
  - Mutation in a statement cell (`x += 1;`) does NOT survive to the
    next cell. Re-bind with `let x = x + 1;` to thread updated values.
  - Re-declaring an item in a later cell (`fn f` in cell 5 after `fn f`
    in cell 2; same for struct / enum / trait / const / type alias /
    distinct type / layout / effect group) shadows the earlier
    definition — the prior decl is pruned from the items buffer before
    the new one is appended. `impl` blocks are anonymous and are never
    pruned (multiple impls for the same target type compose as expected)."
    );
}

/// Cell input shape. The parser only accepts top-level items, so any
/// cell that begins with a statement (raw expression, `let`, etc.) needs
/// the synthetic `fn main()` wrapper to parse at all.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CellShape {
    /// Every non-blank, non-comment line begins with one of the item
    /// keywords (`fn`, `pub`, `private`, `struct`, `enum`, `trait`,
    /// `impl`, `const`, `type`, `distinct`, `extern`, `use`, `import`,
    /// `layout`, `effect`, `mod`, `#[...]` attribute prefix).
    PureItems,
    /// Any other shape — bare expressions, `let`s, `return`s, mixed
    /// statements + items. Routed through the wrapper.
    Statements,
}

pub(super) fn classify_input(src: &str) -> CellShape {
    // Item-starter keywords. `pub` / `private` are visibility modifiers
    // that always precede an item. `#` introduces an attribute that
    // attaches to the next item.
    const ITEM_PREFIXES: &[&str] = &[
        "fn ",
        "fn(",
        "pub ",
        "private ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "impl<",
        "impl[",
        "const ",
        "type ",
        "distinct ",
        "extern ",
        "use ",
        "import ",
        "layout ",
        "effect ",
        "mod ",
        "#",
    ];
    for raw_line in src.lines() {
        let line = raw_line.trim_start();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("//") || line.starts_with("/*") {
            continue;
        }
        // Continuation of a prior item's body — `}` closing a brace block
        // is permitted in pure-items mode. Same for trailing `,` etc. We
        // approximate by treating any line that is purely closing punctuation
        // or that begins with a bracket-closer as item-continuation.
        if line.starts_with('}') || line.starts_with(')') || line.starts_with(']') {
            continue;
        }
        // Field / variant / arm bodies inside an item — punted to the
        // pessimistic side: any line we can't classify as item-prefixed
        // forces statements mode. To keep the heuristic tight enough that
        // real item bodies aren't misclassified, we additionally accept
        // lines that look like field declarations (`name: Type,`) and
        // variant declarations (`Name,` / `Name { ... },`).
        if ITEM_PREFIXES.iter().any(|p| line.starts_with(p)) {
            continue;
        }
        if looks_like_item_body_line(line) {
            continue;
        }
        return CellShape::Statements;
    }
    // All non-blank lines were item-shaped (or item-body lines).
    CellShape::PureItems
}

/// Heuristic: a line that lives inside an item body but doesn't itself
/// start an item. Recognizes struct field declarations, enum variants,
/// trait method headers, and so on. Conservative — when in doubt, returns
/// `false` and the cell falls into statements mode.
pub(super) fn looks_like_item_body_line(line: &str) -> bool {
    // "name: Type," / "name: Type" — struct field.
    // "Name," / "Name { ... }," / "Name(...)," — enum variant.
    // "fn name(...) -> ...;" — trait method header (no body).
    // We accept any line that contains a `:` before any `=` (rules out
    // `let x: T = 5;`-style statements; `let` statements always start with
    // `let`, which `classify_input` already rejects).
    if line.starts_with("let ") || line.starts_with("return ") {
        return false;
    }
    // Pure punctuation / comment trailers.
    if line
        .chars()
        .all(|c| c.is_whitespace() || c == ',' || c == ';' || c == '{' || c == '}')
    {
        return true;
    }
    // Field / variant / where-clause / attribute body line — accept if
    // the line is identifier-ish followed by `:` or comma-separated.
    if line.contains(':') && !line.contains("=") {
        return true;
    }
    // Bare variant name (possibly with payload).
    let first = line
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next();
    if let Some(name) = first {
        if let Some(c) = name.chars().next() {
            // Heuristic: identifiers starting uppercase look like enum
            // variants when they appear at body-line position.
            if c.is_ascii_uppercase() {
                return true;
            }
        }
    }
    false
}

/// Extract the bare name of a named top-level item. Anonymous items
/// (`impl`, `use`, `import`, `independent`) return `None` — they have no
/// shadowable identity, so the prune loop should leave them alone.
pub(super) fn item_name(item: &crate::ast::Item) -> Option<&str> {
    use crate::ast::Item;
    match item {
        Item::Function(f) => Some(&f.name),
        Item::StructDef(s) => Some(&s.name),
        Item::EnumDef(e) => Some(&e.name),
        Item::TraitDef(t) => Some(&t.name),
        Item::TraitAlias(t) => Some(&t.name),
        Item::MarkerTrait(t) => Some(&t.name),
        Item::ConstDecl(c) => Some(&c.name),
        Item::ExternFunction(f) => Some(&f.name),
        Item::TypeAlias(t) => Some(&t.name),
        Item::DistinctType(d) => Some(&d.name),
        Item::LayoutDef(l) => Some(&l.name),
        Item::EffectResource(r) => Some(&r.name),
        Item::EffectGroup(g) => Some(&g.name),
        Item::EffectVerbDecl(v) => Some(&v.verb_name),
        // `AliasDecl` is an alias *declaration* (`alias L = R`), not a
        // type alias — its identity is the LHS name pair, not a single
        // shadowable identifier. Treat as anonymous for shadow purposes.
        // `ExternBlock` groups N items under one trust-boundary header;
        // there is no single shadowable identifier — treat as anonymous.
        Item::ImplBlock(_)
        | Item::UseDecl(_)
        | Item::Import(_)
        | Item::IndependentDecl(_)
        | Item::ExternBlock(_)
        | Item::AliasDecl(_) => None,
    }
}

/// Recover the parser-recorded span for any item kind so the prune loop
/// can identify byte ranges in the source buffer. The span starts after
/// any doc comments / attributes / visibility keyword (the parser eats
/// those before recording `start = current_span()`); the prune loop
/// recovers the leading bytes by attaching them to the *next* item.
pub(super) fn item_span(item: &crate::ast::Item) -> &crate::token::Span {
    use crate::ast::Item;
    match item {
        Item::Function(f) => &f.span,
        Item::StructDef(s) => &s.span,
        Item::EnumDef(e) => &e.span,
        Item::TraitDef(t) => &t.span,
        Item::TraitAlias(t) => &t.span,
        Item::MarkerTrait(t) => &t.span,
        Item::ConstDecl(c) => &c.span,
        Item::ExternFunction(f) => &f.span,
        Item::TypeAlias(t) => &t.span,
        Item::DistinctType(d) => &d.span,
        Item::LayoutDef(l) => &l.span,
        Item::EffectResource(r) => &r.span,
        Item::EffectGroup(g) => &g.span,
        Item::EffectVerbDecl(v) => &v.span,
        Item::AliasDecl(a) => &a.span,
        Item::ImplBlock(b) => &b.span,
        Item::UseDecl(u) => &u.span,
        Item::Import(i) => &i.span,
        Item::IndependentDecl(d) => &d.span,
        Item::ExternBlock(b) => &b.span,
    }
}

/// Parse a snippet of top-level item source and return the set of names
/// it declares (functions, structs, enums, traits, consts, type aliases,
/// distinct newtypes, layouts, effect groups/resources/verbs, aliases).
/// Anonymous items contribute nothing. Empty set if the snippet does not
/// parse — the production pipeline will surface that error elsewhere.
pub(super) fn collect_top_level_item_names(src: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let parsed = crate::parse(src);
    if !parsed.errors.is_empty() {
        return names;
    }
    for item in &parsed.program.items {
        if let Some(n) = item_name(item) {
            names.insert(n.to_string());
        }
    }
    names
}

/// One captured `let` source slice plus the names it binds.
pub(super) struct CellLet {
    pub(super) slice: String,
    pub(super) names: std::collections::HashSet<String>,
}

/// Parse a cell body inside a synthetic `fn __cell()` wrapper and return
/// each top-level `let` / `let ... else` statement: its raw source slice
/// plus the names its pattern binds. Returns an empty list if the cell
/// fails to parse (the production pipeline will surface that error
/// elsewhere; this helper just declines to capture).
pub(super) fn scan_top_level_lets(cell_src: &str) -> Vec<CellLet> {
    use crate::ast::{Item, StmtKind};
    let mut wrapper = String::with_capacity(cell_src.len() + 24);
    wrapper.push_str("fn __cell() {\n");
    let body_offset = wrapper.len();
    wrapper.push_str(cell_src);
    if !cell_src.ends_with('\n') {
        wrapper.push('\n');
    }
    wrapper.push_str("}\n");
    let parsed = crate::parse(&wrapper);
    let mut out = Vec::new();
    if !parsed.errors.is_empty() {
        return out;
    }
    for item in &parsed.program.items {
        let Item::Function(f) = item else { continue };
        if f.name != "__cell" {
            continue;
        }
        for stmt in &f.body.stmts {
            let pattern = match &stmt.kind {
                StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => pattern,
                _ => continue,
            };
            let start = stmt.span.offset;
            let end = start.saturating_add(stmt.span.length);
            if start < body_offset || end > wrapper.len() || start >= end {
                continue;
            }
            let mut names = std::collections::HashSet::new();
            collect_pattern_bindings(&pattern.kind, &mut names);
            out.push(CellLet {
                slice: wrapper[start..end].to_string(),
                names,
            });
        }
        break;
    }
    out
}

/// Walk a pattern, collecting every value-binding name. Used to drop
/// prior persistent `let`s when a new cell shadows the same name.
pub(super) fn collect_pattern_bindings(
    pat: &crate::ast::PatternKind,
    out: &mut std::collections::HashSet<String>,
) {
    use crate::ast::PatternKind;
    match pat {
        PatternKind::Binding(name) => {
            out.insert(name.clone());
        }
        PatternKind::Tuple(parts) => {
            for p in parts {
                collect_pattern_bindings(&p.kind, out);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for f in fields {
                if let Some(ref p) = f.pattern {
                    collect_pattern_bindings(&p.kind, out);
                } else {
                    // Shorthand `Foo { x }` binds `x` directly.
                    out.insert(f.name.clone());
                }
            }
        }
        PatternKind::TupleVariant { patterns, .. } => {
            for p in patterns {
                collect_pattern_bindings(&p.kind, out);
            }
        }
        PatternKind::AtBinding { name, pattern } => {
            out.insert(name.clone());
            collect_pattern_bindings(&pattern.kind, out);
        }
        PatternKind::Or(alternatives) => {
            // Every alternative binds the same set of names per the
            // language's or-pattern rule; pick the first.
            if let Some(first) = alternatives.first() {
                collect_pattern_bindings(&first.kind, out);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for p in prefix.iter().chain(suffix.iter()) {
                collect_pattern_bindings(&p.kind, out);
            }
            if let Some(crate::ast::RestPattern::Bound(name)) = rest {
                out.insert(name.clone());
            }
        }
        // Leaves: no bindings.
        PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
    }
}

/// Re-parse a stored `let` source slice to recover the names it binds.
/// Used for shadow-pruning — when a new cell binds `x`, we drop every
/// prior entry whose pattern also binds `x`.
pub(super) fn parse_let_binding_names(let_src: &str) -> std::collections::HashSet<String> {
    use crate::ast::{Item, StmtKind};
    let mut wrapper = String::with_capacity(let_src.len() + 24);
    wrapper.push_str("fn __probe() {\n");
    wrapper.push_str(let_src);
    if !let_src.ends_with('\n') {
        wrapper.push('\n');
    }
    wrapper.push_str("}\n");
    let parsed = crate::parse(&wrapper);
    let mut names = std::collections::HashSet::new();
    if !parsed.errors.is_empty() {
        return names;
    }
    for item in &parsed.program.items {
        let Item::Function(f) = item else { continue };
        if f.name != "__probe" {
            continue;
        }
        for stmt in &f.body.stmts {
            let pattern = match &stmt.kind {
                StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => pattern,
                _ => continue,
            };
            collect_pattern_bindings(&pattern.kind, &mut names);
        }
    }
    names
}

pub(super) fn strip_main(src: &str) -> String {
    // Remove every `fn main(...) { ... }` definition from `src` so the
    // synthetic wrapper can install its own entry point. Uses brace
    // balancing — string-literal escaping is approximate but sufficient
    // for the MVP since `items_source` only contains accepted user input.
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Look for the literal "fn main" preceded by start-of-input or a
        // non-identifier byte (avoid matching inside identifiers).
        let after_fn_main_starts = bytes[i..].starts_with(b"fn main")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
        if after_fn_main_starts {
            // Skip past `fn main`, then to the opening `{`.
            let mut j = i + b"fn main".len();
            while j < bytes.len() && bytes[j] != b'{' {
                j += 1;
            }
            if j == bytes.len() {
                // Malformed — fall through and copy the rest.
                out.push_str(&src[i..]);
                return out;
            }
            // Brace-balance from the opening `{`.
            let mut depth = 0i32;
            while j < bytes.len() {
                match bytes[j] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Find the static type of a `let __t = <expr>;` binding inside the
/// synthetic main. Walks the AST for the let, then looks up the RHS span
/// in `expr_types`.
pub(super) fn lookup_let_rhs_type(
    program: &crate::ast::Program,
    typed: &crate::typechecker::TypeCheckResult,
    name: &str,
) -> Option<String> {
    use crate::ast::{ExprKind, Item, PatternKind, StmtKind};
    use crate::resolver::SpanKey;
    use crate::typechecker::type_display;

    for it in &program.items {
        if let Item::Function(f) = it {
            if f.name != "main" {
                continue;
            }
            for stmt in &f.body.stmts {
                if let StmtKind::Let { pattern, value, .. } = &stmt.kind {
                    let bound_name = match &pattern.kind {
                        PatternKind::Binding(n) => Some(n.as_str()),
                        _ => None,
                    };
                    if bound_name == Some(name) {
                        let key = SpanKey::from_span(&value.span);
                        if let Some(t) = typed.expr_types.get(&key) {
                            return Some(type_display(t));
                        }
                        // Fall back to the literal-int / -bool case where
                        // expr_types may not record a span for trivial RHS.
                        return match &value.kind {
                            ExprKind::Bool(_) => Some("bool".into()),
                            ExprKind::Integer(_, _) => Some("i64".into()),
                            ExprKind::Float(_, _) => Some("f64".into()),
                            ExprKind::StringLit(_) => Some("String".into()),
                            _ => None,
                        };
                    }
                }
            }
        }
    }
    None
}

/// Cheap structural-balance probe: returns `false` when the input clearly
/// has more open delimiters than close. Skips characters inside string
/// literals (`"..."`), char literals (`'.'`), and line comments (`// …`)
/// so braces inside those don't trip the multi-line probe.
pub(super) fn is_balanced(src: &str) -> bool {
    let bytes = src.as_bytes();
    let mut depth_curly = 0i32;
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            b'{' => {
                depth_curly += 1;
                i += 1;
            }
            b'}' => {
                depth_curly -= 1;
                i += 1;
            }
            b'(' => {
                depth_paren += 1;
                i += 1;
            }
            b')' => {
                depth_paren -= 1;
                i += 1;
            }
            b'[' => {
                depth_brack += 1;
                i += 1;
            }
            b']' => {
                depth_brack -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    depth_curly <= 0 && depth_paren <= 0 && depth_brack <= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_simple() {
        assert!(is_balanced(""));
        assert!(is_balanced("let x = 5;"));
        assert!(is_balanced("fn f() { 1 }"));
    }

    #[test]
    fn unbalanced_open_brace() {
        assert!(!is_balanced("fn f() {"));
        assert!(!is_balanced("if true {"));
    }

    #[test]
    fn balanced_with_string_braces() {
        // Curly inside a string literal must not affect balance.
        assert!(is_balanced(r#"println("hello {world}");"#));
    }

    #[test]
    fn balanced_with_line_comment_brace() {
        assert!(is_balanced("let x = 5; // {\n"));
    }

    #[test]
    fn strip_main_removes_def() {
        let src = "struct A {}\nfn main() { let x = 1; }\nfn helper() {}\n";
        let stripped = strip_main(src);
        assert!(!stripped.contains("fn main"));
        assert!(stripped.contains("struct A"));
        assert!(stripped.contains("fn helper"));
    }

    #[test]
    fn strip_main_no_op_when_absent() {
        let src = "struct A {}\nfn helper() { 1 }\n";
        let stripped = strip_main(src);
        assert_eq!(stripped, src);
    }

    #[test]
    fn strip_main_preserves_nested_braces() {
        let src = "fn main() { if true { 1 } else { 2 } }\nfn helper() {}\n";
        let stripped = strip_main(src);
        assert!(!stripped.contains("fn main"));
        assert!(stripped.contains("fn helper"));
    }
}
