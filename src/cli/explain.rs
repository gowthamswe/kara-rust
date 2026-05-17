//! Static concept-level explainer pages surfaced by `karac explain`.
//!
//! Each page is a `&'static str` rendered verbatim. The text shape is
//! frozen by tests in `tests/cli.rs` — diagnostic-redirect wording and
//! cross-references must stay aligned with the implementation surface
//! they describe (the ownership checker, `karac query ownership`, and
//! the design.md sections the page cites).

use std::process;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExplainConcept {
    Closures,
}

impl ExplainConcept {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "closures" => Some(ExplainConcept::Closures),
            _ => None,
        }
    }

    pub fn page(self) -> &'static str {
        match self {
            ExplainConcept::Closures => CLOSURES_PAGE,
        }
    }
}

/// Render the requested concept page to stdout and return. Exits the
/// process non-zero if the concept name is unknown, with a focused
/// hint listing the supported set.
pub fn render(concept_name: &str) {
    let Some(concept) = ExplainConcept::parse(concept_name) else {
        eprintln!(
            "error: unknown concept '{concept_name}'. Supported: {}.",
            concept_list(),
        );
        process::exit(1);
    };
    println!("{}", concept.page());
}

fn concept_list() -> String {
    // Single concept today; the list shape future-proofs against
    // additional pages without rewriting the dispatch surface.
    "closures".to_string()
}

/// Concept page for `karac explain --concept=closures`. Describes
/// Rule 2 first-use capture-mode inference, the three explicit
/// prefixes (`own` / `ref` / `mut ref`), the K2 conflict table with
/// the exact diagnostic-redirect wording the ownership checker emits,
/// the outer-scope routing rule for `own`-captured roots, and the
/// per-function inspection surface (`karac query ownership <fn>`).
///
/// Cross-references the disjoint-capture (Rule 2¼) extension — see
/// `docs/implementation_checklist/phase-5-diagnostics.md` § Disjoint
/// closure capture; once that lands, the per-name inference described
/// here generalises to per-path uniformly through the same
/// `closure_captures` registry without rewriting this page.
const CLOSURES_PAGE: &str = "\
karac explain — Closures: parameter modes, capture, and escape

Source of truth: docs/design.md § Closures: parameter modes, capture,
and escape > Rule 2 / Rule 2½. This page is the concept-level summary;
the design.md section is authoritative when the two disagree.

────────────────────────────────────────────────────────────────────
Bare form: |x| body — Rule 2 first-use inference
────────────────────────────────────────────────────────────────────

A bare closure runs a per-captured-name scan over the body and picks
the weakest mode that satisfies the body's first classifying use:

    first use is a read     → capture is taken by `ref`
    first use is a mutate   → capture is taken by `mut ref`
    first use is a consume  → capture is taken by `own` (moved in)

The closure does whatever the body demands, no more. Modes form an
ordering — `ref < mut ref < own` — and the inferred mode is the
minimum that satisfies the body.

Granularity is per-capture-name today: field projections under the
same root binding (e.g. `o.x` and `o.y`) collapse to one entry for
the root `o`. The disjoint-capture extension (Rule 2¼) will refine
this to per-path so two closures over different fields of the same
struct can each take their own mode — see
phase-5-diagnostics.md § \"Disjoint closure capture\".

────────────────────────────────────────────────────────────────────
Outer-scope routing for `own`-captured roots
────────────────────────────────────────────────────────────────────

When a bare body consumes a captured root, the root is classified
`own` and moved into the closure. A *use of the same binding after
the closure expression* is not a use-after-move error — it routes
through Part 4's RC fallback (RcTrigger::ClosureCaptureWithOuterUse),
tentatively promoting the binding to `Rc`. This is the spec's
\"(ii) Escape with outer use of a capture\" path: the closure creation
and the outer use compose under the existing RC dataflow pass rather
than a closure-specific borrow rule.

────────────────────────────────────────────────────────────────────
Explicit prefixes: own | ref | mut ref
────────────────────────────────────────────────────────────────────

Three optional keywords on a closure expression *pin* every captured
path to a single declared mode regardless of what first-use inference
would pick:

    |x| body          // bare — per-capture inference (Rule 2)
    own |x| body      // every capture is by value (consume — moved)
    ref |x| body      // every capture is by reference (read-only)
    mut ref |x| body  // every capture is by mutable reference

The prefix is *per closure*, not per capture. Per-name override
syntax is deferred — the per-closure form composes forward without
breakage if real programs surface the need.

`move |x|` is rejected with a focused diagnostic redirecting to
`own |x|` (Kāra uses the `own` keyword; see design.md § Reserved
keywords).

────────────────────────────────────────────────────────────────────
K2 conflict table — declared mode is the floor
────────────────────────────────────────────────────────────────────

When a prefix is present, body usage must satisfy the declared mode
but may be weaker. Stronger usage than declared is a compile error
at the closure expression site, naming the capture and the offending
use's line.

    Declared    Body usage          Result
    ────────    ───────────────     ───────────────────────────────
    own         reads only          OK — \"capture for ownership
                                         extension\" idiom
    own         mutates             OK
    own         consumes            OK
    ref         reads only          OK
    ref         mutates             ERROR (escalation)
    ref         consumes            ERROR — see [K2-ref-consume]
    mut ref     reads only          OK — perf note
                                         [unused-mut-capture]
    mut ref     mutates             OK
    mut ref     consumes            ERROR — see [K2-mut-ref-consume]

The bare form has no row in this table — its body-usage row *is*
the inference rule and there is nothing to conflict against.

Diagnostic wording the ownership checker emits (pinned by
slice 1 of phase-5-diagnostics.md § Closure default capture mode):

  [K2-ref-consume]
    capture `x` declared `ref` but consumed in closure body at
    line N — drop the `ref` prefix (use `own` or bare) or remove the consume

  [K2-mut-ref-consume]
    capture `x` declared `mut ref` but consumed in closure body at
    line N — drop the `mut ref` prefix and use `own`

  [unused-mut-capture]
    perf[unused-mut-capture]: capture `x` declared `mut ref` but never
    mutated — consider `ref` (machine-applicable rewrite when the
    prefix span is recorded)

────────────────────────────────────────────────────────────────────
When to use which form
────────────────────────────────────────────────────────────────────

  • Use bare `|x|` when the body is short, the closure stays inside
    its creation scope, and refactoring fragility is not a concern.
    First-use inference is locally fragile — reordering body lines
    or adding an early `.clone()` can flip a capture from consume
    to read, which changes RC decisions in the *enclosing* function.

  • Use an explicit prefix (`own` / `ref` / `mut ref`) when the
    closure escapes (return, store, send across a channel) and the
    captures' fates need to be visible at the closure expression
    site so a benign body refactor cannot silently alter the
    surrounding ownership analysis.

────────────────────────────────────────────────────────────────────
Inspecting inferred capture modes
────────────────────────────────────────────────────────────────────

Per-function inferred capture modes are exposed by

    karac query ownership <file>.<function>

Each closure in the function shows as a JSON entry with `parameters`
(one record per parameter, `{name, mode}`) and `captures` (the same
shape per captured root binding), each tagged with the closure's
source `line` / `column`. The `mode` field is one of `own` / `ref`
/ `mut_ref` and reflects either the prefix-declared mode (if a
prefix is present) or the Rule 2 inferred mode (if bare).

Sample shape:

    {
      \"function\": \"main\",
      \"closures\": [
        {
          \"line\": 7, \"column\": 19,
          \"parameters\": [{\"name\": \"x\", \"mode\": \"ref\"}],
          \"captures\":   [{\"name\": \"o\", \"mode\": \"own\"}]
        }
      ]
    }
";
