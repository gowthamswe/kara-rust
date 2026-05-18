//! Extern-function effect registration + FFI linter hints + profile
//! enforcement.
//!
//! Houses three related functions:
//!
//! - `register_extern_function_effects` — per-`extern` declaration
//!   handler: seeds `declared_effects` from the ABI's trust-not-verify
//!   default, honors `@noblock` opt-outs, and runs the FFI linter.
//! - `check_ffi_linter_hints` — advisory `FfiLintHint` diagnostics
//!   for extern symbols whose names suggest commonly-omitted effects
//!   (`blocks`, `allocates(Heap)`).
//! - `profile_forbids` — compile-profile forbidden-effect lookup
//!   (e.g., the `no_alloc` profile rejects `allocates(Heap)`).
//!
//! Lives in a sibling `impl<'a> super::EffectChecker<'a>` block.

use crate::ast::*;
use crate::manifest::CompileProfile;
use crate::token::Span;

use super::{
    verb_name, DeclaredEffects, Effect, EffectError, EffectErrorKind, EffectOrigin, EffectSet,
};

impl<'a> super::EffectChecker<'a> {
    /// Per-`ExternFunction` effect-set registration. Used at both the
    /// (now-dead) top-level `Item::ExternFunction` arm (with
    /// `block_attrs = &[]`) and the per-item arm inside an
    /// `Item::ExternBlock` (with `block_attrs = &b.attributes`).
    /// Block-level attributes are NOT pre-merged into per-item
    /// `attributes` — they're passed through here so the `@noblock`
    /// check sees the union of block + per-item attrs.
    pub(crate) fn register_extern_function_effects(
        &mut self,
        e: &ExternFunction,
        block_attrs: &[Attribute],
    ) {
        // ABI-keyed default effect set (trust-not-verify: extern has no body).
        // `extern "C"` → {blocks}; `extern "C-unwind"` → {blocks, panics}.
        // `@noblock` removes blocks from the default (e.g. a pure-CPU C++ fn).
        let builtin_span = Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        };
        let has_noblock = e.attributes.iter().any(|a| a.is_bare("noblock"))
            || block_attrs.iter().any(|a| a.is_bare("noblock"));
        let mut abi_defaults = EffectSet::new();
        match e.abi.as_str() {
            "C" if !has_noblock => {
                abi_defaults.add(
                    Effect {
                        verb: EffectVerbKind::Blocks,
                        resource: String::new(),
                    },
                    EffectOrigin::Direct(builtin_span.clone()),
                );
            }
            "C" => {}
            "C-unwind" => {
                if !has_noblock {
                    abi_defaults.add(
                        Effect {
                            verb: EffectVerbKind::Blocks,
                            resource: String::new(),
                        },
                        EffectOrigin::Direct(builtin_span.clone()),
                    );
                }
                // panics is always included for C-unwind (throws across FFI boundary).
                // @noblock cannot suppress panics.
                abi_defaults.add(
                    Effect {
                        verb: EffectVerbKind::Panics,
                        resource: String::new(),
                    },
                    EffectOrigin::Direct(builtin_span),
                );
            }
            _ => {} // other ABIs: no defaults until implemented
        }

        // Parse programmer-supplied annotations, then union with ABI defaults.
        let programmer_decl = self.parse_effect_list(&e.effects);
        let final_decl = match &programmer_decl {
            DeclaredEffects::Polymorphic | DeclaredEffects::PolymorphicWithFixed(_) => {
                // Polymorphic extern: unusual but accepted; ABI defaults dropped.
                programmer_decl.clone()
            }
            DeclaredEffects::Explicit(prog_set) => {
                let mut merged = abi_defaults;
                for te in &prog_set.effects {
                    merged.add(te.effect.clone(), te.origin.clone());
                }
                DeclaredEffects::Explicit(merged)
            }
            DeclaredEffects::None => {
                if abi_defaults.is_empty() {
                    DeclaredEffects::None
                } else {
                    DeclaredEffects::Explicit(abi_defaults)
                }
            }
        };

        // Profile-compatibility check: reject effects forbidden by the
        // active compile profile at the extern declaration site.
        if let DeclaredEffects::Explicit(ref set) = final_decl {
            for te in &set.effects {
                if let Some(forbidden_reason) = self.profile_forbids(&te.effect, &e.name, &e.abi) {
                    self.errors.push(EffectError {
                        message: forbidden_reason,
                        span: e.span.clone(),
                        kind: EffectErrorKind::ProfileViolation,
                        subtype_trace: None,
                    });
                }
            }
        }

        // Advisory linter hints for commonly-omitted effects.
        self.check_ffi_linter_hints(&e.name, &e.span, &final_decl);

        self.declared_effects.insert(e.name.clone(), final_decl);
        self.function_visibility.insert(e.name.clone(), true);
        self.function_spans.insert(e.name.clone(), e.span.clone());
        // Seed inferred_effects from the merged set so callers accumulate
        // the correct leaf effects (ABI defaults + programmer annotations).
        if let Some(DeclaredEffects::Explicit(ref set)) = self.declared_effects.get(&e.name) {
            self.inferred_effects.insert(e.name.clone(), set.clone());
        } else {
            self.inferred_effects
                .insert(e.name.clone(), EffectSet::new());
        }
    }

    fn check_ffi_linter_hints(&mut self, symbol: &str, span: &Span, decl: &DeclaredEffects) {
        // Normalize: take the last segment after any `.` separator, strip a
        // leading `_` that some platforms prepend (e.g. macOS `_malloc`).
        let base = symbol.rsplit('.').next().unwrap_or(symbol);
        let base = base.strip_prefix('_').unwrap_or(base);

        let declared_set: Option<&EffectSet> = match decl {
            DeclaredEffects::Explicit(s) => Some(s),
            DeclaredEffects::PolymorphicWithFixed(s) => Some(s),
            _ => None,
        };

        let has_verb = |verb: EffectVerbKind, resource: &str| -> bool {
            declared_set.is_some_and(|s| {
                s.effects.iter().any(|te| {
                    te.effect.verb == verb
                        && (resource.is_empty() || te.effect.resource == resource)
                })
            })
        };

        // Known-blocking symbols — suggest `blocks`.
        const KNOWN_BLOCKING: &[&str] = &[
            "sleep",
            "usleep",
            "nanosleep",
            "read",
            "write",
            "recv",
            "recvfrom",
            "recvmsg",
            "send",
            "sendto",
            "sendmsg",
            "accept",
            "accept4",
            "connect",
            "poll",
            "select",
            "pselect",
            "epoll_wait",
            "kevent",
            "waitpid",
            "wait",
            "wait4",
            "flock",
            "lockf",
            "pthread_mutex_lock",
            "pthread_cond_wait",
            "pthread_join",
            "open",
            "fopen",
            "openat",
            "creat",
            "close",
            "fsync",
            "fdatasync",
            "gethostbyname",
            "getaddrinfo",
        ];

        if KNOWN_BLOCKING.contains(&base) && !has_verb(EffectVerbKind::Blocks, "") {
            self.errors.push(EffectError {
                message: format!(
                    "FFI lint: '{}' is commonly blocking; consider adding `blocks` to its \
                     effect list (or `@noblock` to confirm it is non-blocking in this context)",
                    symbol
                ),
                span: span.clone(),
                kind: EffectErrorKind::FfiLintHint,
                subtype_trace: None,
            });
        }

        // Known-allocating symbols — suggest `allocates(Heap)`.
        const KNOWN_ALLOCATING: &[&str] = &[
            "malloc",
            "calloc",
            "realloc",
            "reallocarray",
            "strdup",
            "strndup",
            "asprintf",
            "vasprintf",
            "posix_memalign",
            "memalign",
            "aligned_alloc",
            "getaddrinfo",
        ];

        if KNOWN_ALLOCATING.contains(&base) && !has_verb(EffectVerbKind::Allocates, "Heap") {
            self.errors.push(EffectError {
                message: format!(
                    "FFI lint: '{}' is commonly allocating; consider adding `allocates(Heap)` \
                     to its effect list",
                    symbol
                ),
                span: span.clone(),
                kind: EffectErrorKind::FfiLintHint,
                subtype_trace: None,
            });
        }
    }

    fn profile_forbids(&self, effect: &Effect, fn_name: &str, abi: &str) -> Option<String> {
        let forbidden = match self.profile {
            CompileProfile::Default => return None,
            CompileProfile::Embedded => matches!(
                (&effect.verb, effect.resource.as_str()),
                (EffectVerbKind::Allocates, "Heap")
            ),
            CompileProfile::Kernel => matches!(
                &effect.verb,
                EffectVerbKind::Allocates
                    | EffectVerbKind::Panics
                    | EffectVerbKind::Blocks
                    | EffectVerbKind::Suspends
            ),
        };
        if forbidden {
            let effect_str = if effect.resource.is_empty() {
                verb_name(&effect.verb)
            } else {
                format!("{}({})", verb_name(&effect.verb), effect.resource)
            };
            Some(format!(
                "extern \"{}\" fn {} declares effect `{}`, which is forbidden by the '{}' profile",
                abi,
                fn_name,
                effect_str,
                self.profile.as_str(),
            ))
        } else {
            None
        }
    }
}
