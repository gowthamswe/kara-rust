//! Lint registry + lint-level data types.
//!
//! Implements the v1 surface of `design.md § Lint Level Attributes`:
//! the four lint-level attributes (`#[allow]`, `#[warn]`, `#[deny]`,
//! `#[expect]`), the central lint registry enumerating named lints
//! with default levels, and the data types the parser/resolver use
//! to attach lint-level overrides to AST items.
//!
//! **Scope of slice 1+2+3 (this module).**
//!
//! - Defines `LintLevel`, `LintLevelOverride`, and `LintInfo`.
//! - Defines the v1 starter set of 15 lint names with their
//!   default levels. The name set is closed at the registry — new
//!   lints are added here, not invented at use sites.
//! - Provides `lint_by_name` lookup so the parser can decide
//!   whether a name in `#[allow(NAME)]` is known.
//!
//! **Out of scope for this slice.** The scope cascade (slice 4 of
//! the parent entry — walking outer attributes when emitting a
//! lint), `#[expect]` semantics (slice 5 — tracking whether the
//! lint fired anywhere in the attributed scope), the
//! `unknown_lint` warning emission (parser-time unknown lints are
//! currently silently accepted so `#[allow(removed_lint)]` from
//! older code keeps building — see "Naming" in design.md), and
//! the per-warning lint-name carryover into structured diagnostic
//! output (slice 7) all land in follow-up slices.

use crate::token::Span;

/// The four level overrides per `design.md § Lint Level
/// Attributes`. `Expect` carries cleanup-tracking semantics in
/// slice 5; today it is structurally identical to `Allow` at
/// parse time (suppresses the lint) and exists so the spec's
/// surface compiles round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintLevel {
    Allow,
    Warn,
    Deny,
    Expect,
}

impl LintLevel {
    /// The attribute name (`"allow"`, `"warn"`, `"deny"`,
    /// `"expect"`) that introduces this level. Used by parser-side
    /// dispatch (the four `#[...]` attribute names map 1:1 here).
    pub fn from_attr_name(name: &str) -> Option<Self> {
        match name {
            "allow" => Some(Self::Allow),
            "warn" => Some(Self::Warn),
            "deny" => Some(Self::Deny),
            "expect" => Some(Self::Expect),
            _ => None,
        }
    }

    pub fn as_attr_name(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn => "warn",
            Self::Deny => "deny",
            Self::Expect => "expect",
        }
    }
}

/// One `(level, lint-name)` pair extracted from a lint-level
/// attribute. `#[allow(deprecated, rc_fallback)]` produces two
/// overrides, both `LintLevel::Allow`, with `lint` set to
/// `"deprecated"` and `"rc_fallback"` respectively. `span` points
/// at the lint-name token inside the attribute parens so the
/// scope-cascade diagnostic (slice 4) can underline the precise
/// authoring site.
#[derive(Debug, Clone)]
pub struct LintLevelOverride {
    pub span: Span,
    pub level: LintLevel,
    pub lint: String,
}

/// Per-lint metadata in the central registry. `default_level` is
/// the level the lint emits at when no override is in scope.
/// `description` is the short blurb surfaced by `karac lint --list`
/// (post-v1; today the field exists for future use and as a
/// reviewer-friendly identifier).
#[derive(Debug, Clone, Copy)]
pub struct LintInfo {
    pub name: &'static str,
    pub default_level: LintLevel,
    pub description: &'static str,
}

/// The v1 starter set of named lints per `design.md § Lint Level
/// Attributes > Built-in lints (v1 starter set)`. The list is
/// closed — adding a new lint requires a new entry here so the
/// registry stays the single source of truth.
///
/// `unsafe_op_in_unsafe_fn` is intentionally absent: per the spec
/// it is a *hard rule*, not a lint, and the four lint-level
/// attributes are rejected on it (slice 6 enforcement).
pub const STARTER_LINTS: &[LintInfo] = &[
    LintInfo {
        name: "deprecated",
        default_level: LintLevel::Warn,
        description: "Use of an item annotated with `#[deprecated]`.",
    },
    LintInfo {
        name: "rc_fallback",
        default_level: LintLevel::Warn,
        description: "An owned binding fell back to RC because a closure or borrow conflict made stack ownership infeasible.",
    },
    LintInfo {
        name: "implicit_clone",
        default_level: LintLevel::Warn,
        description: "A copyable value was implicitly cloned at a consume site.",
    },
    LintInfo {
        name: "mutual_recursion_note",
        default_level: LintLevel::Warn,
        description: "Per-SCC mutual recursion advisory — the effect ceiling spans a recursion group.",
    },
    LintInfo {
        name: "module_mut_binding",
        default_level: LintLevel::Warn,
        description: "A module-level `let mut` binding shadows a value that the resolver would otherwise rebind.",
    },
    LintInfo {
        name: "redundant_suffix",
        default_level: LintLevel::Warn,
        description: "A numeric literal's explicit suffix matches the context's inferred type.",
    },
    LintInfo {
        name: "float_in_serialized_type",
        default_level: LintLevel::Warn,
        description: "A `f32`/`f64` field appears in a `#[derive(Serialize)]` type — IEEE NaN equality may break round-trip.",
    },
    LintInfo {
        name: "f16_software_emulated",
        default_level: LintLevel::Warn,
        description: "`f16` arithmetic on a target without native half-precision support is software-emulated.",
    },
    LintInfo {
        name: "pure_loop_in_par",
        default_level: LintLevel::Warn,
        description: "A `par { }` block contains a loop whose body has no parallelisable work.",
    },
    LintInfo {
        name: "undocumented_unsafe",
        default_level: LintLevel::Warn,
        description: "An `unsafe { }` block or `unsafe fn` declaration is missing a SAFETY: doc comment.",
    },
    LintInfo {
        name: "repr_c_layout_ignored",
        default_level: LintLevel::Warn,
        description: "A `layout { }` block on a `#[repr(C)]` struct is silently ignored — `#[repr(C)]` fixes the layout.",
    },
    LintInfo {
        name: "layout_unassigned_fields",
        default_level: LintLevel::Warn,
        description: "A `layout { }` block does not assign every struct field to a group.",
    },
    LintInfo {
        name: "malformed_diagnostic_attribute",
        default_level: LintLevel::Warn,
        description: "A `#[diagnostic::*]` attribute has the wrong shape — accepted but ignored.",
    },
    LintInfo {
        name: "unfulfilled_lint_expectation",
        default_level: LintLevel::Warn,
        description: "An `#[expect(NAME)]` attribute did not see the named lint fire in its attributed scope.",
    },
    LintInfo {
        name: "unknown_lint",
        default_level: LintLevel::Warn,
        description: "A lint-level attribute names a lint the compiler does not recognise.",
    },
    LintInfo {
        name: "unreachable_arm",
        default_level: LintLevel::Warn,
        description:
            "A match arm pattern is fully covered by an earlier (unguarded) arm, so its body \
             can never execute.",
    },
];

/// Look up a lint by its registered name. Returns `None` for
/// names not in the v1 starter set — the parser uses this to
/// decide whether `#[allow(NAME)]` references a known lint.
pub fn lint_by_name(name: &str) -> Option<&'static LintInfo> {
    STARTER_LINTS.iter().find(|info| info.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_set_has_no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for info in STARTER_LINTS {
            assert!(
                seen.insert(info.name),
                "duplicate lint name in registry: {}",
                info.name,
            );
        }
    }

    #[test]
    fn starter_set_covers_spec_listed_lints() {
        // Pin every lint name design.md § "Built-in lints (v1
        // starter set)" lists. Adding a name to the spec without
        // a registry entry breaks this test loudly.
        let required = [
            "deprecated",
            "rc_fallback",
            "implicit_clone",
            "mutual_recursion_note",
            "module_mut_binding",
            "redundant_suffix",
            "float_in_serialized_type",
            "f16_software_emulated",
            "pure_loop_in_par",
            "undocumented_unsafe",
            "repr_c_layout_ignored",
            "layout_unassigned_fields",
            "malformed_diagnostic_attribute",
            "unfulfilled_lint_expectation",
            "unknown_lint",
        ];
        for name in required {
            assert!(
                lint_by_name(name).is_some(),
                "spec-listed lint missing from registry: {name}",
            );
        }
    }

    #[test]
    fn lint_level_round_trips_attribute_name() {
        for level in [
            LintLevel::Allow,
            LintLevel::Warn,
            LintLevel::Deny,
            LintLevel::Expect,
        ] {
            let n = level.as_attr_name();
            assert_eq!(LintLevel::from_attr_name(n), Some(level));
        }
        assert_eq!(LintLevel::from_attr_name("forbid"), None);
        assert_eq!(LintLevel::from_attr_name("deprecated"), None);
    }
}
