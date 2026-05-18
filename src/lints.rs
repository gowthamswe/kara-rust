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
    LintInfo {
        name: "missing_non_exhaustive",
        default_level: LintLevel::Deny,
        description:
            "A stdlib `pub enum` whose name ends in `Error` lacks `#[non_exhaustive]`, blocking \
             future variant additions across packages without a source break.",
    },
    // Slice 4b cross-cutting — registry entries for the per-module
    // lint names so CLI `-A`/`-W`/`-D`/`-F` fall-through works for
    // them and so source-level `#[allow(NAME)]` doesn't trigger
    // spurious `unknown_lint` warnings. The lint *emitters* live in
    // dedicated modules (`must_use_lint`, `missing_must_use_lint`,
    // `logical_lint`, `ffi_lint`); registering the names here makes
    // the cascade walker treat them like any other registered lint.
    LintInfo {
        name: "must_use",
        default_level: LintLevel::Warn,
        description:
            "Discarded value of an implicitly-must-use type (`Result[T, E]` or `Option[T]`) \
             at statement position — silently dropping the value abandons the error / absence \
             branch the author meant to handle.",
    },
    LintInfo {
        name: "missing_must_use",
        default_level: LintLevel::Warn,
        description:
            "A stdlib `pub fn` returns an iterator-shaped or new-value-from-self value but \
             lacks `#[must_use]` — discarding the return drops the work without surfacing.",
    },
    LintInfo {
        name: "ambiguous_not_comparison",
        default_level: LintLevel::Warn,
        description:
            "`not` is adjacent to a comparison operator (`not x == y` parses as `(not x) == y`); \
             disambiguate with explicit parentheses.",
    },
    LintInfo {
        name: "ffi_float_eq",
        default_level: LintLevel::Warn,
        description:
            "An `extern \"C\"` function returning a float is directly compared with `==` or `!=`; \
             FFI floats may not round-trip exactly, so prefer an epsilon comparison.",
    },
];

/// Look up a lint by its registered name. Returns `None` for
/// names not in the v1 starter set — the parser uses this to
/// decide whether `#[allow(NAME)]` references a known lint.
pub fn lint_by_name(name: &str) -> Option<&'static LintInfo> {
    STARTER_LINTS.iter().find(|info| info.name == name)
}

/// CLI build-wide lint level overrides — set via `-A NAME` /
/// `-W NAME` / `-D NAME` / `-F NAME` flags (slice 4b polish).
///
/// **Resolution order.** The cascade reader
/// [`crate::typechecker::TypeChecker::effective_lint_level`] walks
/// the per-item `lint_override_stack` innermost-first; on a miss it
/// consults `level_for(lint, registry_default)` here. So a source
/// `#[allow(NAME)]` always wins over a CLI `-D NAME` — the spec's
/// intent is that the inner scope is the most specific authority.
///
/// **`-D warnings` catch-all.** Stored as a separate boolean rather
/// than expanding into per-lint overrides at parse time so its
/// scope (every registry-default-`Warn` lint) can be computed
/// lazily and so a later per-name `-A NAME` flag interacts cleanly
/// (per-name beats catch-all when both name the same lint).
///
/// **`-F NAME` (forbid).** Acts as `-D NAME` *and* additionally
/// rejects any inner `#[allow(NAME)]` with
/// `error[E_FORBIDDEN_LINT_ALLOW]`. Forbidden names land in both
/// `levels` (mapped to `Deny`) and `forbidden`; the typechecker's
/// pre-pass consults `forbidden` to emit the rejection.
#[derive(Debug, Default, Clone)]
pub struct CliLintOverrides {
    /// Per-lint-name level set by `-A NAME` / `-W NAME` /
    /// `-D NAME` / `-F NAME`. Repeated flags for the same name
    /// last-write-wins. `-F NAME` writes `Deny` here and also
    /// records the name in `forbidden`.
    pub levels: std::collections::HashMap<String, LintLevel>,
    /// Names mentioned by `-F NAME` (forbid). The typechecker's
    /// `emit_forbidden_lint_allow_errors` pre-pass emits a hard
    /// error at any source-level `#[allow(NAME)]` whose target name
    /// is in this set. Membership is independent of `levels` — the
    /// `Deny` mapping there drives the cascade severity; this set
    /// drives the inner-`#[allow]` rejection.
    pub forbidden: std::collections::HashSet<String>,
    /// `-D warnings` was set on the command line — every
    /// registry-default-`Warn` lint is promoted to `Deny` on
    /// cascade fall-through. Subordinate to per-name CLI flags and
    /// to source-level `#[allow]` (both win over this).
    pub deny_warnings: bool,
}

impl CliLintOverrides {
    /// Resolve the build-wide level for a lint after the per-item
    /// cascade missed. Returns `None` when no CLI flag mentions the
    /// lint and `-D warnings` does not apply — caller falls through
    /// to `registry_default`.
    pub fn level_for(&self, lint_name: &str, registry_default: LintLevel) -> Option<LintLevel> {
        if let Some(&lvl) = self.levels.get(lint_name) {
            return Some(lvl);
        }
        if self.deny_warnings && registry_default == LintLevel::Warn {
            return Some(LintLevel::Deny);
        }
        None
    }

    /// True when `-F NAME` named this lint — the typechecker
    /// pre-pass emits `E_FORBIDDEN_LINT_ALLOW` at any inner
    /// `#[allow(NAME)]`.
    pub fn is_forbidden(&self, lint_name: &str) -> bool {
        self.forbidden.contains(lint_name)
    }

    /// Convenience constructor — a single per-name override, no
    /// forbid, no catch-all. Lets tests express the common case
    /// without the `let mut o = Default::default(); o.levels.insert(...)`
    /// pattern that clippy's `field_reassign_with_default` flags.
    pub fn with_level(name: &str, level: LintLevel) -> Self {
        let mut levels = std::collections::HashMap::new();
        levels.insert(name.to_string(), level);
        Self {
            levels,
            ..Self::default()
        }
    }

    /// Convenience constructor — `-F NAME` shape: per-name `Deny`
    /// plus name in the `forbidden` set so inner `#[allow(NAME)]`
    /// is rejected.
    pub fn with_forbid(name: &str) -> Self {
        let mut levels = std::collections::HashMap::new();
        levels.insert(name.to_string(), LintLevel::Deny);
        let mut forbidden = std::collections::HashSet::new();
        forbidden.insert(name.to_string());
        Self {
            levels,
            forbidden,
            ..Self::default()
        }
    }

    /// Convenience constructor — `-D warnings` shape: catch-all
    /// flag only, no per-name overrides.
    pub fn with_deny_warnings() -> Self {
        Self {
            deny_warnings: true,
            ..Self::default()
        }
    }
}

/// The post-cascade severity for a per-module lint emission
/// (slice 4b cross-cutting helper). Each lint module —
/// `unsafe_lint`, `must_use_lint`, `missing_must_use_lint`,
/// `logical_lint`, `ffi_lint` — has its own `LintDiagnostic` shape
/// with a `Warning` / `Error` severity field. This enum is the
/// shared output type the cascade helper hands back, with a
/// `Suppress` variant covering the cases where the lint should not
/// emit at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleLintSeverity {
    /// Don't emit — `#[allow]` / `#[expect]` / CLI `-A` resolved
    /// the lint to silence.
    Suppress,
    /// Emit as a warning (the default for un-promoted Warn lints).
    Warn,
    /// Emit as an error — `#[deny]` / CLI `-D` promoted the lint.
    Deny,
}

/// Resolve the post-cascade severity for a per-module lint
/// emission given the in-source `#[allow]` / `#[deny]` / `#[expect]`
/// flags at the enclosing scope and the CLI build-wide overrides
/// (slice 4b cross-cutting).
///
/// **Precedence (innermost wins, matching the typechecker's
/// `effective_lint_level` rule):**
///
/// 1. Source `#[allow(NAME)]` → `Suppress`.
/// 2. Source `#[expect(NAME)]` → `Suppress`. **Note:** the per-module
///    lint walkers don't record fulfilment back into the typechecker's
///    `fulfilled_expectations` set, so an `#[expect]` whose firing
///    came from one of these modules will surface as `unfulfilled_lint_expectation`
///    at end-of-typecheck. Fixing this requires a fulfilment
///    side-channel from the lint modules into the typechecker —
///    deferred until a concrete need arises (cross-reference the
///    `[->]` sub-bullet under the parent lint-level epic).
/// 3. Source `#[deny(NAME)]` → `Deny`.
/// 4. CLI `-A` / `-W` / `-D` for this lint name → corresponding
///    severity.
/// 5. CLI `-D warnings` catch-all → `Deny` for default-`Warn` lints.
/// 6. Else → registry default level (`Warn` for un-registered names).
///
/// The order pins that source attributes always beat CLI flags —
/// the inner scope is the most specific authority.
pub fn effective_level_for_module_lint(
    source_allow: bool,
    source_deny: bool,
    source_expect: bool,
    cli: &CliLintOverrides,
    lint_name: &str,
) -> ModuleLintSeverity {
    if source_allow || source_expect {
        return ModuleLintSeverity::Suppress;
    }
    if source_deny {
        return ModuleLintSeverity::Deny;
    }
    let registry_default = lint_by_name(lint_name)
        .map(|info| info.default_level)
        .unwrap_or(LintLevel::Warn);
    let resolved = cli
        .level_for(lint_name, registry_default)
        .unwrap_or(registry_default);
    match resolved {
        LintLevel::Allow | LintLevel::Expect => ModuleLintSeverity::Suppress,
        LintLevel::Warn => ModuleLintSeverity::Warn,
        LintLevel::Deny => ModuleLintSeverity::Deny,
    }
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
            "missing_non_exhaustive",
        ];
        for name in required {
            assert!(
                lint_by_name(name).is_some(),
                "spec-listed lint missing from registry: {name}",
            );
        }
    }

    #[test]
    fn cli_overrides_per_name_wins_over_registry_default() {
        let o = CliLintOverrides::with_level("unreachable_arm", LintLevel::Deny);
        assert_eq!(
            o.level_for("unreachable_arm", LintLevel::Warn),
            Some(LintLevel::Deny),
        );
    }

    #[test]
    fn cli_overrides_deny_warnings_promotes_default_warn() {
        let o = CliLintOverrides::with_deny_warnings();
        // A default-Warn lint promotes to Deny via the catch-all.
        assert_eq!(
            o.level_for("unreachable_arm", LintLevel::Warn),
            Some(LintLevel::Deny),
        );
        // A default-Deny lint (e.g. missing_non_exhaustive) is unaffected.
        assert_eq!(o.level_for("missing_non_exhaustive", LintLevel::Deny), None);
    }

    #[test]
    fn cli_overrides_per_name_wins_over_deny_warnings() {
        // `-A unreachable_arm` plus `-D warnings`: the per-name flag
        // wins (last-named-wins-by-specificity). Pins the precedence.
        let o = CliLintOverrides {
            deny_warnings: true,
            ..CliLintOverrides::with_level("unreachable_arm", LintLevel::Allow)
        };
        assert_eq!(
            o.level_for("unreachable_arm", LintLevel::Warn),
            Some(LintLevel::Allow),
        );
    }

    #[test]
    fn cli_overrides_empty_falls_through() {
        let o = CliLintOverrides::default();
        assert_eq!(o.level_for("anything", LintLevel::Warn), None);
        assert!(!o.is_forbidden("anything"));
    }

    #[test]
    fn effective_level_for_module_lint_source_allow_suppresses() {
        let cli = CliLintOverrides::default();
        let sev = effective_level_for_module_lint(true, false, false, &cli, "undocumented_unsafe");
        assert_eq!(sev, ModuleLintSeverity::Suppress);
    }

    #[test]
    fn effective_level_for_module_lint_source_expect_suppresses() {
        let cli = CliLintOverrides::default();
        let sev = effective_level_for_module_lint(false, false, true, &cli, "must_use");
        assert_eq!(sev, ModuleLintSeverity::Suppress);
    }

    #[test]
    fn effective_level_for_module_lint_source_deny_promotes() {
        let cli = CliLintOverrides::default();
        let sev = effective_level_for_module_lint(false, true, false, &cli, "ffi_float_eq");
        assert_eq!(sev, ModuleLintSeverity::Deny);
    }

    #[test]
    fn effective_level_for_module_lint_cli_deny_promotes_default_warn() {
        let cli = CliLintOverrides::with_level("must_use", LintLevel::Deny);
        let sev = effective_level_for_module_lint(false, false, false, &cli, "must_use");
        assert_eq!(sev, ModuleLintSeverity::Deny);
    }

    #[test]
    fn effective_level_for_module_lint_cli_allow_suppresses_default_warn() {
        let cli = CliLintOverrides::with_level("must_use", LintLevel::Allow);
        let sev = effective_level_for_module_lint(false, false, false, &cli, "must_use");
        assert_eq!(sev, ModuleLintSeverity::Suppress);
    }

    #[test]
    fn effective_level_for_module_lint_deny_warnings_catch_all_promotes() {
        let cli = CliLintOverrides::with_deny_warnings();
        let sev =
            effective_level_for_module_lint(false, false, false, &cli, "ambiguous_not_comparison");
        assert_eq!(sev, ModuleLintSeverity::Deny);
    }

    #[test]
    fn effective_level_for_module_lint_source_beats_cli() {
        // Source `#[allow]` should win over CLI `-D` per the cascade
        // precedence rule (inner scope is most specific).
        let cli = CliLintOverrides::with_level("must_use", LintLevel::Deny);
        let sev = effective_level_for_module_lint(true, false, false, &cli, "must_use");
        assert_eq!(sev, ModuleLintSeverity::Suppress);
    }

    #[test]
    fn effective_level_for_module_lint_no_overrides_defaults_to_warn() {
        let cli = CliLintOverrides::default();
        let sev = effective_level_for_module_lint(false, false, false, &cli, "undocumented_unsafe");
        assert_eq!(sev, ModuleLintSeverity::Warn);
    }

    #[test]
    fn cli_overrides_forbidden_flag_is_separate_from_levels() {
        let o = CliLintOverrides::with_forbid("deprecated");
        assert!(o.is_forbidden("deprecated"));
        assert!(!o.is_forbidden("unreachable_arm"));
        // Forbid also writes Deny into `levels` so the cascade
        // fall-through promotes the lint.
        assert_eq!(
            o.level_for("deprecated", LintLevel::Warn),
            Some(LintLevel::Deny),
        );
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
