//! Static-count cost-surface aggregator backing `karac query cost-summary`.
//!
//! Implements the v1 schema from `design.md § Compiler Query API`:
//! per-function and per-module tallies of every silent runtime cost the
//! compiler emitted, with a `derivation` array of source locations.
//!
//! v1 wires the categories whose data already flows out of completed passes:
//! `rc_ops` (from the ownership pass's RC fallback table) and
//! `arc_provider_wraps` (counted by walking `with_provider[R](...)` call
//! sites in the AST). `borrow_flag_fields` is a totals-only count over every
//! `shared struct`'s `mut` fields — struct-attributable, not function-
//! attributable. `partition_guard_sites` and `auto_clone_insertions` are
//! reported as `0` until parameterized resources and the REPL `--auto-clone`
//! flag respectively land — the schema reserves the keys so post-v1 wiring
//! is purely additive.
//!
//! Tier 2 perf notes (definition-site, predictive — surfaced only when the
//! caller opts in via the perf-report channel) ride alongside the totals as
//! a `perf_notes` array. Each entry carries a stable `code`, the prose
//! `message` from design.md, the offending struct name, the bare-`mut`
//! field names, and the definition-site `Span`. Today's wired diagnostic
//! is `perf[shared-struct-mut-field]`; new notes plug into the same vector
//! without further schema work.

use std::collections::BTreeMap;

use crate::ast::{Expr, ExprKind, ImplItem, Item, Program, Stmt, StmtKind};
use crate::ownership::{OwnershipCheckResult, RcEntry, RcTrigger};
use crate::token::Span;

/// Top-level cost summary for a file.
#[derive(Debug, Clone, Default)]
pub struct CostSummary {
    pub scope: String,
    pub totals: CostTotals,
    /// Per-function rows, sorted by function key for stable output.
    pub by_function: Vec<FunctionCostRow>,
    /// Tier 2 perf notes harvested at definition time. Off-by-default in the
    /// surface that consumes this aggregator (the perf-report channel); the
    /// data is collected unconditionally so the channel is purely additive.
    pub perf_notes: Vec<PerfNote>,
}

/// A definition-site Tier 2 perf note. Predictive — fires before any cost is
/// paid (no RC inserted, no atomic emitted), so the surface that renders this
/// must gate on the user's perf-report opt-in. See design.md § Performance
/// Diagnostics for the three-tier taxonomy.
#[derive(Debug, Clone)]
pub struct PerfNote {
    /// Stable diagnostic code (e.g. `perf[shared-struct-mut-field]`).
    pub code: String,
    /// Human-readable message body, suitable for display alongside the code.
    pub message: String,
    /// Site of the offending definition.
    pub site: Span,
}

#[derive(Debug, Clone, Default)]
pub struct CostTotals {
    pub rc_ops: RcOpsBreakdown,
    pub arc_provider_wraps: usize,
    pub borrow_flag_fields: usize,
    pub partition_guard_sites: usize,
    pub auto_clone_insertions: usize,
}

#[derive(Debug, Clone, Default)]
pub struct RcOpsBreakdown {
    pub count: usize,
    pub rc: usize,
    pub arc: usize,
}

#[derive(Debug, Clone)]
pub struct FunctionCostRow {
    /// Function key, matching the ownership pass's convention (`fn_name`
    /// for free functions, `Type.method` for methods).
    pub function: String,
    pub rc_ops: usize,
    pub arc_provider_wraps: usize,
    pub derivation: Vec<DerivationEntry>,
}

#[derive(Debug, Clone)]
pub struct DerivationEntry {
    pub reason: String,
    pub site: Span,
}

/// Aggregate the cost-summary for `program` against the ownership analysis.
pub fn build(scope: &str, program: &Program, ownership: &OwnershipCheckResult) -> CostSummary {
    let mut rows: BTreeMap<String, FunctionCostRow> = BTreeMap::new();

    // 1. RC ops — drawn from the ownership pass's per-function RC table.
    //    Each entry counts as one rc_op (refcount inc/dec); flavor (Rc vs Arc)
    //    comes from whether the binding got promoted into `arc_values`.
    let mut rc_total = 0usize;
    let mut arc_total = 0usize;
    for (fn_key, rc_map) in &ownership.rc_values {
        let arc_set = ownership.arc_values.get(fn_key);
        let row = rows
            .entry(fn_key.clone())
            .or_insert_with(|| empty_row(fn_key));
        for (binding, entry) in rc_map {
            let is_arc = arc_set.is_some_and(|s| s.contains(binding));
            if is_arc {
                arc_total += 1;
            } else {
                rc_total += 1;
            }
            row.rc_ops += 1;
            row.derivation.push(rc_derivation(entry, is_arc));
        }
    }

    // 2. Arc-provider-wraps — walk every `with_provider[R](...)` call site,
    //    attributing each to its enclosing function. Each call site emits one
    //    Arc wrap when the closure body crosses a task boundary; v1's static
    //    count is the number of call sites (conservative — design.md treats
    //    every wrap as a candidate cost regardless of whether codegen will
    //    actually emit the atomic).
    let mut arc_provider_wraps_total = 0usize;
    for item in &program.items {
        match item {
            Item::Function(f) => {
                let mut sites = Vec::new();
                walk_block_for_with_provider(&f.body, &mut sites);
                if !sites.is_empty() {
                    let row = rows
                        .entry(f.name.clone())
                        .or_insert_with(|| empty_row(&f.name));
                    for site in &sites {
                        row.arc_provider_wraps += 1;
                        row.derivation.push(DerivationEntry {
                            reason: format!(
                                "`with_provider[{}]` call site — closure capture is Arc-wrapped if it crosses a task boundary",
                                site.resource
                            ),
                            site: site.span.clone(),
                        });
                    }
                    arc_provider_wraps_total += sites.len();
                }
            }
            Item::ImplBlock(imp) => {
                use crate::ast::TypeKind;
                let type_name = match &imp.target_type.kind {
                    TypeKind::Path(p) => p.segments.last().cloned().unwrap_or_default(),
                    _ => String::new(),
                };
                for impl_item in &imp.items {
                    let ImplItem::Method(m) = impl_item else {
                        continue;
                    };
                    let mut sites = Vec::new();
                    walk_block_for_with_provider(&m.body, &mut sites);
                    if !sites.is_empty() {
                        let key = if type_name.is_empty() {
                            m.name.clone()
                        } else {
                            format!("{type_name}.{}", m.name)
                        };
                        let row = rows.entry(key.clone()).or_insert_with(|| empty_row(&key));
                        for site in &sites {
                            row.arc_provider_wraps += 1;
                            row.derivation.push(DerivationEntry {
                                reason: format!(
                                    "`with_provider[{}]` call site — closure capture is Arc-wrapped if it crosses a task boundary",
                                    site.resource
                                ),
                                site: site.span.clone(),
                            });
                        }
                        arc_provider_wraps_total += sites.len();
                    }
                }
            }
            _ => {}
        }
    }

    // 3. Borrow-flag fields — totals-only sum over every `shared struct`'s
    //    `mut` fields (struct-attributable, not function-attributable).
    //    Same walk doubles as the definition-site source for the Tier 2
    //    `perf[shared-struct-mut-field]` perf note: one note per offending
    //    struct (not per field), with the field names enumerated in the
    //    message body so the user sees the migration target without
    //    re-reading the source. The note is collected unconditionally; the
    //    perf-report channel decides whether to render it.
    let mut borrow_flag_total = 0usize;
    let mut perf_notes: Vec<PerfNote> = Vec::new();
    for item in &program.items {
        if let Item::StructDef(s) = item {
            if s.is_shared {
                let mut_fields: Vec<&str> = s
                    .fields
                    .iter()
                    .filter(|f| f.is_mut)
                    .map(|f| f.name.as_str())
                    .collect();
                borrow_flag_total += mut_fields.len();
                if !mut_fields.is_empty() {
                    perf_notes.push(shared_struct_mut_field_note(&s.name, &mut_fields, &s.span));
                }
            }
        }
    }

    let mut by_function: Vec<FunctionCostRow> = rows.into_values().collect();
    // Stable order; BTreeMap already sorted by key, but `into_values` keeps
    // that order — assert by sorting again so refactors stay safe.
    by_function.sort_by(|a, b| a.function.cmp(&b.function));

    CostSummary {
        scope: scope.to_string(),
        totals: CostTotals {
            rc_ops: RcOpsBreakdown {
                count: rc_total + arc_total,
                rc: rc_total,
                arc: arc_total,
            },
            arc_provider_wraps: arc_provider_wraps_total,
            borrow_flag_fields: borrow_flag_total,
            partition_guard_sites: 0,
            auto_clone_insertions: 0,
        },
        by_function,
        perf_notes,
    }
}

/// Build the canonical message body for a `perf[shared-struct-mut-field]`
/// note. Mirrors the prose in design.md § Compiler-assisted migration from
/// `shared struct` to `par struct` (and § Performance Diagnostics).
fn shared_struct_mut_field_note(name: &str, mut_fields: &[&str], span: &Span) -> PerfNote {
    let field_list = mut_fields
        .iter()
        .map(|f| format!("`{f}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!(
        "`shared struct {name}` has mut field{plural} ({field_list}); if a future caller \
         needs concurrent access, the migration to `par struct` is structural — consider \
         defining as `par struct` from the start.",
        plural = if mut_fields.len() == 1 { "" } else { "s" },
    );
    PerfNote {
        code: "perf[shared-struct-mut-field]".to_string(),
        message,
        site: span.clone(),
    }
}

fn empty_row(name: &str) -> FunctionCostRow {
    FunctionCostRow {
        function: name.to_string(),
        rc_ops: 0,
        arc_provider_wraps: 0,
        derivation: Vec::new(),
    }
}

fn rc_derivation(entry: &RcEntry, is_arc: bool) -> DerivationEntry {
    let trigger_label = match entry.trigger {
        RcTrigger::DirectReuseAfterConsume => "direct re-use after consume",
        RcTrigger::ClosureCaptureWithOuterUse => "closure capture with outer use",
        RcTrigger::ContainerStoreWithSubsequentUse => "container store with subsequent use",
    };
    let kind = if is_arc { "Arc" } else { "Rc" };
    DerivationEntry {
        reason: format!("{kind} fallback for `{}` — {trigger_label}", entry.binding),
        site: entry.other_use_span.clone(),
    }
}

struct WithProviderSite {
    resource: String,
    span: Span,
}

fn walk_block_for_with_provider(block: &crate::ast::Block, out: &mut Vec<WithProviderSite>) {
    for stmt in &block.stmts {
        walk_stmt_for_with_provider(stmt, out);
    }
    if let Some(ref tail) = block.final_expr {
        walk_expr_for_with_provider(tail, out);
    }
}

fn walk_stmt_for_with_provider(stmt: &Stmt, out: &mut Vec<WithProviderSite>) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => walk_expr_for_with_provider(value, out),
        StmtKind::LetUninit { .. } => {}
        StmtKind::LetElse {
            value, else_block, ..
        } => {
            walk_expr_for_with_provider(value, out);
            walk_block_for_with_provider(else_block, out);
        }
        StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
            walk_block_for_with_provider(body, out)
        }
        StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
            walk_expr_for_with_provider(target, out);
            walk_expr_for_with_provider(value, out);
        }
        StmtKind::Expr(e) => walk_expr_for_with_provider(e, out),
    }
}

fn walk_expr_for_with_provider(expr: &Expr, out: &mut Vec<WithProviderSite>) {
    match &expr.kind {
        ExprKind::Call { callee, args } => {
            if let Some(resource) = match_with_provider_callee(callee) {
                out.push(WithProviderSite {
                    resource,
                    span: expr.span.clone(),
                });
            }
            walk_expr_for_with_provider(callee, out);
            for a in args {
                walk_expr_for_with_provider(&a.value, out);
            }
        }
        ExprKind::Block(b)
        | ExprKind::Unsafe(b)
        | ExprKind::Try(b)
        | ExprKind::Seq(b)
        | ExprKind::Par(b) => walk_block_for_with_provider(b, out),
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            walk_expr_for_with_provider(condition, out);
            walk_block_for_with_provider(then_block, out);
            if let Some(e) = else_branch {
                walk_expr_for_with_provider(e, out);
            }
        }
        ExprKind::IfLet {
            value,
            then_block,
            else_branch,
            ..
        } => {
            walk_expr_for_with_provider(value, out);
            walk_block_for_with_provider(then_block, out);
            if let Some(e) = else_branch {
                walk_expr_for_with_provider(e, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr_for_with_provider(scrutinee, out);
            for arm in arms {
                walk_expr_for_with_provider(&arm.body, out);
            }
        }
        ExprKind::While {
            condition, body, ..
        } => {
            walk_expr_for_with_provider(condition, out);
            walk_block_for_with_provider(body, out);
        }
        ExprKind::WhileLet { value, body, .. } => {
            walk_expr_for_with_provider(value, out);
            walk_block_for_with_provider(body, out);
        }
        ExprKind::For { iterable, body, .. } => {
            walk_expr_for_with_provider(iterable, out);
            walk_block_for_with_provider(body, out);
        }
        ExprKind::Loop { body, .. } => walk_block_for_with_provider(body, out),
        ExprKind::LabeledBlock { body, .. } => walk_block_for_with_provider(body, out),
        ExprKind::Closure { body, .. } => walk_expr_for_with_provider(body, out),
        ExprKind::MethodCall { object, args, .. } => {
            walk_expr_for_with_provider(object, out);
            for a in args {
                walk_expr_for_with_provider(&a.value, out);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            walk_expr_for_with_provider(left, out);
            walk_expr_for_with_provider(right, out);
        }
        ExprKind::Unary { operand, .. } => walk_expr_for_with_provider(operand, out),
        ExprKind::Question(e) => walk_expr_for_with_provider(e, out),
        ExprKind::OptionalChain { object, args, .. } => {
            walk_expr_for_with_provider(object, out);
            if let Some(args) = args {
                for a in args {
                    walk_expr_for_with_provider(&a.value, out);
                }
            }
        }
        ExprKind::NilCoalesce { left, right } | ExprKind::Pipe { left, right } => {
            walk_expr_for_with_provider(left, out);
            walk_expr_for_with_provider(right, out);
        }
        ExprKind::Index { object, index } => {
            walk_expr_for_with_provider(object, out);
            walk_expr_for_with_provider(index, out);
        }
        ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
            walk_expr_for_with_provider(object, out)
        }
        ExprKind::Tuple(items) | ExprKind::ArrayLiteral(items) => {
            for e in items {
                walk_expr_for_with_provider(e, out);
            }
        }
        ExprKind::PrefixCollectionLiteral { items, .. } => {
            for e in items {
                walk_expr_for_with_provider(e, out);
            }
        }
        ExprKind::MapLiteral(entries) => {
            for (k, v) in entries {
                walk_expr_for_with_provider(k, out);
                walk_expr_for_with_provider(v, out);
            }
        }
        ExprKind::RepeatLiteral { value, count, .. } => {
            walk_expr_for_with_provider(value, out);
            walk_expr_for_with_provider(count, out);
        }
        ExprKind::StructLiteral { fields, spread, .. } => {
            for fi in fields {
                walk_expr_for_with_provider(&fi.value, out);
            }
            if let Some(sp) = spread {
                walk_expr_for_with_provider(sp, out);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(e) = start {
                walk_expr_for_with_provider(e, out);
            }
            if let Some(e) = end {
                walk_expr_for_with_provider(e, out);
            }
        }
        ExprKind::Cast { expr: inner, .. } => walk_expr_for_with_provider(inner, out),
        ExprKind::Return(Some(e)) => walk_expr_for_with_provider(e, out),
        ExprKind::Break { value: Some(v), .. } => walk_expr_for_with_provider(v, out),
        ExprKind::Lock { body, .. } => walk_block_for_with_provider(body, out),
        ExprKind::Providers { bindings, body } => {
            for b in bindings {
                walk_expr_for_with_provider(&b.value, out);
            }
            walk_block_for_with_provider(body, out);
        }
        // Leaf nodes contribute nothing.
        ExprKind::Integer(_, _)
        | ExprKind::Float(_, _)
        | ExprKind::CharLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::MultiStringLit(_)
        | ExprKind::InterpolatedStringLit(_)
        | ExprKind::Bool(_)
        | ExprKind::Identifier(_)
        | ExprKind::Path { .. }
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::PipePlaceholder
        | ExprKind::Return(None)
        | ExprKind::Break { value: None, .. }
        | ExprKind::Continue { .. }
        | ExprKind::OffsetOf { .. }
        | ExprKind::Error => {}
    }
}

/// Recognize the AST shape `with_provider[R](provider, closure)` —
/// `Index { object: Identifier("with_provider"), index: Identifier(R) }`
/// applied to two arguments. Returns the bare resource name on a hit.
fn match_with_provider_callee(callee: &Expr) -> Option<String> {
    let ExprKind::Index { object, index } = &callee.kind else {
        return None;
    };
    let is_with_provider = match &object.kind {
        ExprKind::Identifier(n) => n == "with_provider",
        ExprKind::Path { segments, .. } => segments.as_slice() == ["with_provider"],
        _ => false,
    };
    if !is_with_provider {
        return None;
    }
    match &index.kind {
        ExprKind::Identifier(n) => Some(n.clone()),
        ExprKind::Path { segments, .. } => segments.last().cloned(),
        _ => None,
    }
}
