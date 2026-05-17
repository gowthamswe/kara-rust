//! TypeEnv population: pass-1 walk over the program (plus the baked
//! stdlib + compiler intrinsics) that registers every struct, enum,
//! function, trait, impl, const, type alias, distinct type, opaque
//! foreign type, and extern function into `self.env`.
//!
//! Houses `build_type_env` (the driver), `collect_import_origins`,
//! the stdlib bootstrap (`register_baked_stdlib`,
//! `register_compiler_intrinsic_env`, `register_stdlib_impls`),
//! and the per-item-kind `env_add_*` registrars plus the
//! impl-coherence helpers (`impl_overlap_exists`, `register_builtin_impl`).
//! Lives in a sibling `impl<'a> super::TypeChecker<'a>` block.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

use super::env::{EnumInfo, FunctionSig, ImplAssocTypeEntry, ImplInfo, StructInfo, TraitInfo};
use super::types::{
    type_display, type_is_fully_concrete, FloatSize, IntSize, Type, UIntSize, VariantTypeInfo,
};
use super::{
    extract_derived_traits, extract_must_use_message, find_item_visibility, has_display_snake_case,
    normalize_bounds_into_where_clause, TypeErrorKind,
};

impl<'a> super::TypeChecker<'a> {
    // ── Build Type Environment (Pass 1) ─────────────────────────

    pub(super) fn build_type_env(&mut self) {
        // Two-step stdlib seeding (CR-202):
        //   1. Walk every item in `runtime/stdlib/*.kara` (baked into
        //      the binary via `prelude::STDLIB_PROGRAMS`) and register
        //      it through the same `env_add_*` paths user items use.
        //   2. Register the residual compiler-internal entries that
        //      have no syntactic representation in baked source —
        //      `impl_assoc_types` mappings, the `Iterator` parametric
        //      pseudo-struct, primitive operator impls, etc.
        self.register_baked_stdlib();
        self.register_compiler_intrinsic_env();

        let items: Vec<Item> = self.program.items.clone();

        // Stub pre-pass for self-referential shared types. Field-type
        // lowering inside `env_add_struct` / `env_add_enum` calls
        // `lower_type_expr`, which checks `env.structs` / `env.enums`
        // for `is_shared` to decide whether to return `Type::Shared(name)`
        // vs `Type::Named { name, args: [] }`. Without the stub, a
        // shared struct's own self-reference resolves before the parent
        // entry is inserted and falls through to `Type::Named` — later
        // uses of the same name resolve to `Type::Shared`, and the two
        // representations fail `types_compatible` with the visually
        // identical "expected 'Option<S>', found 'Option<S>'"
        // diagnostic. Insert empty-fields stubs first so the self-ref
        // path hits the populated entry; the real pass below overwrites
        // with the fully-lowered fields.
        for item in &items {
            match item {
                Item::StructDef(s) => {
                    let gp = Self::generic_param_names(&s.generic_params);
                    let derived_traits = extract_derived_traits(&s.attributes);
                    let must_use_message = extract_must_use_message(&s.attributes);
                    self.env.structs.insert(
                        s.name.clone(),
                        StructInfo {
                            generic_params: gp,
                            fields: Vec::new(),
                            derived_traits,
                            no_rc: s.no_rc,
                            is_shared: s.is_shared,
                            must_use_message,
                        },
                    );
                }
                Item::EnumDef(e) => {
                    let gp = Self::generic_param_names(&e.generic_params);
                    let derived_traits = extract_derived_traits(&e.attributes);
                    let must_use_message = extract_must_use_message(&e.attributes);
                    self.env.enums.insert(
                        e.name.clone(),
                        EnumInfo {
                            generic_params: gp,
                            variants: Vec::new(),
                            derived_traits,
                            is_shared: e.is_shared,
                            must_use_message,
                        },
                    );
                }
                _ => {}
            }
        }

        for item in &items {
            match item {
                Item::StructDef(s) => self.env_add_struct(s),
                Item::EnumDef(e) => self.env_add_enum(e),
                Item::Function(f) => self.env_add_function(f),
                Item::TraitDef(t) => self.env_add_trait(t),
                Item::TraitAlias(t) => self.env_add_trait_alias(t),
                Item::MarkerTrait(t) => self.env_add_marker_trait(t),
                Item::ImplBlock(i) => self.env_add_impl(i),
                Item::ConstDecl(c) => self.env_add_const(c),
                Item::TypeAlias(t) => self.env_add_type_alias(t),
                Item::ExternFunction(e) => self.env_add_extern_function(e),
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(f) => self.env_add_extern_function(f),
                            ExternItem::OpaqueType(o) => self.env_add_opaque_foreign_type(o),
                        }
                    }
                }
                Item::DistinctType(d) => self.env_add_distinct_type(d),
                _ => {}
            }
        }

        // Cross-module origins: record every imported item's declared
        // visibility in its origin module so `check_signature_visibility`
        // and `infer_field_access` can enforce three-level rules across
        // modules. Silent when `tree` is unset (single-file mode).
        self.collect_import_origins();
    }

    /// Walk `self.program.items` imports, look each target up in the
    /// `ProgramTree`, and stash the (origin path, origin visibility) pair
    /// under the locally-bound name. CR-24 slice 6 (slice 7 extension:
    /// chases `pub import` re-export chains so cross-module field access and
    /// signature-leak checks see the canonical defining module, not the
    /// re-exporter).
    fn collect_import_origins(&mut self) {
        let Some(tree) = self.tree else {
            return;
        };
        // Items collected for env_add_* registration. Done in two
        // passes so the iteration borrow on `self.program.items` ends
        // before the env_add_* methods take `&mut self`.
        let mut imported_items: Vec<(String, crate::ast::Item)> = Vec::new();
        for item in &self.program.items {
            let Item::Import(imp) = item else { continue };
            for ii in &imp.items {
                // Canonical origin walks `pub import` re-exports to the
                // defining module. Falls back to the direct target when no
                // matching item exists (E0225 handles that case in the
                // resolver — typechecker skips the entry silently).
                let Some((origin_path, origin_name)) =
                    crate::module::canonical_origin(tree, &imp.path, &ii.name)
                else {
                    continue;
                };
                let Some(&origin_id) = tree.graph.by_path.get::<[String]>(&origin_path) else {
                    continue;
                };
                let origin_module = tree.module(origin_id);
                if let Some(vis) = find_item_visibility(origin_module, &origin_name) {
                    let bound = ii.alias.clone().unwrap_or_else(|| ii.name.clone());
                    self.type_origins
                        .insert(bound, (origin_path, origin_name.clone(), vis));
                }
                // Theme 4 follow-up (2026-05-10) — pull the imported
                // item's full definition into the local env so per-
                // module typecheck sees imported structs / enums /
                // traits as first-class types. Without this, struct
                // literals on imported types fired `E0207 NotAStruct`
                // even though resolution succeeded. The original CR-24
                // slice-6 surface only carried `(origin_path, name,
                // vis)` in `type_origins`; the full definition is
                // needed for struct-literal validation, variant
                // construction, and trait-method dispatch.
                for oitem in &origin_module.items {
                    let matches = match oitem {
                        Item::StructDef(s) => s.name == origin_name,
                        Item::EnumDef(e) => e.name == origin_name,
                        Item::TraitDef(t) => t.name == origin_name,
                        Item::TraitAlias(t) => t.name == origin_name,
                        Item::MarkerTrait(t) => t.name == origin_name,
                        _ => false,
                    };
                    if matches {
                        imported_items.push((
                            ii.alias.clone().unwrap_or_else(|| ii.name.clone()),
                            oitem.clone(),
                        ));
                        break;
                    }
                }
            }
        }
        for (bound_name, item) in imported_items {
            // Skip when an item with the bound name is already registered
            // — local definitions and stdlib bakeds win over imports.
            match &item {
                Item::StructDef(s) => {
                    if self.env.structs.contains_key(&bound_name) {
                        continue;
                    }
                    // Re-bind the struct under its locally-bound name so
                    // `infer_struct_literal`'s lookup succeeds. The
                    // canonical name is preserved in `type_origins` for
                    // visibility / canonicalization checks.
                    let mut local_def = s.clone();
                    local_def.name = bound_name;
                    self.env_add_struct(&local_def);
                }
                Item::EnumDef(e) => {
                    if self.env.enums.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = e.clone();
                    local_def.name = bound_name;
                    self.env_add_enum(&local_def);
                }
                Item::TraitDef(t) => {
                    if self.env.traits.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = t.clone();
                    local_def.name = bound_name;
                    self.env_add_trait(&local_def);
                }
                Item::TraitAlias(t) => {
                    if self.env.traits.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = t.clone();
                    local_def.name = bound_name;
                    self.env_add_trait_alias(&local_def);
                }
                Item::MarkerTrait(t) => {
                    if self.env.traits.contains_key(&bound_name) {
                        continue;
                    }
                    let mut local_def = t.clone();
                    local_def.name = bound_name;
                    self.env_add_marker_trait(&local_def);
                }
                _ => {}
            }
        }
    }

    /// Walk every item in `runtime/stdlib/*.kara` (baked into the
    /// binary via [`crate::prelude::STDLIB_PROGRAMS`]) and register it
    /// through the same `env_add_*` paths user items use.
    ///
    /// CR-202 incrementally migrated the prelude surface to baked
    /// source; this function is the single entry point that pulls
    /// every type, trait, and impl declaration out of stdlib source
    /// files into the typechecker's environment. See
    /// `runtime/stdlib/` for the authoritative declarations.
    fn register_baked_stdlib(&mut self) {
        let baked: Vec<Item> = crate::prelude::STDLIB_PROGRAMS
            .iter()
            .flat_map(|(_, p)| p.items.iter().cloned())
            .collect();
        for item in &baked {
            match item {
                Item::Function(f) => self.env_add_function(f),
                Item::StructDef(s) => self.env_add_struct(s),
                Item::EnumDef(e) => self.env_add_enum(e),
                Item::TraitDef(t) => self.env_add_trait(t),
                Item::ImplBlock(i) => self.env_add_impl(i),
                Item::TraitAlias(_)
                | Item::MarkerTrait(_)
                | Item::ConstDecl(_)
                | Item::TypeAlias(_)
                | Item::ExternFunction(_)
                | Item::ExternBlock(_)
                | Item::DistinctType(_)
                | Item::EffectResource(_)
                | Item::EffectGroup(_)
                | Item::EffectVerbDecl(_)
                | Item::UseDecl(_)
                | Item::Import(_)
                | Item::LayoutDef(_)
                | Item::AliasDecl(_)
                | Item::IndependentDecl(_) => {
                    // Not yet exercised by baked stdlib source — broaden
                    // the match if a future stdlib file uses one of these
                    // item kinds.
                }
            }
        }
    }

    /// Register the residual compiler-internal entries that have no
    /// syntactic representation in baked source. CR-202 slice 6.5
    /// scrubbed this function down from the original
    /// `register_builtin_types` once the migratable surface had moved
    /// to `runtime/stdlib/*.kara`. What remains is:
    ///
    /// - `impl_assoc_types` mappings keyed `(type, assoc_name) -> Type`
    ///   that thread collection types to their iterator element type.
    ///   `Map[K, V]` yields `(K, V)`, the rest yield `T`.
    /// - The `Iterator` and `Array` parametric pseudo-structs in
    ///   `env.structs`. `Iterator` is a trait per design.md but is
    ///   also treated as a parametric pseudo-type at this layer so
    ///   `for x in v.iter()` resolves through the same
    ///   `impl_assoc_types` path as concrete collections. `Array[T]`
    ///   is a built-in primitive (lowered specially in
    ///   `lower_path_type`); a separate primitive-vs-struct design CR
    ///   would migrate it to baked source.
    /// - The `Range*` family of typechecker-internal iteration types.
    ///   Constructed from `a..b` syntax; never user-referenced as
    ///   `Range[T]`, so a baked struct adds no value.
    /// - Module-path free-function aliases (`env.args`, `env.var`,
    ///   `process.exit`). These cannot be expressed as `impl Env { fn args() }`
    ///   blocks because the lowercase identifier (`env`) doesn't name a
    ///   type; they're a syntactically distinct surface that aliases the
    ///   capitalized `Env.args` / `Env.var` (which now live in baked source).
    ///   The full ambient effect-resource surface — Stdin, Stdout, Stderr,
    ///   FileSystem, Env, Clock, RandomSource — has migrated to
    ///   `runtime/stdlib/io.kara` via the companion-struct pattern; the
    ///   `EffectResource` symbol kind and the baked struct coexist because
    ///   baked source bypasses the resolver, so each resource stays a
    ///   `SymbolKind::EffectResource` for `with_provider[R]` purposes while
    ///   `env.structs` / `env.impls` carries the type+method shape for
    ///   `infer_path_type` lookups.
    /// - The primitive operator impl table via [`Self::register_stdlib_impls`]
    ///   (`impl Add for i32`, `impl Eq for u8`, the numeric widening
    ///   `From` impls, …). Documented as permanently programmatic — a
    ///   compiler-internal dispatch table, not user-readable type
    ///   declarations.
    fn register_compiler_intrinsic_env(&mut self) {
        let t = || Type::TypeParam("T".to_string());
        let k = || Type::TypeParam("K".to_string());
        let v = || Type::TypeParam("V".to_string());

        // Iterator / Array parametric pseudo-structs (see fn doc).
        // The Iterator pseudo-struct carries the `#[must_use]` message
        // slice 2 of the `#[must_use]` mandate would have written into
        // baked source if `Iterator` had a syntactic struct declaration
        // there. Per slice 2's design, the annotation lands here in
        // `register_compiler_intrinsic_env` alongside the slice 4
        // `StructInfo.must_use_message` field that consumes it. Every
        // iterator-adapter return type from `src/typechecker/stdlib_iter.rs`
        // (`map`, `filter`, `take`, `skip`, `chain`, `zip`, `enumerate`,
        // `rev`, `flatten`, `flat_map`, `inspect`, `cycle`, `step_by`,
        // `Vec.iter()`, …) collapses to `Type::Named { name: "Iterator", … }`,
        // so this single annotation propagates the discard-site warning
        // across the whole adapter surface.
        let iterator_must_use_msg = "discarding the iterator drops every \
             adapter without running it — chain a terminal method or \
             bind the result"
            .to_string();
        for name in &["Array", "Iterator"] {
            let must_use_message = if *name == "Iterator" {
                Some(iterator_must_use_msg.clone())
            } else {
                None
            };
            self.env
                .structs
                .entry(name.to_string())
                .or_insert_with(|| StructInfo {
                    generic_params: vec!["T".to_string()],
                    fields: vec![],
                    derived_traits: HashSet::new(),
                    no_rc: false,
                    is_shared: false,
                    must_use_message,
                });
            self.env.impl_assoc_types.insert(
                (name.to_string(), "Item".to_string()),
                ImplAssocTypeEntry {
                    ty: t(),
                    gat_params: vec![],
                },
            );
        }

        // Iterator-element-type (`Item`) mappings for baked collection
        // types. The structs themselves are baked; the assoc-type
        // mapping has no syntactic representation in baked source.
        for name in &["Vec", "VecDeque", "SortedSet", "Set", "Peekable", "Slice"] {
            self.env.impl_assoc_types.insert(
                (name.to_string(), "Item".to_string()),
                ImplAssocTypeEntry {
                    ty: t(),
                    gat_params: vec![],
                },
            );
        }
        self.env.impl_assoc_types.insert(
            ("Map".to_string(), "Item".to_string()),
            ImplAssocTypeEntry {
                ty: Type::Tuple(vec![k(), v()]),
                gat_params: vec![],
            },
        );

        // Range family — typechecker-internal types constructed from
        // `a..b` syntax. Both struct shape and assoc-type mapping
        // registered here.
        for name in &[
            "Range",
            "RangeInclusive",
            "RangeFrom",
            "RangeTo",
            "RangeToInclusive",
        ] {
            self.env
                .structs
                .entry(name.to_string())
                .or_insert_with(|| StructInfo {
                    generic_params: vec!["T".to_string()],
                    fields: vec![],
                    derived_traits: HashSet::new(),
                    no_rc: false,
                    is_shared: false,
                    must_use_message: None,
                });
            self.env.impl_assoc_types.insert(
                (name.to_string(), "Item".to_string()),
                ImplAssocTypeEntry {
                    ty: t(),
                    gat_params: vec![],
                },
            );
        }

        // ── Standard I/O function signatures ───────────────────────────────────
        //
        // The capitalized I/O resource methods (`Stdin.read_line`,
        // `Stdout.println`, `FileSystem.write`, `Env.args`, …) live in baked
        // stdlib source (`runtime/stdlib/io.kara`) as
        // `impl <Resource> { #[compiler_builtin] fn ... }` blocks. The
        // signatures flow through `register_baked_stdlib` → `env_add_impl` →
        // `env.impls`, found by `resolve_path_type`'s impl lookup
        // (`infer_path_type` line ~7227) before the
        // `env.functions.get("Resource.method")` fallback.
        //
        // The lowercase module-path forms `env.args()` / `env.var(name)` stay
        // here — they're aliases that share dispatch with the capitalized
        // form (interpreter routes `env` → `Env` via the alias map at
        // `eval_method_call`) but the lowercase surface has no syntactic
        // representation as an `impl Env { fn args() }` block.

        let vec_string = Type::Named {
            name: "Vec".to_string(),
            args: vec![Type::Str],
        };
        let args_sig = FunctionSig {
            generic_params: vec![],
            param_names: vec![],
            params: vec![],
            return_type: vec_string,
            where_clause: None,
        };
        self.env.functions.insert("env.args".to_string(), args_sig);

        let result_str_var = Type::Named {
            name: "Result".to_string(),
            args: vec![
                Type::Str,
                Type::Named {
                    name: "VarError".to_string(),
                    args: vec![],
                },
            ],
        };
        let var_sig = FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("name".to_string())],
            params: vec![Type::Str],
            return_type: result_str_var,
            where_clause: None,
        };
        self.env.functions.insert("env.var".to_string(), var_sig);

        // `env.set(name, value)` — lowercase alias for `Env.set`. The
        // capitalized form lives in baked stdlib (`runtime/stdlib/io.kara`)
        // alongside `Env.var` / `Env.args`; this lowercase entry mirrors the
        // `env.var` registration above. Carries `writes(Env)` (seeded in
        // `effectchecker::check`) so callers must declare it.
        let set_sig = FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("name".to_string()), Some("value".to_string())],
            params: vec![Type::Str, Type::Str],
            return_type: Type::Unit,
            where_clause: None,
        };
        self.env.functions.insert("env.set".to_string(), set_sig);

        // Register process.exit in the function table
        self.env.functions.insert(
            "process.exit".to_string(),
            FunctionSig {
                generic_params: vec![],
                param_names: vec![Some("code".to_string())],
                params: vec![Type::Int(IntSize::I32)],
                return_type: Type::Never,
                where_clause: None,
            },
        );

        // ── Stats namespace ──────────────────────────────────────────────────
        // CR-202 slice 6.3: every Stats method now lives in baked source as
        // `impl Stats { #[compiler_builtin] fn ... }`. See
        // `runtime/stdlib/stats.kara`.

        // ── Regex namespace ──────────────────────────────────────────────────
        // CR-202 slice 6.3: every Regex method now lives in baked source as
        // `impl Regex { #[compiler_builtin] fn ... }`. See
        // `runtime/stdlib/regex.kara`. Instance-method calls
        // (`r.is_match(s)`, …) still route through `infer_regex_method` /
        // `eval_regex_method`; only the path-call form `Regex.compile(...)`
        // and the env.functions surface migrated.

        // ── std.http namespace ───────────────────────────────────────────────
        // CR-202 slice 6.3: every Client / Response / HttpError method now
        // lives in baked source as `impl <Type> { #[compiler_builtin] fn ... }`.
        // See `runtime/stdlib/http.kara`. Instance-method calls still route
        // through `infer_http_*_method` / `eval_http_*_method`; only
        // `Client.new()` (associated) and the env.functions surface migrated.

        // ── std.encoding namespace (Base64 / Hex / Url) ──────────────────────
        // CR-202 slice 6.3: every Base64 / Hex / Url method now lives in
        // baked source as `impl <Type> { #[compiler_builtin] fn ... }`.
        // See `runtime/stdlib/encoding.kara`. Interpreter dispatches each
        // call by matching on the path string in `eval_encoding_fn`.

        // `register_stdlib_traits` retired — every trait it registered
        // moved to baked source under `runtime/stdlib/*.kara` across
        // CR-202 slices 5a–5l, 6.2a–6.2e. The only remaining hardcoded
        // trait registration is the `Iterator` / `IntoIterator` pseudo-
        // struct + assoc-type pair below (slice 6.2d migrated the trait
        // shape; the pseudo-struct stays in code).
        self.register_stdlib_impls();
    }

    /// Register stdlib trait impls for primitives, String, and F32/F64 wrappers.
    /// Operator dispatch in Step 6 keys off these. Generic-target impls
    /// (Vec/Option/Result Eq/Ord) are deferred — they need bound checking
    /// against type arguments, which the impl table doesn't model yet.
    fn register_stdlib_impls(&mut self) {
        // Method-signature builders. All operator methods are homogeneous in v1
        // (`fn op(self, rhs: Self) -> Self`). Eq/Ord return bool / Ordering.
        let binop = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("rhs".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: ty.clone(),
            where_clause: None,
        };
        let unop = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into())],
            params: vec![ty.clone()],
            return_type: ty.clone(),
            where_clause: None,
        };
        let eq_sig = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("other".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: Type::Bool,
            where_clause: None,
        };
        let ord_sig = |ty: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("self".into()), Some("other".into())],
            params: vec![ty.clone(), ty.clone()],
            return_type: Type::Named {
                name: "Ordering".into(),
                args: vec![],
            },
            where_clause: None,
        };

        let signed_ints: &[(&str, Type)] = &[
            ("i8", Type::Int(IntSize::I8)),
            ("i16", Type::Int(IntSize::I16)),
            ("i32", Type::Int(IntSize::I32)),
            ("i64", Type::Int(IntSize::I64)),
        ];
        let unsigned_ints: &[(&str, Type)] = &[
            ("u8", Type::UInt(UIntSize::U8)),
            ("u16", Type::UInt(UIntSize::U16)),
            ("u32", Type::UInt(UIntSize::U32)),
            ("u64", Type::UInt(UIntSize::U64)),
            ("usize", Type::UInt(UIntSize::Usize)),
        ];
        let floats: &[(&str, Type)] = &[
            ("f32", Type::Float(FloatSize::F32)),
            ("f64", Type::Float(FloatSize::F64)),
        ];
        let f_wrappers: &[(&str, Type)] = &[
            (
                "F32",
                Type::Named {
                    name: "F32".into(),
                    args: vec![],
                },
            ),
            (
                "F64",
                Type::Named {
                    name: "F64".into(),
                    args: vec![],
                },
            ),
        ];

        let all_ints: Vec<(&str, Type)> = signed_ints
            .iter()
            .chain(unsigned_ints.iter())
            .cloned()
            .collect();
        let all_numeric: Vec<(&str, Type)> =
            all_ints.iter().chain(floats.iter()).cloned().collect();
        let signed_numeric: Vec<(&str, Type)> =
            signed_ints.iter().chain(floats.iter()).cloned().collect();

        // Arithmetic on all numeric primitives (binary).
        for (target, ty) in &all_numeric {
            for (trait_name, method) in [
                ("Add", "add"),
                ("Sub", "sub"),
                ("Mul", "mul"),
                ("Div", "div"),
                ("Rem", "rem"),
            ] {
                self.register_builtin_impl(trait_name, target, vec![(method, binop(ty))]);
            }
        }
        // Neg on signed integers and floats only.
        for (target, ty) in &signed_numeric {
            self.register_builtin_impl("Neg", target, vec![("neg", unop(ty))]);
        }
        // Bitwise BitAnd/BitOr/BitXor on integers + bool.
        for (target, ty) in all_ints
            .iter()
            .chain(std::iter::once(&("bool", Type::Bool)))
        {
            for (trait_name, method) in [
                ("BitAnd", "bitand"),
                ("BitOr", "bitor"),
                ("BitXor", "bitxor"),
            ] {
                self.register_builtin_impl(trait_name, target, vec![(method, binop(ty))]);
            }
        }
        // Shifts on integers only (rhs = Self per v1 homogeneity rule).
        for (target, ty) in &all_ints {
            for (trait_name, method) in [("Shl", "shl"), ("Shr", "shr")] {
                self.register_builtin_impl(trait_name, target, vec![(method, binop(ty))]);
            }
        }
        // Not on integers + bool.
        for (target, ty) in all_ints
            .iter()
            .chain(std::iter::once(&("bool", Type::Bool)))
        {
            self.register_builtin_impl("Not", target, vec![("not", unop(ty))]);
        }
        // Eq + Ord on integers, bool, char, String, F32/F64 wrappers.
        // Floats (f32/f64) deliberately excluded — IEEE NaN breaks Eq/Ord.
        let eq_ord_targets: Vec<(&str, Type)> = all_ints
            .iter()
            .cloned()
            .chain(std::iter::once(("bool", Type::Bool)))
            .chain(std::iter::once(("char", Type::Char)))
            .chain(std::iter::once(("String", Type::Str)))
            .chain(f_wrappers.iter().cloned())
            .collect();
        // `ne`/`lt`/`le`/`gt`/`ge` share the bool-returning shape that
        // `eq_sig` produces, so reuse it for them. `cmp` is the only Ord
        // method with the Ordering-returning shape. Registering these makes
        // the names directly callable (e.g. `i32.lt(a, b)`) alongside the
        // operator-lowered form.
        for (target, ty) in &eq_ord_targets {
            let cmp_bool = eq_sig(ty);
            self.register_builtin_impl(
                "Eq",
                target,
                vec![("eq", cmp_bool.clone()), ("ne", cmp_bool.clone())],
            );
            self.register_builtin_impl(
                "Ord",
                target,
                vec![
                    ("cmp", ord_sig(ty)),
                    ("lt", cmp_bool.clone()),
                    ("le", cmp_bool.clone()),
                    ("gt", cmp_bool.clone()),
                    ("ge", cmp_bool),
                ],
            );
        }
        // Add for String — heap concatenation. Effect tracking (allocates(Heap))
        // wired in Step 6 when operator lowering routes through this impl.
        self.register_builtin_impl("Add", "String", vec![("add", binop(&Type::Str))]);

        // Numeric widening: register `impl From[Source] for Target` for every
        // lossless source→target pair. `target.from(value)` then dispatches
        // through this table; the source type disambiguates between impls
        // sharing a target.
        let from_sig = |source: &Type, target: &Type| FunctionSig {
            generic_params: vec![],
            param_names: vec![Some("value".into())],
            params: vec![source.clone()],
            return_type: target.clone(),
            where_clause: None,
        };
        let widening_pairs: &[(&str, Type, &str, Type)] = &[
            // signed → signed
            ("i8", Type::Int(IntSize::I8), "i16", Type::Int(IntSize::I16)),
            ("i8", Type::Int(IntSize::I8), "i32", Type::Int(IntSize::I32)),
            ("i8", Type::Int(IntSize::I8), "i64", Type::Int(IntSize::I64)),
            (
                "i16",
                Type::Int(IntSize::I16),
                "i32",
                Type::Int(IntSize::I32),
            ),
            (
                "i16",
                Type::Int(IntSize::I16),
                "i64",
                Type::Int(IntSize::I64),
            ),
            (
                "i32",
                Type::Int(IntSize::I32),
                "i64",
                Type::Int(IntSize::I64),
            ),
            // unsigned → unsigned
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "u16",
                Type::UInt(UIntSize::U16),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "u32",
                Type::UInt(UIntSize::U32),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "u64",
                Type::UInt(UIntSize::U64),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "usize",
                Type::UInt(UIntSize::Usize),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "u32",
                Type::UInt(UIntSize::U32),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "u64",
                Type::UInt(UIntSize::U64),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "usize",
                Type::UInt(UIntSize::Usize),
            ),
            (
                "u32",
                Type::UInt(UIntSize::U32),
                "u64",
                Type::UInt(UIntSize::U64),
            ),
            // unsigned → wider signed (always lossless)
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "i16",
                Type::Int(IntSize::I16),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "i32",
                Type::Int(IntSize::I32),
            ),
            (
                "u8",
                Type::UInt(UIntSize::U8),
                "i64",
                Type::Int(IntSize::I64),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "i32",
                Type::Int(IntSize::I32),
            ),
            (
                "u16",
                Type::UInt(UIntSize::U16),
                "i64",
                Type::Int(IntSize::I64),
            ),
            (
                "u32",
                Type::UInt(UIntSize::U32),
                "i64",
                Type::Int(IntSize::I64),
            ),
            // float widening
            (
                "f32",
                Type::Float(FloatSize::F32),
                "f64",
                Type::Float(FloatSize::F64),
            ),
        ];
        for (_src_name, src_ty, tgt_name, tgt_ty) in widening_pairs {
            self.register_builtin_impl("From", tgt_name, vec![("from", from_sig(src_ty, tgt_ty))]);
        }
    }

    fn env_add_struct(&mut self, s: &StructDef) {
        let gp = Self::generic_param_names(&s.generic_params);
        let fields: Vec<(String, Type, bool)> = s
            .fields
            .iter()
            .map(|f| (f.name.clone(), self.lower_type_expr(&f.ty, &gp), f.is_pub))
            .collect();
        let derived_traits = extract_derived_traits(&s.attributes);
        let must_use_message = extract_must_use_message(&s.attributes);
        self.env.structs.insert(
            s.name.clone(),
            StructInfo {
                generic_params: gp,
                fields,
                derived_traits,
                no_rc: s.no_rc,
                is_shared: s.is_shared,
                must_use_message,
            },
        );
    }

    fn env_add_enum(&mut self, e: &EnumDef) {
        let gp = Self::generic_param_names(&e.generic_params);
        let variants: Vec<(String, VariantTypeInfo)> = e
            .variants
            .iter()
            .map(|v| {
                let vtype = match &v.kind {
                    VariantKind::Unit => VariantTypeInfo::Unit,
                    VariantKind::Tuple(types) => VariantTypeInfo::Tuple(
                        types.iter().map(|t| self.lower_type_expr(t, &gp)).collect(),
                    ),
                    VariantKind::Struct(fields) => VariantTypeInfo::Struct(
                        fields
                            .iter()
                            .map(|f| (f.name.clone(), self.lower_type_expr(&f.ty, &gp)))
                            .collect(),
                    ),
                };
                (v.name.clone(), vtype)
            })
            .collect();
        let derived_traits = extract_derived_traits(&e.attributes);
        if has_display_snake_case(&e.attributes) {
            self.display_snake_case_enums.insert(e.name.clone());
        }
        let must_use_message = extract_must_use_message(&e.attributes);
        self.env.enums.insert(
            e.name.clone(),
            EnumInfo {
                generic_params: gp,
                variants,
                derived_traits,
                is_shared: e.is_shared,
                must_use_message,
            },
        );
    }

    fn env_add_function(&mut self, f: &Function) {
        let gp = Self::generic_param_names(&f.generic_params);
        let param_names: Vec<Option<String>> = f
            .params
            .iter()
            .map(|p| p.name().map(|s| s.to_string()))
            .collect();
        let params: Vec<Type> = f
            .params
            .iter()
            .map(|p| self.lower_type_expr(&p.ty, &gp))
            .collect();
        let return_type = f
            .return_type
            .as_ref()
            .map(|t| self.lower_type_expr(t, &gp))
            .unwrap_or(Type::Unit);
        self.env.functions.insert(
            f.name.clone(),
            FunctionSig {
                generic_params: gp,
                param_names,
                params,
                return_type,
                // Normalize inline param bounds into the where-clause so the
                // call-site discharge engine sees both forms uniformly.
                // Slice 0.a, sub-step 1 of monomorphized collections prereq.
                where_clause: normalize_bounds_into_where_clause(
                    &f.generic_params,
                    &f.where_clause,
                ),
            },
        );
        if f.attributes.iter().any(|a| a.name == "compiler_builtin") {
            self.env.compiler_builtins.insert(f.name.clone());
        }
        // Slice 4 of the `#[must_use]` mandate: record the attribute on
        // the free function so the discard-site walker can flag callers
        // that drop the return value at statement position. Keyed by
        // the function's bare name; impl-method must-use is keyed by
        // `"TargetType.method"` in `env_add_impl` below.
        if let Some(msg) = extract_must_use_message(&f.attributes) {
            self.env
                .must_use_functions
                .insert(f.name.clone(), Some(msg));
        }
    }

    fn env_add_impl(&mut self, imp: &ImplBlock) {
        // Impl-level generic params are in scope when lowering method
        // signatures so `T` in `impl[T] Box[T] { fn echo(v: T) -> T }` is
        // recognized as a `Type::TypeParam("T")` rather than a `Type::Named
        // { "T", [] }` fallback. The method's own generic params extend the
        // scope; the per-method `generic_params` field on `FunctionSig`
        // continues to record only the method's own names so generic-call
        // inference at the use site doesn't accidentally rebind the
        // impl-level params.
        let impl_gp_names: Vec<String> = imp
            .generic_params
            .as_ref()
            .map(|gp| gp.params.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default();

        // Slice 1b: `impl Foo { ... }` and `impl Trait for Foo` are both
        // rejected when `Foo` is an opaque foreign type — opaque types have
        // no Kāra-visible methods or trait implementations. We check this
        // before `lower_type_expr` is called on the target so the user
        // doesn't see a duplicate `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`
        // diagnostic on the same span. Bail before `add_impl` so the impl
        // doesn't register and produce downstream confusion.
        if let TypeKind::Path(path) = &imp.target_type.kind {
            if path.segments.len() == 1 {
                let target_name = &path.segments[0];
                if self.env.opaque_foreign_types.contains(target_name) {
                    let prefix = match &imp.trait_name {
                        Some(tp) => format!(
                            "`impl {} for {}`",
                            tp.segments.last().cloned().unwrap_or_default(),
                            target_name
                        ),
                        None => format!("`impl {}`", target_name),
                    };
                    self.type_error(
                        format!(
                            "error[E_OPAQUE_TYPE_NO_INHERENT_OR_TRAIT_IMPLS]: cannot \
                             write {prefix} — opaque foreign type '{target_name}' has \
                             no Kāra-visible methods or trait implementations. Wrap \
                             '{target_name}' in a Kāra-side newtype (e.g. `shared \
                             struct {target_name}Handle {{ p: ref {target_name} }}`) \
                             and impl on the wrapper instead"
                        ),
                        imp.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    return;
                }
            }
        }

        // Lower the target type-expression through the standard pipeline so
        // type aliases canonicalize at registration time (`type MyOpt =
        // Option[Ordering]; impl Foo for MyOpt` resolves to target_type
        // "Option" + target_args [Ordering] before insertion). Theme-4
        // slice — see `phase-4-interpreter.md` § `impl Option[Ordering]`.
        let lowered_target = self.lower_type_expr(&imp.target_type, &impl_gp_names);
        let (type_name, target_args) = match &lowered_target {
            Type::Named { name, args } => {
                // Specialized impls store the concrete arg vector;
                // generic-on-name impls (anything containing a TypeParam
                // recursively) collapse to empty target_args so the
                // args-match rule treats them as wildcard-match.
                let concrete = !args.is_empty() && args.iter().all(type_is_fully_concrete);
                if concrete {
                    (name.clone(), args.clone())
                } else {
                    (name.clone(), Vec::new())
                }
            }
            // Shared structs: `impl S { ... }` for a `shared struct S`
            // registers under the bare name (no target_args — shared
            // structs are non-generic at v1 per design.md § Part 5).
            // Sub-item 2 audit miss caught during sub-item 3a.
            Type::Shared(name) => (name.clone(), Vec::new()),
            // Non-path target types (`impl Foo for (i32, i32)` etc.) are
            // unsupported in v1; bail without registering. Matches the
            // pre-Theme-4 behavior of the path-only short-circuit.
            _ => return,
        };

        let trait_name = imp
            .trait_name
            .as_ref()
            .and_then(|p| p.segments.last().cloned());

        let mut methods = HashMap::new();
        for item in &imp.items {
            let method = match item {
                ImplItem::Method(m) => m,
                ImplItem::AssocType(_) => continue,
            };
            let method_gp = Self::generic_param_names(&method.generic_params);
            let mut lowering_scope = impl_gp_names.clone();
            lowering_scope.extend(method_gp.iter().cloned());
            let param_names: Vec<Option<String>> = method
                .params
                .iter()
                .map(|p: &Param| p.name().map(|s| s.to_string()))
                .collect();
            let params: Vec<Type> = method
                .params
                .iter()
                .map(|p| self.lower_type_expr(&p.ty, &lowering_scope))
                .collect();
            let return_type = method
                .return_type
                .as_ref()
                .map(|t| self.lower_type_expr(t, &lowering_scope))
                .unwrap_or(Type::Unit);
            methods.insert(
                method.name.clone(),
                FunctionSig {
                    generic_params: method_gp,
                    param_names,
                    params,
                    return_type,
                    // Method-level inline bounds normalize alongside the
                    // method's own where-clause; impl-level inline bounds
                    // are tracked separately on `ImplInfo.generic_params`
                    // and discharged by `impl_bounds_discharge` at the
                    // dispatch site — NOT folded in here.
                    where_clause: normalize_bounds_into_where_clause(
                        &method.generic_params,
                        &method.where_clause,
                    ),
                },
            );
            // Slice 4 of the `#[must_use]` mandate: register impl
            // methods under the `"TargetType.method"` key shape that
            // `method_callee_types` / `bare_assoc_fn_targets` already
            // produce at call sites, so the discard-site walker can do
            // a single lookup. Both inherent impls and trait-impl
            // methods register; this covers inherent annotations and
            // impl-side overrides of trait defaults. Trait-declaration
            // `#[must_use]` registers separately through `env_add_trait`'s
            // default-body path (where applicable) and via the
            // future trait-attribute-inheritance follow-up.
            if let Some(msg) = extract_must_use_message(&method.attributes) {
                self.env
                    .must_use_functions
                    .insert(format!("{}.{}", type_name, method.name), Some(msg));
            }
        }

        // Theme-4 overlap check: reject coexistence of generic-on-name and
        // specialized impls for the same `(trait_name, target_type)` pair,
        // and reject duplicate specialized impls on the same concrete args.
        // Generic-vs-generic and same-args-duplicate cases are pre-existing
        // trait-coherence concerns left unchanged. See
        // `phase-4-interpreter.md` § `impl Option[Ordering]` for the
        // locked design rationale (rejection over Rust-style
        // specialization).
        if self.impl_overlap_exists(&trait_name, &type_name, &target_args) {
            self.type_error(
                format!(
                    "conflicting impl: another `impl{} {}{}` already exists; v1 \
                     does not support generic-vs-specialized impl overlap on the \
                     same trait + target",
                    trait_name
                        .as_deref()
                        .map(|t| format!(" {} for", t))
                        .unwrap_or_default(),
                    type_name,
                    if target_args.is_empty() {
                        String::new()
                    } else {
                        let rendered = target_args
                            .iter()
                            .map(type_display)
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("[{}]", rendered)
                    },
                ),
                imp.span.clone(),
                TypeErrorKind::ConflictingImpl,
            );
            return;
        }

        self.env.add_impl(ImplInfo {
            target_type: type_name,
            target_args,
            trait_name,
            methods,
            generic_params: imp.generic_params.clone(),
            where_clause: imp.where_clause.clone(),
        });
    }

    /// Theme-4 overlap detection. Returns `true` iff registering an impl
    /// with `(trait_name, target_type, target_args)` would conflict with
    /// an already-registered impl on the same `(trait_name, target_type)`
    /// pair under the v1 rule: generic-on-name (`target_args.is_empty()`)
    /// cannot coexist with any specialized variant, and two specialized
    /// variants cannot vector-equal on `target_args`. Anything else
    /// (different concrete instantiations, different traits) is fine.
    fn impl_overlap_exists(
        &self,
        trait_name: &Option<String>,
        target_type: &str,
        target_args: &[Type],
    ) -> bool {
        for existing in &self.env.impls {
            if existing.trait_name != *trait_name || existing.target_type != target_type {
                continue;
            }
            let existing_empty = existing.target_args.is_empty();
            let new_empty = target_args.is_empty();
            if existing_empty != new_empty {
                // generic-on-name + specialized — overlap
                return true;
            }
            if !existing_empty && existing.target_args == target_args {
                // two specialized impls on the same concrete instantiation
                return true;
            }
        }
        false
    }

    /// Register a built-in stdlib impl programmatically (no AST source).
    /// Used by `register_stdlib_impls` to seed primitive operator impls.
    /// Compiler-internal stdlib impls are unconditional and registered
    /// with empty `target_args` (generic-on-name) so primitive operator
    /// dispatch (`1 + 2` etc.) continues to apply uniformly.
    #[allow(dead_code)]
    fn register_builtin_impl(
        &mut self,
        trait_name: &str,
        target_type: &str,
        methods: Vec<(&str, FunctionSig)>,
    ) {
        let methods = methods
            .into_iter()
            .map(|(n, sig)| (n.to_string(), sig))
            .collect();
        self.env.add_impl(ImplInfo {
            target_type: target_type.to_string(),
            target_args: Vec::new(),
            trait_name: Some(trait_name.to_string()),
            methods,
            // Compiler-internal stdlib impls are unconditional —
            // primitive operator dispatch isn't generic over a bound.
            generic_params: None,
            where_clause: None,
        });
    }

    fn env_add_const(&mut self, c: &ConstDecl) {
        let ty = self.lower_type_expr(&c.ty, &[]);
        self.env.constants.insert(c.name.clone(), ty);
    }

    fn env_add_trait(&mut self, t: &TraitDef) {
        let assoc_types: Vec<String> = t
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::AssocType(decl) => Some(decl.name.clone()),
                _ => None,
            })
            .collect();
        let supertraits: Vec<String> = t
            .supertraits
            .iter()
            .map(|b| b.path.last().cloned().unwrap_or_default())
            .collect();
        self.env.traits.insert(
            t.name.clone(),
            TraitInfo {
                assoc_types,
                supertraits,
            },
        );
    }

    fn env_add_trait_alias(&mut self, t: &TraitAliasDef) {
        // v1 stub registration — record the name so use sites can emit
        // `E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET`. Bound substitution + the
        // matching `TraitInfo` shape land in P1.
        self.env.trait_aliases.insert(t.name.clone());
    }

    fn env_add_marker_trait(&mut self, t: &MarkerTraitDef) {
        // Register in `traits` so bound resolution treats the marker
        // identically to an ordinary trait. The companion entry in
        // `marker_traits` records the marker-ness for impl-body checks.
        let supertraits: Vec<String> = t
            .supertraits
            .iter()
            .map(|b| b.path.last().cloned().unwrap_or_default())
            .collect();
        self.env.traits.insert(
            t.name.clone(),
            TraitInfo {
                assoc_types: Vec::new(),
                supertraits,
            },
        );
        self.env.marker_traits.insert(t.name.clone());
    }

    fn env_add_type_alias(&mut self, t: &TypeAliasDef) {
        let gp = Self::generic_param_names(&t.generic_params);
        let ty = self.lower_type_expr(&t.ty, &gp);
        self.env.type_aliases.insert(t.name.clone(), ty);
    }

    fn env_add_distinct_type(&mut self, d: &crate::ast::DistinctTypeDef) {
        let derived = extract_derived_traits(&d.attributes);
        self.env.distinct_types.insert(d.name.clone(), derived);
    }

    fn env_add_opaque_foreign_type(&mut self, o: &crate::ast::OpaqueTypeDecl) {
        // Register the name so downstream type-resolution sees the
        // identifier as a known type (not an unknown-symbol error). Slice
        // 1b's use-site precision diagnostics (`E_OPAQUE_TYPE_REQUIRES_INDIRECTION`,
        // `E_OPAQUE_TYPE_NO_FIELDS`, `E_OPAQUE_TYPE_NO_INHERENT_OR_TRAIT_IMPLS`)
        // consult this set at `lower_type_expr_inner`, `infer_field_access`,
        // and `env_add_impl` respectively. The fourth code from
        // `design.md § Opaque Foreign Types` —
        // `E_OPAQUE_TYPE_NO_KNOWN_SIZE` (`size_of[Foo]()` /
        // `align_of[Foo]()`) — lands with the `offset_of[T](field)`
        // intrinsic family per `design.md § Field Offsets`; the
        // intrinsic surface does not exist in user code today.
        self.env.opaque_foreign_types.insert(o.name.clone());
    }

    fn env_add_extern_function(&mut self, e: &ExternFunction) {
        let param_names: Vec<Option<String>> = e
            .params
            .iter()
            .map(|p| p.name().map(|s| s.to_string()))
            .collect();
        let params: Vec<Type> = e
            .params
            .iter()
            .map(|p| self.lower_type_expr(&p.ty, &[]))
            .collect();
        let return_type = e
            .return_type
            .as_ref()
            .map(|t| self.lower_type_expr(t, &[]))
            .unwrap_or(Type::Unit);
        self.env.functions.insert(
            e.name.clone(),
            FunctionSig {
                generic_params: Vec::new(),
                param_names,
                params,
                return_type,
                where_clause: None,
            },
        );
    }
}
