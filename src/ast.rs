// src/ast.rs

//! Abstract Syntax Tree definitions for the Kāra language.
//! Every node carries a `Span` for source location tracking.

/// Three-level visibility per `design.md § Three-level visibility`.
/// Items carry `is_pub: bool` and `is_private: bool`; this enum is the
/// single-value view used by the resolver / typechecker when enforcing
/// cross-module access rules (CR-24 slice 6). Exactly one of the two
/// bools may be true; both false means `Default` (project-internal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Marked `pub` — visible to end users and all project files.
    Pub,
    /// No visibility keyword — project-internal (visible to all files in
    /// the package, not to external consumers).
    Default,
    /// Marked `private` — visible only within the same directory.
    Private,
}

impl Visibility {
    /// Build a Visibility from the two transitional booleans. Callers that
    /// violate the "at most one true" invariant get `Pub` as the safe fallback
    /// — parser validation should have rejected the combination earlier.
    pub fn from_flags(is_pub: bool, is_private: bool) -> Self {
        if is_pub {
            Visibility::Pub
        } else if is_private {
            Visibility::Private
        } else {
            Visibility::Default
        }
    }

    pub fn is_pub(self) -> bool {
        matches!(self, Visibility::Pub)
    }

    pub fn is_private(self) -> bool {
        matches!(self, Visibility::Private)
    }
}

// ── Program ──────────────────────────────────────────────────────

/// Side-table populated by `lowering::lower_program` from the typechecker's
/// `TypeCheckResult.question_conversions`. Maps each `?` expression's span
/// (offset, length as a `(usize, usize)` tuple) to the fully-qualified name
/// of the target error type when a `From`-based conversion must run before
/// propagation. Used by codegen to emit `Target.from(e)` ahead of the early
/// return; see `src/codegen.rs:compile_question`.
pub type QuestionConversionTable = std::collections::HashMap<(usize, usize), String>;

/// Side-table populated by the cli pipeline from `EffectCheckResult`. Maps
/// each callable's canonical name (free fn `name`, assoc/method `Type.method`)
/// to whether its inferred or declared effects include any of the four
/// "side-effect-bearing" verbs — `reads`, `writes`, `sends`, `receives`.
/// Read by codegen at par-branch call sites: a callee marked `false` skips
/// the cooperative cancel-check atomic load; absent or `true` callees fall
/// back to the conservative "always fire" behavior. See design.md
/// § Effect-boundary cooperative cancellation.
pub type CalleeEffectfulTable = std::collections::HashMap<String, bool>;

/// Side-table populated by the lowering pass from the typechecker's
/// `expr_types` map. Maps each `MethodCall` expression's span to the
/// canonical `Type.method` callee key — the same shape used in
/// `CalleeEffectfulTable`. Codegen consults this table at method-call
/// sites in par branches so the cooperative cancel-check narrowing
/// applies to instance methods, not just free-function / `Type.assoc`
/// calls.
pub type MethodCalleeTypesTable = std::collections::HashMap<(usize, usize), String>;

/// Side-table populated by the lowering pass from the typechecker's
/// `pattern_binding_types` map. Maps each pattern-binding's span (offset,
/// length) to the canonical surface type name (e.g. `"MyError"`). Used by
/// codegen at match-arm bind sites: when binding a tuple-variant payload
/// to a name whose surface type is a struct, codegen reconstitutes the
/// struct value from the i64 payload word so subsequent `.field` access
/// dispatches through the right struct shape.
pub type PatternBindingTypesTable = std::collections::HashMap<(usize, usize), String>;

/// Sibling to `PatternBindingTypesTable`: maps each pattern-binding's span
/// `(offset, length)` to the inner element `TypeExpr` for `Vec[T]` /
/// `Slice[T]` bindings only. Populated by the lowering pass from the
/// typechecker's `pattern_binding_inner_types` map. Consumed by codegen at
/// `bind_pattern_values` to register `vec_elem_types` / `slice_elem_types`
/// under the binding's variable name, so direct method dispatch on a
/// pattern-bound collection payload (`xs.len()` / `xs[0]` / `xs.push(...)`)
/// routes through the right element-typed path. PB sibling slice
/// (2026-05-09).
pub type PatternBindingInnerTypesTable = std::collections::HashMap<(usize, usize), TypeExpr>;

/// Borrow form for a pattern binding under a `ref` / `mut ref` scrutinee.
/// `Ref` corresponds to a `ref T` scrutinee mode; `MutRef` to `mut ref T`.
/// Owned bindings have no entry in `PatternBindingBorrowModesTable` —
/// presence-as-signal lets the codegen short-circuit in the common case.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PatternBindingBorrow {
    Ref,
    MutRef,
}

/// Per-pattern-binding borrow mode populated by the typechecker's
/// `check_pattern_against` walk and forwarded by the lowering pass for
/// codegen to consult. Codegen consumes this at every leaf binding site
/// (plain `Binding`, struct shorthand fields, slice rest bindings,
/// `@`-bindings) to wrap the binding in a "ref shim" — an extra alloca
/// holding a pointer to the value alloca, registered in `ref_params` —
/// so call sites that take a `ref T` / `mut ref T` parameter receive
/// the right ABI shape rather than the raw value. Mirrors the
/// typechecker's `ScrutineeMode::wrap_binding_ty` rule for the codegen
/// surface — design.md § Match Arm Binding Modes.
pub type PatternBindingBorrowModesTable =
    std::collections::HashMap<(usize, usize), PatternBindingBorrow>;

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub items: Vec<Item>,
    /// Joined `//!` doc-comment text at the top of the source file.
    /// Lines from a single run of `//!` are concatenated with `\n`.
    /// `None` when the file has no leading `//!` lines.
    pub module_doc_comment: Option<String>,
    /// Set by the lowering pass; empty before lowering runs.
    pub question_conversions: QuestionConversionTable,
    /// Set by the cli pipeline after effectcheck; empty otherwise.
    pub callee_effectful: CalleeEffectfulTable,
    /// Set by the lowering pass from `TypeCheckResult.expr_types`; empty otherwise.
    pub method_callee_types: MethodCalleeTypesTable,
    /// Set by the lowering pass from `TypeCheckResult.pattern_binding_types`.
    pub pattern_binding_types: PatternBindingTypesTable,
    /// Set by the lowering pass from `TypeCheckResult.pattern_binding_inner_types`.
    /// PB sibling slice (2026-05-09).
    pub pattern_binding_inner_types: PatternBindingInnerTypesTable,
    /// Set by the lowering pass from
    /// `TypeCheckResult.pattern_binding_borrow_modes`. Consumed by codegen
    /// to apply the ref-binding shim at match-arm leaf bindings under a
    /// `ref` / `mut ref` scrutinee. Empty entries mean owned bindings.
    pub pattern_binding_borrow_modes: PatternBindingBorrowModesTable,
}

mod exprs;
mod items;
mod patterns;
mod stmts;
mod types;
pub use exprs::*;
pub use items::*;
pub use patterns::*;
pub use stmts::*;
pub use types::*;
