//! LLVM code generation for Kāra programs.
//!
//! Compiles the AST to LLVM IR, then to native object files.
//! Uses the `inkwell` crate for LLVM bindings.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Linkage;
use inkwell::module::Module;
use inkwell::targets::{FileType, TargetData};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType, StructType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FunctionValue, GlobalValue, PointerValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

use crate::ast::*;
use crate::concurrency::{ConcurrencyAnalysis, FunctionConcurrency};
use crate::ownership::OwnershipCheckResult;
use crate::token::Span;

mod calls;
mod clone_drop;
mod control_flow;
mod declarations;
mod driver;
mod exprs;
mod functions;
mod helpers;
mod maps;
mod par_blocks;
mod pattern_binding;
mod provider;
mod runtime;
mod state;
mod stmts;
mod synth;
mod types_lowering;
mod vec_method;

use driver::{
    apply_optimization_passes, create_target_machine, read_auto_par_env,
    read_runtime_debug_metadata_env,
};
pub use driver::{link_executable, link_executable_with_sanitizer};
use helpers::{
    const_value_from_literal_expr, const_value_to_mangle_str, expr_as_type_expr_codegen,
    impl_target_name, make_impl_method_function, match_with_provider_call, method_self_is_value,
};
use state::{
    AssertedIndexBound, CleanupAction, EnumLayout, LoopFrame, MapMonoMethods, SetOpFilter,
    SharedTypeInfo, SoaLayout, SpawnSiteRecord, VarSlot,
};

// ── Public API ─────────────────────────────────────────────────

/// Compile a Kāra program to LLVM IR text (for debugging/testing).
pub fn compile_to_ir(
    program: &Program,
    ownership: Option<&OwnershipCheckResult>,
    concurrency: Option<&ConcurrencyAnalysis>,
) -> Result<String, String> {
    compile_to_ir_with_options(program, ownership, concurrency, None, None)
}

/// Like [`compile_to_ir`] but accepts optional source-filename and source-text
/// strings used by side features:
///   - `source_filename` is materialized as a deduped global string and
///     passed to `karac_error_trace_push(...)` at `?` failure sites so
///     error-return traces print as `<file>:<line>:<col>`. `None` preserves
///     the MVP fallback `<line>:<col>` output.
///   - `source_text` is consumed by the SpawnSiteId metadata table emission
///     (Debugger Contract slice 3) to resolve `par {}` byte offsets to
///     `(line, col)` for the `KARAC_SPAWN_SITES` global. `None` records
///     `(0, 0)` for each site — the table still emits, just without
///     source-position fidelity.
pub fn compile_to_ir_with_options(
    program: &Program,
    ownership: Option<&OwnershipCheckResult>,
    concurrency: Option<&ConcurrencyAnalysis>,
    source_filename: Option<&str>,
    source_text: Option<&str>,
) -> Result<String, String> {
    let context = Context::create();
    let mut cg = Codegen::new(&context, "karac_module");
    cg.load_rc_fallback(ownership);
    cg.load_concurrency_analysis(concurrency);
    cg.set_source_filename(source_filename);
    cg.set_source_text(source_text);
    cg.compile_program(program)?;
    Ok(cg.module.print_to_string().to_string())
}

/// Compile a Kāra program to a native object file.
pub fn compile_to_object(
    program: &Program,
    output_path: &str,
    ownership: Option<&OwnershipCheckResult>,
    concurrency: Option<&ConcurrencyAnalysis>,
) -> Result<(), String> {
    compile_to_object_with_options(program, output_path, ownership, concurrency, None, None)
}

/// Like [`compile_to_object`] but accepts optional source-filename and
/// source-text strings; see [`compile_to_ir_with_options`] for the
/// rationale and how each is consumed.
pub fn compile_to_object_with_options(
    program: &Program,
    output_path: &str,
    ownership: Option<&OwnershipCheckResult>,
    concurrency: Option<&ConcurrencyAnalysis>,
    source_filename: Option<&str>,
    source_text: Option<&str>,
) -> Result<(), String> {
    let context = Context::create();
    let mut cg = Codegen::new(&context, "karac_module");
    cg.load_rc_fallback(ownership);
    cg.load_concurrency_analysis(concurrency);
    cg.set_source_filename(source_filename);
    cg.set_source_text(source_text);
    cg.compile_program(program)?;

    let target_machine = create_target_machine()?;
    apply_optimization_passes(&cg.module, &target_machine)?;
    target_machine
        .write_to_file(&cg.module, FileType::Object, Path::new(output_path))
        .map_err(|e| format!("Failed to write object file: {}", e))
}

// ── Codegen ────────────────────────────────────────────────────

pub(super) struct Codegen<'ctx> {
    pub(crate) context: &'ctx Context,
    pub(crate) module: Module<'ctx>,
    pub(crate) builder: Builder<'ctx>,
    /// Maps variable name → (alloca pointer, value type).
    pub(crate) variables: HashMap<String, VarSlot<'ctx>>,
    /// Maps variable name → Kāra type name (for struct/enum field resolution).
    pub(crate) var_type_names: HashMap<String, String>,
    pub(crate) current_fn: Option<FunctionValue<'ctx>>,
    pub(crate) printf_fn: FunctionValue<'ctx>,
    /// `int snprintf(char* buf, size_t n, const char* fmt, ...)` — used by f-string
    /// codegen to convert integers and floats to their decimal string forms.
    pub(crate) snprintf_fn: FunctionValue<'ctx>,
    /// LLVM struct types for Kāra structs (struct name → LLVM type).
    pub(crate) struct_types: HashMap<String, StructType<'ctx>>,
    /// Field names in declaration order (struct name → field names).
    pub(crate) struct_field_names: HashMap<String, Vec<String>>,
    /// Field type-names in declaration order (struct name → per-field
    /// user-type name, or `None` if the field's declared type isn't a
    /// path / isn't a known user struct). Used to recover the inner
    /// type of chained field accesses (`o.inner.name` requires knowing
    /// the type of `o.inner` to resolve `name`'s field index in
    /// `compile_field_access` / `field_index_for`).
    pub(crate) struct_field_type_names: HashMap<String, Vec<Option<String>>>,
    /// Enum layouts for tagged-union codegen (enum name → layout).
    pub(crate) enum_layouts: HashMap<String, EnumLayout<'ctx>>,
    /// Nested loop stack — innermost frame is last.
    pub(crate) loop_stack: Vec<LoopFrame<'ctx>>,
    // ── Generic monomorphization ──────────────────────────────────
    /// Generic function AST nodes keyed by name. Not compiled until instantiated.
    pub(crate) generic_fns: HashMap<String, Function>,
    /// Already-generated monomorphizations (mangled name → done). Prevents duplicate codegen.
    pub(crate) generated_monos: HashSet<String>,
    /// Active type-parameter substitution during a monomorphization pass.
    /// Maps generic param name (e.g. `"T"`) → concrete LLVM type.
    pub(crate) type_subst: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Active const-parameter substitution during a monomorphization
    /// pass (const generics slice 4). Maps const-generic param name
    /// (e.g. `"N"`) → its bound `ConstValue`. Used by
    /// `compile_expr ExprKind::Identifier` to lower const-param
    /// references in generic bodies to LLVM constants of the matching
    /// width via `compile_primitive_const`, and by `Array[T, N]`
    /// element-size extraction sites to recover the size from a
    /// const-param reference. Slice 1b populates this map during
    /// `compile_generic_call`'s mango-key mango step; slice 4
    /// extends the save/restore around `compile_mono_function` so the
    /// body lowering sees the same bindings.
    pub(crate) const_subst: HashMap<String, crate::prelude::ConstValue>,
    // ── Closure compilation ────────────────────────────────────────
    /// Monotonic counter used to generate unique closure function names.
    pub(crate) closure_counter: u32,
    /// Monotonic counter for synthesized identifier names emitted by the
    /// indexed-receiver method-dispatch lowering (`__indexed_elem_<n>`).
    /// Each call site that lowers an `Index { object, index }` receiver
    /// allocates one synth name, registers it in the variable + element-type
    /// registries pointing into the outer container's storage, dispatches the
    /// method through the existing identifier path, and cleans up after.
    pub(crate) indexed_elem_counter: u32,
    /// Maps local variable names that hold closure fat-pointers to their LLVM function type.
    /// Required for indirect calls: `build_indirect_call` needs the callee's function type.
    pub(crate) closure_fn_types: HashMap<String, FunctionType<'ctx>>,
    /// Staging slot — set by `compile_closure` so the surrounding `let` binding can record
    /// the function type under the newly bound name.
    pub(crate) pending_closure_fn_type: Option<FunctionType<'ctx>>,
    /// Staging slot — caller-supplied LLVM types for a closure's parameters,
    /// consulted by `compile_closure` when the source has no type annotation
    /// to refine. Used by `Vec.sort_by` to push the element type into
    /// `|a, b|` closures so tuple receivers don't collapse to bare `i64`.
    /// Taken once and cleared on entry to `compile_closure`.
    pub(crate) pending_closure_param_hints: Option<Vec<BasicTypeEnum<'ctx>>>,
    // ── Shared types (RC) ─────────────────────────────────────────
    /// Shared type metadata (struct/enum name → heap layout info).
    pub(crate) shared_types: HashMap<String, SharedTypeInfo<'ctx>>,
    /// malloc function for heap allocation.
    pub(crate) malloc_fn: FunctionValue<'ctx>,
    /// free function for heap deallocation.
    pub(crate) free_fn: FunctionValue<'ctx>,
    /// exit function for runtime panics.
    pub(crate) exit_fn: FunctionValue<'ctx>,
    /// memcmp for string comparison.
    pub(crate) memcmp_fn: FunctionValue<'ctx>,
    /// Local bindings that alias `vec_var.len()` — populated at let-sites of
    /// the form `let n = v.len()` where `v` is a Vec identifier in scope.
    /// Consulted by the bounds-check-elision pass when parsing while-guard
    /// predicates of form `idx < n`: resolving `n` back to `v.len()` lets
    /// the elision recognize `idx < v.len()` and skip the upper-half of
    /// `compile_vec_index`'s bounds check on a matching `v[idx]` site.
    /// Cleared / replaced as bindings shadow; the simple HashMap shape is
    /// load-bearing because tracked Vec names don't shadow each other in
    /// practice — refine to scope-keyed if a counter-example surfaces.
    pub(crate) len_alias: HashMap<String, String>,
    /// Asserted bounds in the current emission scope — facts established
    /// by a dominating `while`-guard or `for`-range that the bounds-check
    /// emission can rely on. Each entry asserts one half of a Vec-index
    /// safety fact; `compile_vec_index` consults this stack at the
    /// indexing site and elides the matching half of the bounds check.
    /// The stack discipline (push on body-entry, pop on body-exit) maps
    /// directly onto the source-level lexical scope of the guard.
    pub(crate) asserted_index_bounds: Vec<AssertedIndexBound>,
    /// Per-variable Vec element type tracking (variable name → element LLVM type).
    pub(crate) vec_elem_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Slice element type tracking (variable name → element LLVM type).
    /// Entries only exist for values whose LLVM representation is the
    /// 2-field slice struct `{ptr, i64}`; used to dispatch indexing and
    /// iteration lowering.
    pub(crate) slice_elem_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Variables that are ref parameters (name → inner LLVM type for dereferencing).
    pub(crate) ref_params: HashMap<String, BasicTypeEnum<'ctx>>,
    /// SoA layout metadata (layout name → SoaLayout).
    pub(crate) soa_layouts: HashMap<String, SoaLayout>,
    /// Function parameter ref-ness (function name → vec of is_ref per param).
    pub(crate) fn_param_ref: HashMap<String, Vec<bool>>,
    /// Function parameter slice element type (function name → per-param
    /// Some(elem_ty) if that param is Slice[T] / mut Slice[T], else None).
    /// Used at call sites to emit Array → Slice and Vec → Slice coercions.
    pub(crate) fn_param_slice_elem: HashMap<String, Vec<Option<BasicTypeEnum<'ctx>>>>,
    /// Per-scope cleanup stack.  Each inner `Vec` is one scope frame; entries
    /// are emitted in reverse-push order at scope exit (innermost first).
    pub(crate) scope_cleanup_actions: Vec<Vec<CleanupAction<'ctx>>>,
    /// Set by `compile_match` when the scrutinee is a borrow-returning
    /// call (`Map.get`, `Vec.first`, ...) — used by `bind_pattern_values`
    /// to suppress `track_vec_var` for the bound name, since the payload
    /// aliases the container's storage and the container's own cleanup
    /// already covers the buffer.
    pub(crate) pattern_binding_is_borrow: bool,
    /// Phase 7.2 Slice DP — per-enum drop function cache (enum name →
    /// `__karac_drop_<EnumName>` `FunctionValue`). Lazily populated by
    /// `emit_enum_drop_switch` on first registration of a value-type
    /// enum binding via `track_enum_var`. One drop fn per enum type;
    /// reused across all registration sites for that type. Mirrors the
    /// existing `display_fn_cache` / `clone_fn_cache` lazy-synth pattern.
    pub(crate) enum_drop_fns: HashMap<String, FunctionValue<'ctx>>,
    /// Per-struct lazy drop-fn cache (struct name → `__karac_drop_struct_<Name>`
    /// `FunctionValue`). Lazily populated by `emit_struct_drop_synthesis` on
    /// first registration of a non-shared struct binding via `track_struct_var`.
    /// One drop fn per struct type; reused across registration sites. Mirrors
    /// `enum_drop_fns`. The drop fn walks fields and frees Vec/String data
    /// buffers + invokes `karac_map_free` on Map/Set handle fields. Structs
    /// with no heap-owning fields don't get an entry (the synthesis fn returns
    /// `None`) and don't reach `CleanupAction::StructDrop`.
    pub(crate) struct_drop_fns: HashMap<String, FunctionValue<'ctx>>,
    /// Cross-error-type conversion targets at `?` sites — populated from
    /// `Program.question_conversions` (set by the lowering pass from the
    /// typechecker's `question_conversions` map). Key: `(span.offset,
    /// span.length)` of the `?` expression. Value: target type name (e.g.
    /// `"AppError"`). When present, `compile_question` emits `Target.from(e)`
    /// against the inner err payload before the propagation early-return.
    pub(crate) question_conversions: HashMap<(usize, usize), String>,
    /// Per-callee effectfulness side-table — populated from
    /// `Program.callee_effectful` (set by the cli pipeline after effectcheck).
    /// Key: callable's canonical name (free fn `name`, assoc/method
    /// `Type.method`). Value: `true` iff the callee carries any of
    /// `reads`/`writes`/`sends`/`receives`. Read by `emit_branch_cancel_check`
    /// to skip the cooperative cancel atomic load when we can prove the
    /// callee is non-observably-effectful. Absent callees are treated as
    /// potentially effectful (fall back to the conservative MVP behavior).
    pub(crate) callee_effectful: HashMap<String, bool>,
    /// Per-method-call → `Type.method` callee key side-table — populated
    /// from `Program.method_callee_types` (set by the lowering pass from
    /// `TypeCheckResult.expr_types`). Key: `(span.offset, span.length)` of
    /// the `MethodCall` expression. Value: canonical `Type.method` string
    /// usable as a lookup into `callee_effectful`. Lets
    /// `compile_method_call` apply the same narrowing that `compile_call`
    /// applies to free-function and `Type.assoc` calls.
    pub(crate) method_callee_types: HashMap<(usize, usize), String>,
    /// Per-pattern-binding surface type table — populated from
    /// `Program.pattern_binding_types` (set by the lowering pass from
    /// `TypeCheckResult.pattern_binding_types`). Key: pattern's
    /// `(span.offset, span.length)`. Value: canonical type name (e.g.
    /// `"MyError"`). Used in `bind_pattern_values` to reconstitute struct
    /// payloads from the i64 word when the surface binding type is a
    /// struct, so subsequent `.field` access dispatches through the right
    /// struct shape.
    pub(crate) pattern_binding_types: HashMap<(usize, usize), String>,
    /// Sibling to `pattern_binding_types` carrying the inner element
    /// `TypeExpr` for `Vec[T]` / `Slice[T]` pattern bindings only. Populated
    /// from `Program.pattern_binding_inner_types`. Read by
    /// `bind_pattern_values` to lower the inner element type to a
    /// `BasicTypeEnum` (via `llvm_type_for_type_expr`) and register it
    /// under the binding's variable name in `vec_elem_types` /
    /// `slice_elem_types`, so subsequent method-dispatch (`xs.len()` /
    /// `xs[0]` / `xs.push(...)`) on a pattern-bound `Vec` / `Slice` payload
    /// routes through the right element-typed path. PB sibling slice
    /// (2026-05-09).
    pub(crate) pattern_binding_inner_types: HashMap<(usize, usize), TypeExpr>,
    /// Per-leaf-binding borrow mode populated from
    /// `Program.pattern_binding_borrow_modes`. Consumed by
    /// `bind_pattern_values` (Binding arm) to wrap a value-typed leaf
    /// binding in a ref-shim — an extra `ptr` alloca holding the value
    /// alloca's address, registered in `ref_params` — so call sites
    /// expecting `ref T` / `mut ref T` receive the right ABI shape.
    /// Empty for owned bindings. Slice 3a, 2026-05-14.
    pub(crate) pattern_binding_borrow_modes:
        HashMap<(usize, usize), crate::ast::PatternBindingBorrow>,
    /// Top-level `const NAME: T = value` declarations, populated by
    /// `compile_program` from `Item::ConstDecl` items before any function
    /// body is compiled. Key: const name. Value: the const's value
    /// expression. References to a const inside function bodies (parsed as
    /// `ExprKind::Identifier(name)` for bare uses) re-compile this stored
    /// expression at the use site, leaving constant folding to LLVM.
    /// Cycles are precluded upstream by the typechecker's const-evaluation
    /// pass (`check_const_decl`).
    pub(crate) consts: HashMap<String, Expr>,
    /// Source filename threaded in from the CLI (`compile_to_object_with_options`
    /// / `compile_to_ir_with_options`). When `Some`, `emit_error_trace_push`
    /// emits a deduped global string and passes its `(ptr, len)` to the runtime
    /// so error-return traces print as `<file>:<line>:<col>`. `None` preserves
    /// the original `<line>:<col>` MVP output for callers that don't supply a
    /// filename (most tests, ad-hoc IR dumps).
    pub(crate) source_filename: Option<String>,
    /// Memoized `(ptr, len)` for the global string materialized from
    /// `source_filename`. Populated lazily on first `?` site so we don't add
    /// an unused global to programs with no `?` propagation. Cleared on each
    /// `compile_program` entry alongside the other side-tables.
    pub(crate) source_filename_global: Option<(PointerValue<'ctx>, u64)>,
    /// Source text threaded in from the CLI (`compile_to_object_with_options`
    /// / `compile_to_ir_with_options` via `set_source_text`). When `Some`,
    /// `record_spawn_site` resolves each `par {}` block's byte offset to
    /// `(line, col)` via `crate::byte_offset_to_line_col`. When `None`,
    /// recorded entries fall back to `(0, 0)` — the metadata table still
    /// emits, just without source-position fidelity (most tests and ad-hoc
    /// IR dumps don't supply source text, and the `(line, col)` fields are
    /// strictly for the slice 5 / debugger surface).
    pub(crate) source_text: Option<String>,
    /// Symbols carrying `#[used]` collected during declaration. After the
    /// program is fully lowered, `emit_llvm_used` materializes them into the
    /// special `@llvm.used` appending-linkage global so the linker preserves
    /// each symbol even when nothing else references it. Order is preserved
    /// for stable IR output (helps snapshot tests and diffs).
    pub(crate) used_symbols: Vec<FunctionValue<'ctx>>,
    /// When compiling a par-branch function body, holds the LLVM pointer
    /// to the runtime's `AtomicBool` cancel flag (the second parameter
    /// passed by `karac_par_run`). `compile_call` reads this to emit a
    /// cooperative cancel check before each call site, implementing
    /// mid-branch cooperative cancellation per `design.md § Effect-boundary
    /// cooperative cancellation`. Conservatively fires before ANY call —
    /// the spec narrows the requirement to effectful calls (reads/writes/
    /// sends/receives) but the over-approximation is sound and avoids
    /// threading the `EffectCheckResult` through codegen for v1.
    /// `None` outside par branches.
    pub(crate) branch_cancel_ptr: Option<PointerValue<'ctx>>,
    // ── RC-fallback bindings ──────────────────────────────────────
    /// Per-function RC-fallback binding names populated from `OwnershipCheckResult`.
    /// Function name → set of binding names that need heap-boxing + refcount.
    pub(crate) rc_fallback_fns: HashMap<String, HashSet<String>>,
    /// Per-function Arc-promoted binding names — the subset of `rc_fallback_fns`
    /// flagged by the ownership pass as crossing a `par {}` thread boundary.
    /// Inc/dec on these bindings emits atomic LLVM operations (`atomicrmw add` /
    /// `atomicrmw sub`, `SeqCst`); the rest stay on plain non-atomic load+arith+store.
    /// Allocation site is unchanged — the heap layout `{ refcount: i64, payload: T }`
    /// is identical for both flavors.
    pub(crate) arc_fallback_fns: HashMap<String, HashSet<String>>,
    /// Heap struct type for each active RC-fallback binding in the current function.
    /// Cleared at each `compile_function` call. Key: binding name.
    pub(crate) rc_fallback_heap_types: HashMap<String, StructType<'ctx>>,
    /// Per-function parallelization decisions populated from `ConcurrencyAnalysis`.
    /// Function name → `FunctionConcurrency` (parallel groups + total stmt count).
    /// Threaded in by `load_concurrency_analysis`; consumed in slice 2 by the
    /// auto-par lowering path that emits `karac_par_run` for inferred groups
    /// outside explicit `par {}` blocks. Empty when no analysis was supplied.
    pub(crate) concurrency_decisions: HashMap<String, FunctionConcurrency>,
    /// Name of the function currently being compiled (for rc_fallback_fns lookup).
    pub(crate) current_fn_name: String,
    // ── Par block runtime ─────────────────────────────────────────
    /// Monotonic counter used to generate unique par-branch function names.
    /// Also serves as the `SpawnSiteId` for each `par {}` block — the value
    /// at the time `emit_par_run` records a spawn site is the ID written
    /// into the `KARAC_SPAWN_SITES` metadata table (slice 3 of the
    /// Debugger Contract; see `SpawnSiteRecord`).
    pub(crate) par_counter: u32,
    /// Runtime struct `KaracBranch { ptr func, ptr ctx }` — shared across par blocks.
    pub(crate) karac_branch_ty: StructType<'ctx>,
    /// Runtime entry point `void karac_par_run(const KaracBranch*, usize)`.
    pub(crate) karac_par_run_fn: FunctionValue<'ctx>,
    // ── Debugger contract: SpawnSiteId metadata (slice 3) ─────────
    /// One entry per `par {}` block (explicit or inferred). Populated by
    /// `record_spawn_site`; emitted as the `KARAC_SPAWN_SITES` global by
    /// `emit_spawn_sites_metadata` at the end of compilation. The order
    /// matches `SpawnSiteId` order (entry 0 → ID 0, entry 1 → ID 1, …).
    pub(crate) spawn_sites: Vec<SpawnSiteRecord>,
    /// Whether `KARAC_SPAWN_SITES` and friends emit populated. Driven by
    /// the `KARAC_RUNTIME_DEBUG_METADATA` env var read at `Codegen::new`
    /// time:
    ///
    /// - `Ok("0")` → false (gate explicitly off).
    /// - `Ok(_)`   → true.
    /// - `Err(_)`  → true (dev default).
    ///
    /// Slice 3 ships dev-default-on with env-var override only; profile-
    /// aware defaults (release / embedded / `isr` → off) and the TOML
    /// config home for the knob land in Phase 8.5 Track 2 (Build &
    /// Dependency Tooling). When the gate is off, all three globals
    /// (`KARAC_SPAWN_SITES`, `_LEN`, `_ENABLED`) still emit so slice 5's
    /// runtime API can read through the same symbols regardless of build
    /// mode and degrade cleanly — `_LEN` is zero, the array has zero
    /// entries, and `_ENABLED` is false. ID minting is unaffected so
    /// `__par_branch_<id>_<i>` symbol names stay stable across the
    /// gate-on / gate-off boundary. See `phase-8-stdlib-floor.md`
    /// § Auto-Concurrency Codegen — Debugger Contract slice 3.
    pub(crate) runtime_debug_metadata_enabled: bool,
    /// Slice 6 (Parallax-lite workload) — when true,
    /// `compile_function_body` skips its parallel-group dispatch path
    /// entirely and falls through to plain sequential `compile_block`,
    /// disabling auto-par codegen. Read once from the `KARAC_AUTO_PAR`
    /// env var at `Codegen` construction (see `read_auto_par_env`); the
    /// default is `false` (auto-par on). Used to support side-by-side
    /// wall-clock benchmarking of auto-par vs sequential codegen on the
    /// same workload without changing source. The user-facing
    /// `--sequential` CLI flag is a Phase 8.5 Track 2 deliverable; in
    /// v1, `KARAC_AUTO_PAR=0` is the only way to flip the gate. See
    /// `phase-8-stdlib-floor.md` § "Auto-Concurrency Codegen —
    /// Parallax-lite Workload".
    pub(crate) auto_par_disabled: bool,
    // ── Theme 6: `with_provider[R]` trait-method dispatch ──────────
    /// Resource name → stable u32 ID assigned at codegen init from the
    /// declaration order of `Item::EffectResource` items. The same
    /// integer flows through to runtime calls (`karac_provider_push`,
    /// `karac_provider_lookup`); the runtime is name-agnostic.
    pub(crate) provider_resource_ids: HashMap<String, u32>,
    /// Resource name → trait name for resources declared as
    /// `effect resource R: T`. Used to (1) drive vtable emission for
    /// the impls of `T` and (2) resolve method indices at `R.method(...)`
    /// call sites.
    pub(crate) provider_resource_traits: HashMap<String, String>,
    /// Trait name → ordered method-name list (source-declaration order
    /// from the `trait T { ... }` block). Vtables for `impl T for U`
    /// store fn ptrs in this same order; method dispatch resolves the
    /// vtable index by `position()` against this list.
    pub(crate) provider_trait_methods: HashMap<String, Vec<String>>,
    /// (impl-target type name, trait name) → emitted vtable global.
    /// Populated after impl method declarations run in `compile_program`.
    pub(crate) provider_vtables: HashMap<(String, String), GlobalValue<'ctx>>,
    /// Runtime extern: `karac_provider_push(frame_ptr, resource_id, data_ptr, vtable_ptr)`.
    /// Consumed by `with_provider[R]` lowering (sub-step 3).
    pub(crate) karac_provider_push_fn: FunctionValue<'ctx>,
    /// Runtime extern: `karac_provider_pop()`. Consumed by `with_provider[R]`
    /// lowering (sub-step 3) for the matching pop on body exit.
    pub(crate) karac_provider_pop_fn: FunctionValue<'ctx>,
    /// Runtime extern: `karac_provider_lookup(resource_id) -> ProviderLookupResult`.
    /// Consumed by `R.method(...)` dispatch (sub-step 4) to find the
    /// active provider's data pointer and vtable.
    pub(crate) karac_provider_lookup_fn: FunctionValue<'ctx>,
    /// Runtime extern: `karac_provider_get_stack_head() -> *const ProviderFrame`.
    /// Consumed by par-block lowering (sub-step 5) to snapshot the
    /// calling thread's stack head into the par-block env-struct.
    pub(crate) karac_provider_get_stack_head_fn: FunctionValue<'ctx>,
    /// Runtime extern: `karac_provider_set_stack_head(head)`. Consumed
    /// by par-branch fn prologues (sub-step 5) to seed each worker
    /// thread's TLS from the env-struct snapshot, so providers in
    /// scope at the par-block site stay visible inside spawned branches.
    pub(crate) karac_provider_set_stack_head_fn: FunctionValue<'ctx>,
    /// LLVM struct type for `ProviderFrame { prev, resource_id, data, vtable }`
    /// — `#[repr(C)]` matches `runtime/src/lib.rs::ProviderFrame`. Consumed
    /// at `with_provider[R]` lowering sites for the alloca'd frame storage
    /// (sub-step 3); declared here so the type is established alongside
    /// the runtime extern declarations.
    pub(crate) provider_frame_ty: StructType<'ctx>,
    /// LLVM struct type for `ProviderLookupResult { data, vtable }` —
    /// matches the runtime's `#[repr(C)]` shape. Used once at codegen
    /// init to type the `karac_provider_lookup` extern's return; after
    /// that the call's return type carries the shape implicitly so
    /// extractvalue at sub-step 4 dispatch sites doesn't need to look
    /// it up here. Field retained as ABI documentation for future
    /// readers and as the canonical anchor if `ProviderLookupResult`'s
    /// shape ever changes.
    #[allow(dead_code)]
    pub(crate) provider_lookup_result_ty: StructType<'ctx>,
    // ── Map runtime ───────────────────────────────────────────────
    /// Per-variable Map key LLVM type (variable name → K LLVM type).
    pub(crate) map_key_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Map value LLVM type (variable name → V LLVM type).
    pub(crate) map_val_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Map key type name string (e.g. "i64", "String") for hash/eq fn selection.
    pub(crate) map_key_type_names: HashMap<String, String>,
    /// Per-variable element-`TypeExpr` side-table for collection variables —
    /// the *element* of a Vec/Slice/Array, or the *value* of a Map. Used by
    /// `compile_for_*_var` so for-loop bindings inherit the right side-table
    /// registrations (`vec_elem_types`, `slice_elem_types`, `map_*_types`)
    /// when the element is itself a Vec/String/Slice/Map. Without this,
    /// LLVM-type-only tracking can't distinguish `Vec[String]` from
    /// `Vec[Vec[T]]` (both store `vec_struct_type` as the element LLVM type).
    pub(crate) var_elem_type_exprs: HashMap<String, TypeExpr>,
    /// Per-Map-variable key-`TypeExpr` side-table (parallels
    /// `var_elem_type_exprs` for the key slot). Used by `compile_for_map_var`
    /// to register the per-iteration `k` binding when iterating with a tuple
    /// pattern `for (k, v) in m`.
    pub(crate) map_key_type_exprs: HashMap<String, TypeExpr>,
    /// Per-variable Set element LLVM type (variable name → T LLVM type).
    /// Mirrors `map_key_types` — `Set[T]` lowers to `Map[T, ()]` at codegen,
    /// reusing the `karac_map_*` C runtime, but the surface type identity is
    /// kept distinct so codegen can pick the right method dispatch and the
    /// Display fn can pick the `Set{...}` brace style.
    pub(crate) set_elem_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Set element type name string (e.g. `"i64"`, `"String"`)
    /// for hash/eq fn selection. Mirrors `map_key_type_names`.
    pub(crate) set_elem_type_names: HashMap<String, String>,
    /// Per-variable Set element-`TypeExpr` side-table. Mirrors
    /// `map_key_type_exprs` and is consulted alongside it by Set-aware paths
    /// (`compile_for_set_var`, Set Display fn) so compound element types
    /// (`Set[(i64, String)]`, `Set[Vec[T]]`) compose through the
    /// TypeExpr-aware hash/eq/Display paths.
    pub(crate) set_elem_type_exprs: HashMap<String, TypeExpr>,
    /// Variables whose surface type is `String`. Disambiguates Strings from
    /// `Vec[u8]` at iteration time — both share the `{ptr, i64, i64}`
    /// physical layout and are both registered in `vec_elem_types` with
    /// element-LLVM-type `i8`, so the for-loop dispatcher otherwise can't
    /// tell which iteration shape to emit. `for c in s` and `for c in
    /// s.chars()` on a String iterate per Unicode scalar value via the
    /// `karac_string_decode_char` runtime helper; `for b in v` on a
    /// `Vec[u8]` iterates per byte. Populated alongside the existing
    /// `vec_elem_types` insertion at every String-registration site.
    pub(crate) string_vars: HashSet<String>,
    /// HTTP handler ABI trampoline (2026-05-09): cache of per-handler-fn
    /// `extern "C"` shims. Key is the user handler's mangled fn name (e.g.
    /// `"handle"`); value is the synthesized shim function. Sharing the
    /// shim across multiple `Server.serve(handler)` calls in one program
    /// avoids redundant emission and keeps the IR stable. Pinned by
    /// `tests/codegen.rs::test_server_serve_handler_shim_caches`.
    pub(crate) http_shim_cache: HashMap<String, FunctionValue<'ctx>>,
    pub(crate) karac_map_new_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_free_fn: FunctionValue<'ctx>,
    /// `karac_map_free_with_drop_vec(map: ptr, drop_key: i32, drop_val: i32)`
    /// — `karac_map_free` variant that recursively drops per-entry
    /// Vec/String content before deallocating the bucket storage.
    /// `drop_key != 0` releases each live entry's key data buffer when
    /// the key follows the `{ptr, len, cap}` layout; `drop_val != 0`
    /// does the same for the value. Selected by the `FreeMapHandle`
    /// cleanup arm whenever either flag is set. Replaces the narrower
    /// `karac_map_free_with_val_drop_vec` (val-only) helper that
    /// shipped 2026-05-13.
    ///
    /// Closes leaks for `Set[Vec[T]]` / `Set[String]` (key drop only),
    /// `Map[String, V]` / `Map[Vec[T], V]` (key drop only),
    /// `Map[String, Vec[U]]` / `Map[Vec[T], Vec[U]]` (both flags). The
    /// primitive-only `Map[i64, i64]` case stays on plain
    /// `karac_map_free` for zero overhead.
    pub(crate) karac_map_free_with_drop_vec_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_insert_old_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_get_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_remove_old_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_contains_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_len_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_clear_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_iter_new_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_iter_next_fn: FunctionValue<'ctx>,
    pub(crate) karac_map_iter_free_fn: FunctionValue<'ctx>,
    /// `i64 karac_string_decode_char(*const u8 data, i64 len, i64 byte_offset, *mut u32 out_cp)`.
    /// Returns the byte offset after the decoded char and writes the
    /// codepoint through the out-param. Drives `for c in s` / `for c in
    /// s.chars()` lowering — see `compile_for_string_chars`.
    pub(crate) karac_string_decode_char_fn: FunctionValue<'ctx>,
    /// `karac_map_entry(map: ptr, key: ptr, out_slot_ptr: ptr) -> i1` —
    /// probe-and-insert-on-vacant. Used by entry chains whose terminal is
    /// `or_insert` / `or_insert_with` — codegen will write a default through
    /// the slot when occupied=false, so the runtime claims the bucket up
    /// front.
    pub(crate) karac_map_entry_fn: FunctionValue<'ctx>,
    /// `karac_map_lookup_slot(map: ptr, key: ptr, out_slot_ptr: ptr) -> i1`
    /// — read-only variant used by entry chains whose terminal is
    /// `and_modify`. The closure runs only when occupied=true; nothing is
    /// inserted on the Vacant path.
    pub(crate) karac_map_lookup_slot_fn: FunctionValue<'ctx>,
    /// `karac_string_clone(src: ptr, dst: ptr) -> void` — runtime helper
    /// for the codegen-emitted String case in `emit_clone_fn_for_type_expr`.
    /// Allocates a fresh buffer, copies len bytes, writes the new
    /// `{data, len, cap}` to `dst`. Static-literal sources (cap = 0) get
    /// a heap-owned copy so scope-exit cleanup fires; source untouched.
    pub(crate) karac_string_clone_fn: FunctionValue<'ctx>,
    /// Per-type clone function cache. Keyed on the canonical mangled type
    /// name (`display_mangle_te`). Each emitted fn has signature
    /// `void karac_clone_<typename>(*const T src, *mut T dst)` — caller
    /// provides both source and destination addresses, callee writes the
    /// cloned value into the destination slot. Mirror of `display_fn_cache`.
    pub(crate) clone_fn_cache: HashMap<String, FunctionValue<'ctx>>,
    /// Per-type Drop function cache. Keyed by the canonical type name
    /// (e.g. `"i64"`, `"String"`, `"Vec_i64"`, `"Map_String_i64"`). Each
    /// emitted fn has signature `void karac_drop_<typename>(*mut T)` and
    /// releases any heap owned by the value (for primitives: no-op; for
    /// String: free the data buffer if cap > 0; for Vec: per-element drop
    /// then free; for tuple: per-field drop; for Map/Set: delegate to the
    /// existing `karac_map_free*` runtime as a placeholder pending the
    /// monomorphized Map layout in Slice 1+). Mirror of `clone_fn_cache`.
    /// See [`wip-monomorphized-collections.md`](../docs/implementation_checklist/wip-monomorphized-collections.md) §3.3.
    ///
    /// `#[allow(dead_code)]` until Slice 1 lands the first production
    /// consumer (monomorphized `Map[i64, i64]` drop, per
    /// [`phase-7-codegen.md`](../docs/implementation_checklist/phase-7-codegen.md)
    /// "Monomorphized collections" entry). The framework is foundation;
    /// it has no production caller until the consumer lands.
    #[allow(dead_code)]
    pub(crate) drop_fn_cache: HashMap<String, FunctionValue<'ctx>>,
    /// Per-(K, V) cache of monomorphized `Map[K, V]` method symbols.
    /// Keyed by the mangled `"{key_mangle}_{val_mangle}"` token (e.g.
    /// `"i64_i64"`) produced by `mono_map_cache_key`. Lazily populated
    /// by `get_or_emit_map_mono_methods` on the first request for a
    /// given K/V tuple. Per-method `FunctionValue`s have `LinkOnceODR`
    /// linkage so cross-crate / cross-TU duplicates collapse at link
    /// time (locked design § 3.2). Slice 1 ships `Map[i64, i64]` only;
    /// the gating predicate `should_use_mono_map_for` returns `false`
    /// for every other K/V tuple, leaving them on the erased fallback
    /// per § 3.6.
    pub(crate) map_mono_methods: HashMap<String, MapMonoMethods<'ctx>>,
    /// Per-type Display function cache. Keyed by the canonical type name
    /// (e.g. `"i64"`, `"String"`, `"Vec_i64"`, `"Map_String_i64"`). Each
    /// emitted fn has signature `void karac_display_<typename>(ptr)` and
    /// writes characters to stdout via `printf` with no trailing newline.
    /// The pointer-by-reference convention is uniform across every type so
    /// callers don't need per-type calling conventions; primitives load the
    /// value, structs extract fields, opaque ptrs load the handle.
    ///
    /// `dead_code` is allowed because subtasks 1+2 of the Display canonical
    /// bullet ship the machinery + primitive Display fns ahead of subtasks
    /// 3-7 which add the callers (Vec/Map/Set/Tuple Display fns + the
    /// `compile_print` integration). Remove the allow when subtask 7 lands.
    #[allow(dead_code)]
    pub(crate) display_fn_cache: HashMap<String, FunctionValue<'ctx>>,
    // ── Error return trace runtime ────────────────────────────────
    /// `void karac_error_trace_push(ptr file, i64 file_len, i32 line, i32 col)`.
    /// Called by `compile_question` at each `?` failure block before
    /// `emit_scope_cleanup`. The runtime maintains a thread-local depth-64
    /// ring buffer; an atexit handler prints it to stderr at program exit.
    pub(crate) karac_error_trace_push_fn: FunctionValue<'ctx>,
    /// `void karac_error_trace_clear()`. Emitted at every `?` success site
    /// so a recovered earlier propagation doesn't leak frames into a later
    /// failure.
    pub(crate) karac_error_trace_clear_fn: FunctionValue<'ctx>,
    /// Lazily-initialized `TargetData` consumed by the layout-introspection
    /// intrinsics (`align_of[T]()`, `offset_of[T](field)`). Constructed
    /// via `create_target_machine().get_target_data()` on first use; the
    /// rest of codegen never reads it. Held as `Option` because the
    /// host-target initialization pulls in `Target::initialize_native`,
    /// which we want to avoid in the (common) path where no layout
    /// intrinsic is invoked.
    pub(crate) target_data: Option<TargetData>,
}

impl<'ctx> Codegen<'ctx> {
    fn new(context: &'ctx Context, module_name: &str) -> Self {
        let module = context.create_module(module_name);
        let builder = context.create_builder();

        let i32_type = context.i32_type();
        let i64_type = context.i64_type();
        let ptr_type = context.ptr_type(AddressSpace::default());
        let printf_type = i32_type.fn_type(&[BasicMetadataTypeEnum::from(ptr_type)], true);
        let printf_fn = module.add_function("printf", printf_type, None);

        // `int snprintf(char* buf, size_t n, const char* fmt, ...)` for f-string
        // integer/float → string conversion.
        let snprintf_type = i32_type.fn_type(
            &[
                BasicMetadataTypeEnum::from(ptr_type),
                BasicMetadataTypeEnum::from(i64_type),
                BasicMetadataTypeEnum::from(ptr_type),
            ],
            true,
        );
        let snprintf_fn = module.add_function("snprintf", snprintf_type, Some(Linkage::External));

        // Declare malloc and free for RC heap allocation.
        let malloc_type = ptr_type.fn_type(&[BasicMetadataTypeEnum::from(i64_type)], false);
        let malloc_fn = module.add_function("malloc", malloc_type, Some(Linkage::External));
        let free_type = context
            .void_type()
            .fn_type(&[BasicMetadataTypeEnum::from(ptr_type)], false);
        let free_fn = module.add_function("free", free_type, Some(Linkage::External));

        let exit_type = context
            .void_type()
            .fn_type(&[BasicMetadataTypeEnum::from(i32_type)], false);
        let exit_fn = module.add_function("exit", exit_type, Some(Linkage::External));

        let memcmp_type = i32_type.fn_type(
            &[
                BasicMetadataTypeEnum::from(ptr_type),
                BasicMetadataTypeEnum::from(ptr_type),
                BasicMetadataTypeEnum::from(i64_type),
            ],
            false,
        );
        let memcmp_fn = module.add_function("memcmp", memcmp_type, Some(Linkage::External));

        // Declare runtime types and entry points for par blocks.
        // `KaracBranch { func: ptr, ctx: ptr }` matches the #[repr(C)] struct in
        // runtime/src/lib.rs. karac_par_run(branches, count, spawn_site_id)
        // spawns one thread per branch and joins all before returning. The
        // `spawn_site_id` argument (Debugger Contract slice 4) identifies the
        // par site for `KaracFrame` metadata; the runtime ignores it when
        // `KARAC_RUNTIME_DEBUG_METADATA=0`.
        let karac_branch_ty = context.struct_type(&[ptr_type.into(), ptr_type.into()], false);
        let karac_par_run_type = context.void_type().fn_type(
            &[
                BasicMetadataTypeEnum::from(ptr_type),
                BasicMetadataTypeEnum::from(i64_type),
                BasicMetadataTypeEnum::from(i32_type),
            ],
            false,
        );
        let karac_par_run_fn =
            module.add_function("karac_par_run", karac_par_run_type, Some(Linkage::External));

        // ── Theme 6: provider stack ABI ──────────────────────────────────
        //
        // Mirrors `runtime/src/lib.rs::ProviderFrame` and
        // `ProviderLookupResult` `#[repr(C)]` layouts. `ProviderFrame` is
        // alloca'd at each `with_provider[R](...)` site; the runtime only
        // updates head pointers, so the storage shape needs to match the
        // runtime's reads but isn't owned by the runtime.
        let provider_frame_ty = context.struct_type(
            &[
                ptr_type.into(), // prev: *const ProviderFrame
                i32_type.into(), // resource_id: u32
                ptr_type.into(), // provider_data_ptr: *const u8
                ptr_type.into(), // vtable_ptr: *const VTable
            ],
            false,
        );
        let provider_lookup_result_ty = context.struct_type(
            &[
                ptr_type.into(), // data: *const u8
                ptr_type.into(), // vtable: *const VTable
            ],
            false,
        );
        let karac_provider_push_type = context.void_type().fn_type(
            &[
                ptr_type.into(), // frame: *mut ProviderFrame
                i32_type.into(), // resource_id: u32
                ptr_type.into(), // provider_data: *const u8
                ptr_type.into(), // vtable: *const VTable
            ],
            false,
        );
        let karac_provider_push_fn = module.add_function(
            "karac_provider_push",
            karac_provider_push_type,
            Some(Linkage::External),
        );
        let karac_provider_pop_type = context.void_type().fn_type(&[], false);
        let karac_provider_pop_fn = module.add_function(
            "karac_provider_pop",
            karac_provider_pop_type,
            Some(Linkage::External),
        );
        let karac_provider_lookup_type =
            provider_lookup_result_ty.fn_type(&[i32_type.into()], false);
        let karac_provider_lookup_fn = module.add_function(
            "karac_provider_lookup",
            karac_provider_lookup_type,
            Some(Linkage::External),
        );
        // Sub-step 5 (par-block inheritance): get/set the per-thread head
        // pointer so par-branch worker tasks can inherit the parent
        // thread's provider stack via the env-struct snapshot mechanism.
        let karac_provider_get_stack_head_type = ptr_type.fn_type(&[], false);
        let karac_provider_get_stack_head_fn = module.add_function(
            "karac_provider_get_stack_head",
            karac_provider_get_stack_head_type,
            Some(Linkage::External),
        );
        let karac_provider_set_stack_head_type =
            context.void_type().fn_type(&[ptr_type.into()], false);
        let karac_provider_set_stack_head_fn = module.add_function(
            "karac_provider_set_stack_head",
            karac_provider_set_stack_head_type,
            Some(Linkage::External),
        );

        // ── Debugger Contract slice 5: `std.runtime` introspection ──
        //
        // Two extern declarations consumed by `compile_assoc_call`'s
        // arms for `Runtime.has_debug_metadata()` and
        // `Runtime.list_par_blocks()`. The third API
        // (`Runtime.list_tasks()`) always returns the empty Vec value
        // in v1, so it has no runtime-side dispatch — the call lowers
        // to a `Vec.new()`-shaped value directly.
        //
        // `karac_runtime_has_debug_metadata() -> bool` reads the
        // `KARAC_SPAWN_SITES_ENABLED` global emitted by slice 3.
        // `karac_runtime_list_par_blocks_into(out: *mut KaracVec)` writes
        // a freshly-materialized `Vec[ParBlockInfo]` `{data, len, cap}`
        // descriptor into the slot at `out`. Slice 5 takes the
        // runtime-side full Vec materialization path (hard-stop trigger 3
        // fallback) — runtime knows Kāra Vec + String layout (already
        // documented in `clone.rs::karac_string_clone`) and the
        // `KaracParBlockInfo` `#[repr(C)]` matches what user-side codegen
        // would produce for the baked-stdlib `ParBlockInfo` struct.
        let karac_runtime_has_debug_metadata_type = context.bool_type().fn_type(&[], false);
        let _karac_runtime_has_debug_metadata_fn = module.add_function(
            "karac_runtime_has_debug_metadata",
            karac_runtime_has_debug_metadata_type,
            Some(Linkage::External),
        );
        let karac_runtime_list_par_blocks_into_type = context
            .void_type()
            .fn_type(&[BasicMetadataTypeEnum::from(ptr_type)], false);
        let _karac_runtime_list_par_blocks_into_fn = module.add_function(
            "karac_runtime_list_par_blocks_into",
            karac_runtime_list_par_blocks_into_type,
            Some(Linkage::External),
        );

        // ── Slice B: HTTP server FFI surface (minimal `std.http`) ────
        //
        // `karac_runtime_serve_http_static(addr: *const c_char, body: *const u8,
        // body_len: usize) -> i32` — bind on `addr`, serve every
        // request with a 200/JSON response carrying `body` as the
        // payload. v1's smoke handler. Real handler-fn-ptr dispatch
        // lands in a follow-up; see the Slice B close-out under
        // `wip-list1.md`.
        //
        // Return code: 0 on graceful shutdown (currently unreachable —
        // the accept loop runs forever); non-zero on bind / runtime-
        // construction failure. Codegen translates the return into a
        // `Result[Unit, HttpError]` per the Kāra-side
        // `Server.serve_static` signature.
        let karac_runtime_serve_http_static_type = context
            .i32_type()
            .fn_type(&[ptr_type.into(), ptr_type.into(), i64_type.into()], false);
        let _karac_runtime_serve_http_static_fn = module.add_function(
            "karac_runtime_serve_http_static",
            karac_runtime_serve_http_static_type,
            Some(Linkage::External),
        );

        // Slice B follow-up (2026-05-09): full handler-dispatch entry.
        // `karac_runtime_serve_http(addr_cstr: *const c_char, handler:
        // extern "C" fn(*const KaracHttpRequest, *mut KaracHttpResponse),
        // bound_port_out: *mut u16) -> i32`. v1 always passes a null
        // `bound_port_out` — the smoke test reads the port from the
        // runtime's `BOUND_PORT=<n>\n` stdout line per Slice B's
        // convention. The handler is a free-fn pointer (sub-step (b) of
        // the Slice B follow-up); closures with captures are rejected
        // upstream with `E_CLOSURE_AS_FN_PTR_NOT_YET`. The LLVM
        // function-pointer types of the user handler and this extern's
        // `handler` slot don't have to match structurally — LLVM
        // function-pointer-typed parameters are just `ptr`s at the
        // indirect-call boundary; the runtime invokes the handler with
        // the documented `extern "C"` signature regardless of the user
        // handler's lowered Kāra signature.
        let karac_runtime_serve_http_type = context.i32_type().fn_type(
            &[
                ptr_type.into(), // addr_cstr
                ptr_type.into(), // handler fn-ptr (just `ptr` at LLVM level)
                ptr_type.into(), // bound_port_out
            ],
            false,
        );
        let _karac_runtime_serve_http_fn = module.add_function(
            "karac_runtime_serve_http",
            karac_runtime_serve_http_type,
            Some(Linkage::External),
        );

        // HTTP handler ABI trampoline (2026-05-09): per-request runtime
        // externs invoked from the Kāra-side `Request.path()` / `.method()`
        // methods and from the per-handler shim that decomposes the user-
        // returned `Response` into the FFI `KaracHttpResponse` slot.
        //
        // - `karac_runtime_http_request_path(*const KaracHttpRequest)
        //    -> *const c_char` — null-terminated UTF-8 path. Lifetime tied
        //   to the request struct (dropped after the handler returns), so
        //   `Request.path()` copies into a fresh Kāra String per call (F2).
        // - `karac_runtime_http_request_method(...)` — same shape, returns
        //   the HTTP method verb.
        // - `karac_runtime_http_response_set_status(*mut KaracHttpResponse,
        //    u16)` — write the status code.
        // - `karac_runtime_http_response_set_body(*mut KaracHttpResponse,
        //    *const u8, usize)` — copy a byte buffer into a fresh
        //   runtime-owned response body.
        let request_path_type = ptr_type.fn_type(&[ptr_type.into()], false);
        let _karac_runtime_http_request_path_fn = module.add_function(
            "karac_runtime_http_request_path",
            request_path_type,
            Some(Linkage::External),
        );
        let request_method_type = ptr_type.fn_type(&[ptr_type.into()], false);
        let _karac_runtime_http_request_method_fn = module.add_function(
            "karac_runtime_http_request_method",
            request_method_type,
            Some(Linkage::External),
        );
        let response_set_status_type = context
            .void_type()
            .fn_type(&[ptr_type.into(), context.i16_type().into()], false);
        let _karac_runtime_http_response_set_status_fn = module.add_function(
            "karac_runtime_http_response_set_status",
            response_set_status_type,
            Some(Linkage::External),
        );
        let response_set_body_type = context
            .void_type()
            .fn_type(&[ptr_type.into(), ptr_type.into(), i64_type.into()], false);
        let _karac_runtime_http_response_set_body_fn = module.add_function(
            "karac_runtime_http_response_set_body",
            response_set_body_type,
            Some(Linkage::External),
        );
        let strlen_type = i64_type.fn_type(&[ptr_type.into()], false);
        if module.get_function("strlen").is_none() {
            module.add_function("strlen", strlen_type, Some(Linkage::External));
        }

        // ── Map runtime extern declarations ──────────────────────────────
        // All map methods use opaque ptr for the map handle and key/value
        // pointers. Sizes and fn-pointers are passed as i64 / ptr.
        let i64_ty: BasicMetadataTypeEnum = i64_type.into();
        let ptr_md: BasicMetadataTypeEnum = ptr_type.into();

        // karac_map_new(key_size: i64, val_size: i64, hash_fn: ptr, eq_fn: ptr) -> ptr
        let map_new_ty = ptr_type.fn_type(&[i64_ty, i64_ty, ptr_md, ptr_md], false);
        let karac_map_new_fn =
            module.add_function("karac_map_new", map_new_ty, Some(Linkage::External));

        // karac_map_free(map: ptr) -> void
        let map_free_ty = context.void_type().fn_type(&[ptr_md], false);
        let karac_map_free_fn =
            module.add_function("karac_map_free", map_free_ty, Some(Linkage::External));

        // karac_map_free_with_drop_vec(map: ptr, drop_key: i32, drop_val: i32) -> void —
        // generalized variant: walks live buckets and frees per-entry key
        // and/or value data pointers (when the respective flag is set
        // and the field's `cap > 0`) before deallocating the bucket
        // storage. Selected when either side of `Map[K, V]` / `Set[T]`
        // follows the Vec/String `{ptr, len, cap}` layout. The i32 flags
        // are codegen-set: nonzero means "drop this side".
        let i32_ty: BasicMetadataTypeEnum = context.i32_type().into();
        let map_free_with_drop_ty = context
            .void_type()
            .fn_type(&[ptr_md, i32_ty, i32_ty], false);
        let karac_map_free_with_drop_vec_fn = module.add_function(
            "karac_map_free_with_drop_vec",
            map_free_with_drop_ty,
            Some(Linkage::External),
        );

        // karac_map_insert_old(map: ptr, key: ptr, val: ptr, out_old_val: ptr) -> i1
        let map_insert_old_ty = context
            .bool_type()
            .fn_type(&[ptr_md, ptr_md, ptr_md, ptr_md], false);
        let karac_map_insert_old_fn = module.add_function(
            "karac_map_insert_old",
            map_insert_old_ty,
            Some(Linkage::External),
        );

        // karac_map_get(map: ptr, key: ptr, out_val: ptr) -> i1
        let map_get_ty = context
            .bool_type()
            .fn_type(&[ptr_md, ptr_md, ptr_md], false);
        let karac_map_get_fn =
            module.add_function("karac_map_get", map_get_ty, Some(Linkage::External));

        // karac_map_remove_old(map: ptr, key: ptr, out_old_val: ptr) -> i1
        let map_remove_old_ty = context
            .bool_type()
            .fn_type(&[ptr_md, ptr_md, ptr_md], false);
        let karac_map_remove_old_fn = module.add_function(
            "karac_map_remove_old",
            map_remove_old_ty,
            Some(Linkage::External),
        );

        // karac_map_contains(map: ptr, key: ptr) -> i1
        let map_contains_ty = context.bool_type().fn_type(&[ptr_md, ptr_md], false);
        let karac_map_contains_fn = module.add_function(
            "karac_map_contains",
            map_contains_ty,
            Some(Linkage::External),
        );

        // karac_map_len(map: ptr) -> i64
        let map_len_ty = i64_type.fn_type(&[ptr_md], false);
        let karac_map_len_fn =
            module.add_function("karac_map_len", map_len_ty, Some(Linkage::External));

        // karac_map_clear(map: ptr) -> void
        let map_clear_ty = context.void_type().fn_type(&[ptr_md], false);
        let karac_map_clear_fn =
            module.add_function("karac_map_clear", map_clear_ty, Some(Linkage::External));

        // karac_map_iter_new(map: ptr) -> ptr
        let map_iter_new_ty = ptr_type.fn_type(&[ptr_md], false);
        let karac_map_iter_new_fn = module.add_function(
            "karac_map_iter_new",
            map_iter_new_ty,
            Some(Linkage::External),
        );

        // karac_map_iter_next(iter: ptr, out_key: ptr, out_val: ptr) -> i1
        let map_iter_next_ty = context
            .bool_type()
            .fn_type(&[ptr_md, ptr_md, ptr_md], false);
        let karac_map_iter_next_fn = module.add_function(
            "karac_map_iter_next",
            map_iter_next_ty,
            Some(Linkage::External),
        );

        // karac_map_iter_free(iter: ptr) -> void
        let map_iter_free_ty = context.void_type().fn_type(&[ptr_md], false);
        let karac_map_iter_free_fn = module.add_function(
            "karac_map_iter_free",
            map_iter_free_ty,
            Some(Linkage::External),
        );

        // karac_string_decode_char(data: ptr, len: i64, byte_offset: i64,
        //                          out_codepoint: ptr) -> i64
        // Drives `for c in s` / `for c in s.chars()` lowering. Returns the
        // byte offset after the decoded char; writes the codepoint to
        // `out_codepoint`.
        let i64_md: BasicMetadataTypeEnum = context.i64_type().into();
        let string_decode_char_ty = context
            .i64_type()
            .fn_type(&[ptr_md, i64_md, i64_md, ptr_md], false);
        let karac_string_decode_char_fn = module.add_function(
            "karac_string_decode_char",
            string_decode_char_ty,
            Some(Linkage::External),
        );

        // karac_map_entry(map: ptr, key: ptr, out_slot_ptr: ptr) -> i1
        let map_entry_ty = context
            .bool_type()
            .fn_type(&[ptr_md, ptr_md, ptr_md], false);
        let karac_map_entry_fn =
            module.add_function("karac_map_entry", map_entry_ty, Some(Linkage::External));

        // karac_map_lookup_slot(map: ptr, key: ptr, out_slot_ptr: ptr) -> i1
        let map_lookup_slot_ty = context
            .bool_type()
            .fn_type(&[ptr_md, ptr_md, ptr_md], false);
        let karac_map_lookup_slot_fn = module.add_function(
            "karac_map_lookup_slot",
            map_lookup_slot_ty,
            Some(Linkage::External),
        );

        // karac_string_clone(src: ptr, dst: ptr) -> void
        let string_clone_ty = context.void_type().fn_type(&[ptr_md, ptr_md], false);
        let karac_string_clone_fn = module.add_function(
            "karac_string_clone",
            string_clone_ty,
            Some(Linkage::External),
        );

        // ── Error return trace runtime ────────────────────────────────
        // karac_error_trace_push(file_ptr: ptr, file_len: i64, line: i32, col: i32) -> void
        let i32_ty = context.i32_type();
        let trace_push_ty = context.void_type().fn_type(
            &[
                ptr_md,
                i64_ty,
                BasicMetadataTypeEnum::from(i32_ty),
                BasicMetadataTypeEnum::from(i32_ty),
            ],
            false,
        );
        let karac_error_trace_push_fn = module.add_function(
            "karac_error_trace_push",
            trace_push_ty,
            Some(Linkage::External),
        );
        // karac_error_trace_clear() -> void
        let trace_clear_ty = context.void_type().fn_type(&[], false);
        let karac_error_trace_clear_fn = module.add_function(
            "karac_error_trace_clear",
            trace_clear_ty,
            Some(Linkage::External),
        );

        Codegen {
            context,
            module,
            builder,
            variables: HashMap::new(),
            var_type_names: HashMap::new(),
            current_fn: None,
            printf_fn,
            snprintf_fn,
            struct_types: HashMap::new(),
            struct_field_names: HashMap::new(),
            struct_field_type_names: HashMap::new(),
            enum_layouts: HashMap::new(),
            loop_stack: Vec::new(),
            generic_fns: HashMap::new(),
            generated_monos: HashSet::new(),
            type_subst: HashMap::new(),
            const_subst: HashMap::new(),
            closure_counter: 0,
            indexed_elem_counter: 0,
            closure_fn_types: HashMap::new(),
            pending_closure_fn_type: None,
            pending_closure_param_hints: None,
            shared_types: HashMap::new(),
            malloc_fn,
            free_fn,
            exit_fn,
            memcmp_fn,
            len_alias: HashMap::new(),
            asserted_index_bounds: Vec::new(),
            vec_elem_types: HashMap::new(),
            slice_elem_types: HashMap::new(),
            fn_param_slice_elem: HashMap::new(),
            ref_params: HashMap::new(),
            fn_param_ref: HashMap::new(),
            soa_layouts: HashMap::new(),
            scope_cleanup_actions: Vec::new(),
            pattern_binding_is_borrow: false,
            enum_drop_fns: HashMap::new(),
            struct_drop_fns: HashMap::new(),
            question_conversions: HashMap::new(),
            callee_effectful: HashMap::new(),
            method_callee_types: HashMap::new(),
            pattern_binding_types: HashMap::new(),
            pattern_binding_inner_types: HashMap::new(),
            pattern_binding_borrow_modes: HashMap::new(),
            consts: HashMap::new(),
            source_filename: None,
            source_filename_global: None,
            source_text: None,
            used_symbols: Vec::new(),
            branch_cancel_ptr: None,
            rc_fallback_fns: HashMap::new(),
            arc_fallback_fns: HashMap::new(),
            rc_fallback_heap_types: HashMap::new(),
            concurrency_decisions: HashMap::new(),
            current_fn_name: String::new(),
            par_counter: 0,
            karac_branch_ty,
            karac_par_run_fn,
            spawn_sites: Vec::new(),
            runtime_debug_metadata_enabled: read_runtime_debug_metadata_env(),
            auto_par_disabled: !read_auto_par_env(),
            provider_resource_ids: HashMap::new(),
            provider_resource_traits: HashMap::new(),
            provider_trait_methods: HashMap::new(),
            provider_vtables: HashMap::new(),
            karac_provider_push_fn,
            karac_provider_pop_fn,
            karac_provider_lookup_fn,
            karac_provider_get_stack_head_fn,
            karac_provider_set_stack_head_fn,
            provider_frame_ty,
            provider_lookup_result_ty,
            map_key_types: HashMap::new(),
            map_val_types: HashMap::new(),
            map_key_type_names: HashMap::new(),
            var_elem_type_exprs: HashMap::new(),
            map_key_type_exprs: HashMap::new(),
            set_elem_types: HashMap::new(),
            set_elem_type_names: HashMap::new(),
            set_elem_type_exprs: HashMap::new(),
            string_vars: HashSet::new(),
            http_shim_cache: HashMap::new(),
            karac_map_new_fn,
            karac_map_free_fn,
            karac_map_free_with_drop_vec_fn,
            karac_map_insert_old_fn,
            karac_map_get_fn,
            karac_map_remove_old_fn,
            karac_map_contains_fn,
            karac_map_len_fn,
            karac_map_clear_fn,
            karac_map_iter_new_fn,
            karac_map_iter_next_fn,
            karac_map_iter_free_fn,
            karac_string_decode_char_fn,
            karac_map_entry_fn,
            karac_map_lookup_slot_fn,
            karac_string_clone_fn,
            clone_fn_cache: HashMap::new(),
            drop_fn_cache: HashMap::new(),
            map_mono_methods: HashMap::new(),
            display_fn_cache: HashMap::new(),
            karac_error_trace_push_fn,
            karac_error_trace_clear_fn,
            target_data: None,
        }
    }

    /// Lazily build (and cache) the host-target `TargetData` used by the
    /// layout-introspection intrinsics. Most modules never invoke
    /// `align_of[T]()` / `offset_of[T](field)`, so we pay the
    /// `Target::initialize_native` + `create_target_machine` cost only
    /// when the first such intrinsic is lowered.
    fn ensure_target_data(&mut self) -> Result<&TargetData, String> {
        if self.target_data.is_none() {
            let tm = create_target_machine()?;
            self.target_data = Some(tm.get_target_data());
        }
        Ok(self.target_data.as_ref().unwrap())
    }

    /// Populate RC-fallback data from an ownership check result.
    ///
    /// Two side-tables land here:
    /// * `rc_fallback_fns` — every binding the dataflow flagged for heap-boxing
    ///   + refcount.
    /// * `arc_fallback_fns` — the subset of those that also cross a `par {}`
    ///   thread boundary (Phase 2 promotion). Codegen routes inc/dec on the
    ///   subset to the atomic path (`atomicrmw add` / `atomicrmw sub`,
    ///   `SeqCst`); the rest stay on plain non-atomic ops. Allocation is
    ///   identical for both — the heap shape is `{ refcount: i64, payload: T }`
    ///   regardless of flavor and the initial `refcount = 1` store happens
    ///   before the value is shared.
    fn load_rc_fallback(&mut self, ownership: Option<&OwnershipCheckResult>) {
        let Some(ow) = ownership else { return };
        for (fn_name, rc_map) in &ow.rc_values {
            let names: HashSet<String> = rc_map.keys().cloned().collect();
            self.rc_fallback_fns.insert(fn_name.clone(), names);
        }
        for (fn_name, arc_set) in &ow.arc_values {
            self.arc_fallback_fns
                .insert(fn_name.clone(), arc_set.clone());
        }
    }

    /// Set the source filename used for `karac_error_trace_push` calls at
    /// `?` failure sites. See the field doc on `source_filename`.
    fn set_source_filename(&mut self, filename: Option<&str>) {
        self.source_filename = filename.map(|s| s.to_string());
    }

    /// Set the source text used by `record_spawn_site` to resolve byte
    /// offsets to `(line, col)` for the `KARAC_SPAWN_SITES` metadata
    /// table (Debugger Contract slice 3). Mirrors `set_source_filename`.
    fn set_source_text(&mut self, text: Option<&str>) {
        self.source_text = text.map(|s| s.to_string());
    }

    /// Mint a fresh `SpawnSiteId` and record a `SpawnSiteRecord` for the
    /// par block at `span` with `worker_count` static branches. Returns
    /// the assigned ID. The ID is the value of `par_counter` at entry —
    /// using a single counter for both par-branch function naming and
    /// SpawnSiteId minting keeps `__par_branch_<id>_<i>` and the metadata
    /// table in lockstep.
    ///
    /// Recording happens unconditionally (regardless of
    /// `runtime_debug_metadata_enabled`) so the IDs are stable across
    /// the gate-on / gate-off boundary; the gate decides only whether
    /// the emitted globals are populated. See `Codegen::spawn_sites` and
    /// the slice 3 plan in `phase-8-stdlib-floor.md`.
    fn record_spawn_site(&mut self, span: &Span, worker_count: Option<u32>) -> u32 {
        let id = self.par_counter;
        self.par_counter += 1;
        let (line, col) = match self.source_text.as_deref() {
            Some(src) => {
                let (l, c) = crate::byte_offset_to_line_col(src, span.offset);
                (l as u32, c as u32)
            }
            None => (span.line as u32, span.column as u32),
        };
        let file = self.source_filename.clone().unwrap_or_default();
        self.spawn_sites.push(SpawnSiteRecord {
            id,
            file,
            line,
            col,
            worker_count,
        });
        id
    }

    fn is_rc_fallback_binding(&self, name: &str) -> bool {
        self.rc_fallback_fns
            .get(&self.current_fn_name)
            .is_some_and(|set| set.contains(name))
    }

    /// True iff `name` was promoted to Arc in the current function — i.e. it
    /// lives in the `arc_values` subset for this function key. Inc/dec on
    /// such bindings must use the atomic path.
    fn is_arc_binding(&self, name: &str) -> bool {
        self.arc_fallback_fns
            .get(&self.current_fn_name)
            .is_some_and(|set| set.contains(name))
    }

    /// Populate concurrency-analysis data from a `ConcurrencyAnalysis` result.
    ///
    /// Mirrors `load_rc_fallback`: walks `analysis.function_decisions` and
    /// clones each entry into `concurrency_decisions`. The plumbing is
    /// behavior-neutral — slice 2 will consume the loaded decisions to emit
    /// `karac_par_run` for compiler-inferred parallel groups outside
    /// explicit `par {}` blocks. `None` is a no-op (the existing `par`
    /// codegen path stays in effect).
    fn load_concurrency_analysis(&mut self, analysis: Option<&ConcurrencyAnalysis>) {
        let Some(an) = analysis else { return };
        for (fn_name, decision) in &an.function_decisions {
            self.concurrency_decisions
                .insert(fn_name.clone(), decision.clone());
        }
    }

    /// Look up the parallelization decision for the function currently being
    /// compiled. Returns `None` when no concurrency analysis was threaded in
    /// (the legacy entry-point path) or when the current function isn't
    /// keyed in the analysis (e.g. compiler-synthesized helpers). Slice 2
    /// consumes this in `compile_function_body` to decide whether to emit
    /// `karac_par_run` for compiler-inferred parallel groups outside
    /// explicit `par {}` blocks.
    fn parallel_groups_for_current_fn(&self) -> Option<&FunctionConcurrency> {
        if self.concurrency_decisions.is_empty() {
            return None;
        }
        self.concurrency_decisions.get(&self.current_fn_name)
    }

    // ── Program / function compilation ───────────────────────────

    fn compile_program(&mut self, program: &Program) -> Result<(), String> {
        // Seed `Option` / `Result` layouts before walking struct fields so
        // a `shared struct N { mut left: Option[N] }` declaration's field-
        // type lowering finds the `{i64 tag, i64 payload}` layout via
        // `llvm_type_for_name("Option")` and embeds a 2-word slot in the
        // heap struct, rather than collapsing the field to the default
        // `i64` and losing the payload word.
        self.seed_builtin_enum_layouts();
        self.declare_structs(program);
        self.declare_enums(program);
        self.collect_soa_layouts(program);
        self.declare_extern_functions(program)?;

        // Side-table set by `lowering::lower_program`: at each `?` site that
        // requires `From`-based error conversion, the target type name is
        // recorded so `compile_question` can emit `Target.from(e)` ahead of
        // the early-return.
        self.question_conversions = program.question_conversions.clone();

        // Side-table set by the cli pipeline after effectcheck: per-callee
        // boolean indicating whether the callee carries any observable
        // effect (reads/writes/sends/receives). Read by
        // `emit_branch_cancel_check` to skip the cancel atomic load when the
        // callee is provably non-observable.
        self.callee_effectful = program.callee_effectful.clone();

        // Side-table set by `lowering::lower_program`: each `MethodCall`
        // expression's span maps to the canonical `Type.method` callee key.
        // Read by `compile_method_call` so the par-branch cancel-check
        // narrowing applies to instance methods, not just free-function
        // and `Type.assoc` calls.
        self.method_callee_types = program.method_callee_types.clone();

        // Side-table set by `lowering::lower_program`: each pattern-
        // binding's span maps to its surface type name. Read by
        // `bind_pattern_values` to reconstitute struct payloads from the
        // i64 word at match-arm bind sites — so `Err(e) => e.field` works
        // when the variant payload is a struct.
        self.pattern_binding_types = program.pattern_binding_types.clone();

        // Side-table set by `lowering::lower_program`: each pattern-
        // binding's span maps to its inner element TypeExpr for `Vec[T]` /
        // `Slice[T]` bindings only. Read by `bind_pattern_values` to
        // populate `vec_elem_types` / `slice_elem_types` under the
        // binding's variable name so direct method dispatch on the
        // binding (`xs.len()`, `xs[0]`, `xs.push(...)`) routes through
        // the right element-typed path. PB sibling slice (2026-05-09).
        self.pattern_binding_inner_types = program.pattern_binding_inner_types.clone();

        // Side-table set by `lowering::lower_program`: each pattern-
        // binding's span maps to its borrow form (`Ref` / `MutRef`) when
        // the enclosing match scrutinee is `ref T` / `mut ref T`. Owned
        // bindings are absent. Read by `bind_pattern_values` (Binding
        // arm) to wrap the value-typed leaf alloca in a ref-shim so call
        // sites that take a `ref T` / `mut ref T` parameter receive the
        // right ABI shape — slice 3a, 2026-05-14.
        self.pattern_binding_borrow_modes = program.pattern_binding_borrow_modes.clone();

        // Top-level `const NAME: T = value` collection. References from
        // function bodies (parsed as `ExprKind::Identifier(name)` for bare
        // uses) look up this map and re-compile the stored expression at
        // each use site; LLVM folds the resulting constant arithmetic.
        // Must precede function compilation so forward references work.
        for item in &program.items {
            if let Item::ConstDecl(c) = item {
                self.consts.insert(c.name.clone(), c.value.clone());
            }
        }

        // Theme 6: assign stable u32 IDs to `effect resource R[: T]`
        // declarations + capture each provider trait's method-declaration
        // order. The IDs flow through to `karac_provider_push` /
        // `karac_provider_lookup` calls; method order pins the vtable
        // layout. Must precede impl-method declaration so vtable emission
        // (after impl declarations) can read these tables.
        let mut next_resource_id: u32 = 0;
        for item in &program.items {
            if let Item::EffectResource(decl) = item {
                self.provider_resource_ids
                    .insert(decl.name.clone(), next_resource_id);
                next_resource_id += 1;
                if let Some(trait_name) = &decl.provider_trait {
                    self.provider_resource_traits
                        .insert(decl.name.clone(), trait_name.clone());
                }
            }
        }
        for item in &program.items {
            if let Item::TraitDef(t) = item {
                let methods: Vec<String> = t
                    .items
                    .iter()
                    .filter_map(|ti| match ti {
                        TraitItem::Method(m) => Some(m.name.clone()),
                        TraitItem::AssocType(_) => None,
                    })
                    .collect();
                self.provider_trait_methods.insert(t.name.clone(), methods);
            }
        }

        // First pass: register generic functions for on-demand monomorphization;
        // declare concrete (non-generic) functions for forward-call support.
        for item in &program.items {
            if let Item::Function(f) = item {
                if f.generic_params.is_some() {
                    self.generic_fns.insert(f.name.clone(), f.clone());
                } else {
                    self.declare_function(f)?;
                }
            }
        }

        // Declare user impl-block methods as LLVM functions named
        // "Type.method". Self-taking methods get `self` prepended as a
        // normal owned parameter (`ref self`/`mut ref self` deferred; all
        // current use sites — Eq/Ord/constructors — take `self` or no
        // receiver). Generic methods are deferred along with generic-fn
        // monomorphization.
        //
        // Duplicate impls (e.g. `impl PartialEq for Point { fn eq(ref self,
        // ref Point) }` and `impl Eq for Point { fn eq(self, Point) }` —
        // both legal in Kāra because `Eq` is a non-marker compat shim per
        // `runtime/stdlib/eq.kara`) collide on the LLVM symbol
        // `@Type.method`. We pick the value-self variant when there's a
        // choice: the lowering pass at `lowering.rs:543` dispatches `==`
        // via the `Eq` trait and emits `Type.eq(a, b)` with value-typed
        // arguments, so the function signature must accept values. A
        // ref-self body wouldn't compile correctly today anyway (deferred
        // — see comment above and `var_type_names` not being populated for
        // ref-typed params in `compile_function`). Two-pass iteration:
        // value-self impls first, then ref-self impls of the same method
        // are skipped as duplicates.
        let mut declared_impl_methods: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for value_self_pass in [true, false] {
            for item in &program.items {
                if let Item::ImplBlock(imp) = item {
                    if let Some(type_name) = impl_target_name(&imp.target_type) {
                        for impl_item in &imp.items {
                            if let ImplItem::Method(method) = impl_item {
                                if method.generic_params.is_some() {
                                    continue;
                                }
                                if method_self_is_value(method) != value_self_pass {
                                    continue;
                                }
                                let qualified = format!("{}.{}", type_name, method.name);
                                if !declared_impl_methods.insert(qualified) {
                                    continue;
                                }
                                let synth = make_impl_method_function(&type_name, method);
                                self.declare_function(&synth)?;
                            }
                        }
                    }
                }
            }
        }

        // Theme 6: emit static vtables for impls of provider traits.
        // Runs after impl methods are *declared* (their fn-ptrs become
        // vtable entries) but BEFORE function bodies are compiled — body
        // compilation may include `with_provider[R]` call sites that
        // need the vtable global to already exist in `provider_vtables`
        // for the lookup at sub-step 3 lowering time. Bodies don't need
        // to be compiled yet because the vtable only references fn-ptr
        // symbols which were established by `declare_function`.
        self.emit_provider_vtables(program);

        // Second pass: compile concrete functions (generic ones are compiled lazily).
        for item in &program.items {
            if let Item::Function(f) = item {
                if f.generic_params.is_none() {
                    self.compile_function(f)?;
                }
            }
        }

        // Compile user impl-block method bodies. Mirror the declaration
        // pass's value-self-first ordering so the body that gets compiled
        // is the same one whose signature was declared.
        let mut compiled_impl_methods: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for value_self_pass in [true, false] {
            for item in &program.items {
                if let Item::ImplBlock(imp) = item {
                    if let Some(type_name) = impl_target_name(&imp.target_type) {
                        for impl_item in &imp.items {
                            if let ImplItem::Method(method) = impl_item {
                                if method.generic_params.is_some() {
                                    continue;
                                }
                                if method_self_is_value(method) != value_self_pass {
                                    continue;
                                }
                                let qualified = format!("{}.{}", type_name, method.name);
                                if !compiled_impl_methods.insert(qualified) {
                                    continue;
                                }
                                let synth = make_impl_method_function(&type_name, method);
                                self.compile_function(&synth)?;
                            }
                        }
                    }
                }
            }
        }

        self.emit_llvm_used();
        self.emit_spawn_sites_metadata();

        self.module
            .verify()
            .map_err(|e| format!("Module verification failed: {}", e))
    }

    /// Materialize the special `@llvm.used` global from `used_symbols`.
    /// Standard LLVM convention: an `appending`-linkage array of pointers
    /// in section `llvm.metadata`. The linker treats every entry as
    /// implicitly referenced and preserves it across dead-code elimination.
    /// No-op when no `#[used]` symbols were declared.
    fn emit_llvm_used(&mut self) {
        if self.used_symbols.is_empty() {
            return;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let arr_ty = ptr_ty.array_type(self.used_symbols.len() as u32);
        let global = self.module.add_global(arr_ty, None, "llvm.used");
        let entries: Vec<inkwell::values::PointerValue<'ctx>> = self
            .used_symbols
            .iter()
            .map(|f| f.as_global_value().as_pointer_value())
            .collect();
        let init = ptr_ty.const_array(&entries);
        global.set_initializer(&init);
        global.set_linkage(inkwell::module::Linkage::Appending);
        global.set_section(Some("llvm.metadata"));
    }

    /// Emit the three module-scope globals that make up the SpawnSiteId
    /// metadata table — the foundation of the four-piece Debugger
    /// Contract specified in `design.md § AI-First Compiler Interface
    /// > Debugger Contract`. Slice 3 of the contract; consumed by
    /// slices 4 + 5 and the `std.panic` crash report's
    /// `parallel_context` field.
    ///
    /// Globals (external linkage; names are exact — runtime APIs and
    /// any external debugger tooling key on them verbatim):
    ///
    /// - `KARAC_SPAWN_SITES_ENABLED: i1` — `true` iff
    ///   `runtime_debug_metadata_enabled`. `std.runtime`'s
    ///   `has_debug_metadata()` (slice 5) reads this.
    /// - `KARAC_SPAWN_SITES_LEN: i32` — 0 when the gate is off,
    ///   `spawn_sites.len()` otherwise.
    /// - `KARAC_SPAWN_SITES: [N x SpawnSiteEntry]` where
    ///   `SpawnSiteEntry = { i32 id, ptr file_cstr, i32 line,
    ///   i32 col, i32 worker_count, i32 reserved }`.
    ///   The trailing `reserved` field is intentionally future-additive
    ///   (per `design.md § Debugger Contract > Stability`); it lets
    ///   future fields land within a major version without breaking
    ///   ABI. When the gate is off, `N == 0`.
    ///
    /// Per-filename `i8`-array globals are deduped by file path so a
    /// program with many `par {}` blocks in the same file emits only
    /// one filename string.
    fn emit_spawn_sites_metadata(&mut self) {
        let i32_ty = self.context.i32_type();
        let i8_ty = self.context.i8_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let bool_ty = self.context.bool_type();

        // Entry struct layout: { i32 id, ptr file_cstr, i32 line,
        //                        i32 col, i32 worker_count, i32 reserved }
        let entry_ty = self.context.struct_type(
            &[
                i32_ty.into(),
                ptr_ty.into(),
                i32_ty.into(),
                i32_ty.into(),
                i32_ty.into(),
                i32_ty.into(),
            ],
            false,
        );

        // Decide what the table contains. When the gate is off, emit
        // an empty array and a length of zero; the runtime API in
        // slice 5 reads through the same symbols regardless and
        // degrades cleanly (`list_par_blocks() == []`,
        // `has_debug_metadata() == false`).
        let emit_entries = self.runtime_debug_metadata_enabled;
        let len_value = if emit_entries {
            self.spawn_sites.len() as u32
        } else {
            0
        };

        // Build the per-filename `i8`-array globals (one per distinct
        // file path) and remember each as a pointer-to-first-byte.
        let mut file_globals: HashMap<String, PointerValue<'ctx>> = HashMap::new();
        if emit_entries {
            for record in &self.spawn_sites {
                if file_globals.contains_key(&record.file) {
                    continue;
                }
                // Null-terminated; use `const_string(.., true)` to
                // append the trailing NUL automatically.
                let cstr = self.context.const_string(record.file.as_bytes(), true);
                let arr_ty = i8_ty.array_type(cstr.get_type().len());
                let g = self.module.add_global(
                    arr_ty,
                    None,
                    &format!("karac.spawn_site_file.{}", file_globals.len()),
                );
                g.set_initializer(&cstr);
                g.set_linkage(Linkage::Private);
                g.set_constant(true);
                file_globals.insert(record.file.clone(), g.as_pointer_value());
            }
        }

        // Construct the array initializer.
        let entries_init: Vec<_> = if emit_entries {
            self.spawn_sites
                .iter()
                .map(|r| {
                    let file_ptr = file_globals
                        .get(&r.file)
                        .copied()
                        .unwrap_or_else(|| ptr_ty.const_null());
                    let id_v = i32_ty.const_int(r.id as u64, false);
                    let line_v = i32_ty.const_int(r.line as u64, false);
                    let col_v = i32_ty.const_int(r.col as u64, false);
                    let wc_v =
                        i32_ty.const_int(r.worker_count.map(|w| w as u64).unwrap_or(0), false);
                    let reserved_v = i32_ty.const_zero();
                    entry_ty.const_named_struct(&[
                        id_v.into(),
                        file_ptr.into(),
                        line_v.into(),
                        col_v.into(),
                        wc_v.into(),
                        reserved_v.into(),
                    ])
                })
                .collect()
        } else {
            Vec::new()
        };

        let arr_ty = entry_ty.array_type(entries_init.len() as u32);
        let arr_global = self.module.add_global(arr_ty, None, "KARAC_SPAWN_SITES");
        arr_global.set_initializer(&entry_ty.const_array(&entries_init));
        arr_global.set_linkage(Linkage::External);
        arr_global.set_constant(true);

        let len_global = self
            .module
            .add_global(i32_ty, None, "KARAC_SPAWN_SITES_LEN");
        len_global.set_initializer(&i32_ty.const_int(len_value as u64, false));
        len_global.set_linkage(Linkage::External);
        len_global.set_constant(true);

        let enabled_global = self
            .module
            .add_global(bool_ty, None, "KARAC_SPAWN_SITES_ENABLED");
        enabled_global.set_initializer(&bool_ty.const_int(
            if self.runtime_debug_metadata_enabled {
                1
            } else {
                0
            },
            false,
        ));
        enabled_global.set_linkage(Linkage::External);
        enabled_global.set_constant(true);
    }

    /// Lower `<receiver>.clone()` for an identifier-bound collection
    /// receiver (Vec[T], String, Map[K, V], Set[T]). Returns `Some(value)`
    /// when the receiver is recognised; `None` otherwise (caller falls
    /// through to the impl-block / generic dispatch so user `clone` impls
    /// keep working).
    ///
    /// Synthesises a `TypeExpr` for the receiver from the codegen side-
    /// tables (`vec_elem_types` / `var_elem_type_exprs` / `map_key_type_exprs`
    /// / `set_elem_type_exprs`), routes through `emit_clone_fn_for_type_expr`,
    /// and emits the `karac_clone_<typename>(src_slot, dst)` call. The
    /// destination is a fresh stack alloca that the caller's let-binding
    /// (or expression-statement) consumes. Scope-cleanup integration for
    /// the cloned value lives in subtask 6 — at this layer the alloca is
    /// just a temporary; the binding's slot inherits ownership when the
    /// `let` stores into it.
    fn try_compile_clone(&mut self, object: &Expr) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        let ExprKind::Identifier(name) = &object.kind else {
            return Ok(None);
        };
        let name_owned = name.clone();
        let span_zero = crate::token::Span {
            line: 0,
            column: 0,
            offset: 0,
            length: 0,
        };
        let mk_path = |seg: &str, args: Vec<TypeExpr>| -> TypeExpr {
            TypeExpr {
                kind: TypeKind::Path(crate::ast::PathExpr {
                    segments: vec![seg.to_string()],
                    generic_args: if args.is_empty() {
                        None
                    } else {
                        Some(args.into_iter().map(GenericArg::Type).collect())
                    },
                    span: span_zero.clone(),
                }),
                span: span_zero.clone(),
            }
        };

        // Build the receiver's TypeExpr from the side-tables. Order matters
        // — Set/Map come before Vec since Set's bucket is also routed through
        // map_key_types when lowered as Map[T, ()], and a Vec with elem=i8
        // overlaps with String at the LLVM-type level.
        let te: TypeExpr = if self.set_elem_types.contains_key(name_owned.as_str()) {
            let elem = self
                .set_elem_type_exprs
                .get(name_owned.as_str())
                .cloned()
                .ok_or_else(|| {
                    format!("clone: missing set_elem_type_exprs for '{}'", name_owned)
                })?;
            mk_path("Set", vec![elem])
        } else if self.map_key_types.contains_key(name_owned.as_str()) {
            let k = self
                .map_key_type_exprs
                .get(name_owned.as_str())
                .cloned()
                .ok_or_else(|| format!("clone: missing map_key_type_exprs for '{}'", name_owned))?;
            let v = self
                .var_elem_type_exprs
                .get(name_owned.as_str())
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "clone: missing var_elem_type_exprs (val) for '{}'",
                        name_owned
                    )
                })?;
            mk_path("Map", vec![k, v])
        } else if self.vec_elem_types.contains_key(name_owned.as_str()) {
            // Distinguish String from Vec[T]: String registers in
            // `vec_elem_types` (so the str-method dispatch finds it) but
            // skips `var_elem_type_exprs`. Vec[T] populates both.
            if let Some(elem_te) = self.var_elem_type_exprs.get(name_owned.as_str()).cloned() {
                mk_path("Vec", vec![elem_te])
            } else {
                mk_path("String", vec![])
            }
        } else {
            return Ok(None);
        };

        let clone_fn = self.emit_clone_fn_for_type_expr(&te);
        let llvm_ty = self.llvm_type_for_type_expr(&te);
        let fn_val = self
            .current_fn
            .ok_or_else(|| "clone: no current function".to_string())?;
        let dst = self.create_entry_alloca(fn_val, "clone.dst", llvm_ty);
        let src_slot = self
            .variables
            .get(name_owned.as_str())
            .copied()
            .ok_or_else(|| format!("clone: unknown variable '{}'", name_owned))?;
        self.builder
            .build_call(clone_fn, &[src_slot.ptr.into(), dst.into()], "")
            .unwrap();
        let dst_val = self.builder.build_load(llvm_ty, dst, "clone.val").unwrap();
        Ok(Some(dst_val))
    }

    /// Recognise the `Map.entry(k)` chain pattern and lower it as a single
    /// sequence. Returns `Some(value)` only when `<object>.<method>(<args>)`
    /// matches:
    ///
    /// ```text
    /// m.entry(k){.and_modify(f)}*.{or_insert(d)|or_insert_with(f)|and_modify(f)}
    /// ```
    ///
    /// where `m` is an Identifier-bound Map variable. The single `karac_map_entry`
    /// call at the chain root is followed by branch blocks for each
    /// `and_modify` (innermost first) and the terminal method, keeping the
    /// slot pointer valid for the whole sequence — exactly one hash per chain.
    ///
    /// The terminal method's return shape:
    ///
    /// - `or_insert(default)` / `or_insert_with(closure)` — returns the slot
    ///   pointer (`*mut V`), the LLVM realisation of `mut ref V`. Subsequent
    ///   `.push(row)` etc. on the result is the per-type Clone codegen story.
    /// - `and_modify(closure)` — returns the Entry struct value
    ///   `{slot_ptr, occupied}` so further chaining (`.or_insert(d)`) sees
    ///   the same Entry. v1 only nests further `and_modify`s on top; chained
    ///   terminal methods are recognised by recursing through this fn.
    fn try_compile_entry_chain(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        if !matches!(method, "or_insert" | "or_insert_with" | "and_modify") {
            return Ok(None);
        }
        // Peel `and_modify` wrappers off the receiver until we reach
        // `m.entry(k)`. Anything else means the receiver isn't an entry chain.
        // Closure exprs collected in outermost-first order; we reverse before
        // emitting so the innermost (= first written) and_modify runs first.
        let mut and_modify_closures: Vec<&Expr> = Vec::new();
        let mut current = object;
        let (map_obj, key_expr) = loop {
            let ExprKind::MethodCall {
                object: inner_obj,
                method: m,
                args: inner_args,
                ..
            } = &current.kind
            else {
                return Ok(None);
            };
            if m == "entry" && inner_args.len() == 1 {
                break (inner_obj.as_ref(), &inner_args[0].value);
            } else if m == "and_modify" && inner_args.len() == 1 {
                and_modify_closures.push(&inner_args[0].value);
                current = inner_obj;
            } else {
                return Ok(None);
            }
        };
        let ExprKind::Identifier(map_name) = &map_obj.kind else {
            return Ok(None);
        };
        if !self.map_key_types.contains_key(map_name.as_str()) {
            return Ok(None);
        }
        let map_name = map_name.clone();
        let value =
            self.emit_entry_chain(&map_name, key_expr, &and_modify_closures, method, args)?;
        Ok(Some(value))
    }

    /// Emit the entry-chain IR. Caller has already verified that
    /// `<map_name>` is a Map variable. Branches happen at every `and_modify`
    /// site and the terminal method, all sharing the slot pointer returned
    /// by the single `karac_map_entry` call.
    fn emit_entry_chain(
        &mut self,
        map_name: &str,
        key_expr: &Expr,
        and_modify_closures: &[&Expr],
        terminal_method: &str,
        terminal_args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let map_slot = self
            .variables
            .get(map_name)
            .copied()
            .ok_or_else(|| format!("entry chain: unknown map '{}'", map_name))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, map_slot.ptr, "entry.map.handle")
            .unwrap()
            .into_pointer_value();
        let key_ty = *self
            .map_key_types
            .get(map_name)
            .ok_or_else(|| format!("entry chain: missing key type for '{}'", map_name))?;
        let val_ty = *self
            .map_val_types
            .get(map_name)
            .ok_or_else(|| format!("entry chain: missing val type for '{}'", map_name))?;

        let fn_val = self.current_fn.unwrap();

        // Compile the key, store to alloca for the C ABI.
        let key_alloca = self.create_entry_alloca(fn_val, "entry.key", key_ty);
        let key_val = self.compile_expr(key_expr)?;
        self.builder.build_store(key_alloca, key_val).unwrap();

        // Out-pointer alloca: the runtime writes the slot value-pointer into
        // this slot. The slot pointer is `*mut V` after the call.
        let slot_pp = self.create_entry_alloca(fn_val, "entry.slot.pp", ptr_ty.into());

        // Pick the runtime fn based on the terminal: `or_insert` /
        // `or_insert_with` need the runtime to claim the bucket on Vacant
        // (so codegen can store the default through the slot pointer).
        // Bare `and_modify(...)` must NOT insert on Vacant — use the
        // lookup-only variant.
        let runtime_fn = match terminal_method {
            "or_insert" | "or_insert_with" => self.karac_map_entry_fn,
            "and_modify" => self.karac_map_lookup_slot_fn,
            _ => unreachable!("terminal method already validated by caller"),
        };
        let occupied = self
            .builder
            .build_call(
                runtime_fn,
                &[map_handle.into(), key_alloca.into(), slot_pp.into()],
                "entry.occupied",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let slot_ptr = self
            .builder
            .build_load(ptr_ty, slot_pp, "entry.slot.ptr")
            .unwrap()
            .into_pointer_value();

        // Inner `and_modify` closures — innermost first (chain order is
        // outermost-first; reverse to get execution order).
        for &am_closure in and_modify_closures.iter().rev() {
            self.emit_entry_and_modify(am_closure, occupied, slot_ptr, val_ty)?;
        }

        // Terminal method.
        match terminal_method {
            "or_insert" => {
                if terminal_args.is_empty() {
                    return Err("Entry.or_insert requires a default argument".to_string());
                }
                let store_bb = self.context.append_basic_block(fn_val, "or_ins.store");
                let merge_bb = self.context.append_basic_block(fn_val, "or_ins.merge");
                // Vacant (occupied=false) → store default; Occupied → merge.
                self.builder
                    .build_conditional_branch(occupied, merge_bb, store_bb)
                    .unwrap();
                self.builder.position_at_end(store_bb);
                let default_val = self.compile_expr(&terminal_args[0].value)?;
                self.builder.build_store(slot_ptr, default_val).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(slot_ptr.into())
            }
            "or_insert_with" => {
                if terminal_args.is_empty() {
                    return Err("Entry.or_insert_with requires a closure argument".to_string());
                }
                let store_bb = self.context.append_basic_block(fn_val, "or_ins_w.store");
                let merge_bb = self.context.append_basic_block(fn_val, "or_ins_w.merge");
                self.builder
                    .build_conditional_branch(occupied, merge_bb, store_bb)
                    .unwrap();
                self.builder.position_at_end(store_bb);
                let default_val =
                    self.invoke_inline_closure(&terminal_args[0].value, &[], val_ty)?;
                self.builder.build_store(slot_ptr, default_val).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(slot_ptr.into())
            }
            "and_modify" => {
                if terminal_args.is_empty() {
                    return Err("Entry.and_modify requires a closure argument".to_string());
                }
                self.emit_entry_and_modify(&terminal_args[0].value, occupied, slot_ptr, val_ty)?;
                // Return the Entry struct value `{slot_ptr, occupied}` so a
                // chained terminal sees both halves. Currently no callers
                // consume the struct directly (chained-after-terminal is
                // recognised by the dispatcher), but materialising it keeps
                // the contract honest.
                let entry_struct_ty = self
                    .context
                    .struct_type(&[ptr_ty.into(), self.context.bool_type().into()], false);
                let mut agg = entry_struct_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, slot_ptr, 0, "entry.slot.f")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, occupied, 1, "entry.occ.f")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            _ => unreachable!("terminal method already validated"),
        }
    }

    /// Emit the branch-and-call for one `and_modify(closure)` step. Closure
    /// is invoked only when `occupied` is true; receives the slot pointer
    /// (`*mut V`) so `|v| { v += 1 }` mutates through.
    fn emit_entry_and_modify(
        &mut self,
        closure_expr: &Expr,
        occupied: inkwell::values::IntValue<'ctx>,
        slot_ptr: PointerValue<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> Result<(), String> {
        let fn_val = self.current_fn.unwrap();
        let run_bb = self.context.append_basic_block(fn_val, "and_mod.run");
        let cont_bb = self.context.append_basic_block(fn_val, "and_mod.cont");
        self.builder
            .build_conditional_branch(occupied, run_bb, cont_bb)
            .unwrap();
        self.builder.position_at_end(run_bb);
        // The closure's mut-ref-V parameter is realised as a pointer-to-V.
        // We invoke inline with [slot_ptr] so the closure body's mutations
        // through the parameter target the map slot directly. The body's
        // value type is V (loaded once at param bind, stored back at exit).
        self.invoke_and_modify_closure(closure_expr, slot_ptr, val_ty)?;
        self.builder.build_unconditional_branch(cont_bb).unwrap();
        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    /// Invoke a closure expression inline. The closure is compiled to a fat
    /// pointer `{fn_ptr, env_ptr}`; we extract both halves and `build_indirect_call`
    /// with `[env_ptr, ...args]`. Used by `or_insert_with`'s no-arg closure
    /// invocation.
    ///
    /// `expected_return_ty` is the V type the slot stores; the return value
    /// is coerced to it via `coerce_to_i64` and back when needed (in practice
    /// all V types this fn sees fit in a register and round-trip through
    /// the closure return slot losslessly).
    fn invoke_inline_closure(
        &mut self,
        closure_expr: &Expr,
        extra_args: &[BasicValueEnum<'ctx>],
        _expected_return_ty: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let closure_val = self.compile_expr(closure_expr)?;
        let fn_type = self
            .pending_closure_fn_type
            .take()
            .ok_or_else(|| "entry chain: inline closure missing fn_type".to_string())?;
        let fat_sv = closure_val.into_struct_value();
        let fn_ptr = self
            .builder
            .build_extract_value(fat_sv, 0, "entry.cls.fn")
            .unwrap()
            .into_pointer_value();
        let env_ptr = self
            .builder
            .build_extract_value(fat_sv, 1, "entry.cls.env")
            .unwrap()
            .into_pointer_value();
        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
            vec![BasicMetadataValueEnum::from(env_ptr)];
        for &arg in extra_args {
            call_args.push(BasicMetadataValueEnum::from(arg));
        }
        let call = self
            .builder
            .build_indirect_call(fn_type, fn_ptr, &call_args, "entry.cls.call")
            .unwrap();
        let basic = call.try_as_basic_value();
        if basic.is_instruction() {
            Ok(self.context.i64_type().const_int(0, false).into())
        } else {
            Ok(basic.unwrap_basic())
        }
    }

    /// Specialised closure-invocation for `and_modify`. The closure's
    /// parameter is `mut ref V` per the spec, but Kāra closures default to
    /// passing user params by value when unannotated (`|v| { v += 1 }`). To
    /// preserve the mut-ref-V semantic without surgery on the closure-param
    /// type-inference path, we inline the closure body directly: bind the
    /// closure parameter name to a local alloca initialised from `slot_ptr`,
    /// run the body, then store the alloca value back through `slot_ptr`.
    /// The closure-fn boundary is bypassed entirely — mutations to the
    /// parameter inside the body are mutations to the slot.
    ///
    /// Restriction: only inline `ExprKind::Closure` exprs are supported (the
    /// common case — `m.entry(k).and_modify(|v| { ... })`). Named-fn forms
    /// like `m.entry(k).and_modify(f)` for a previously-bound `f` would
    /// require the indirect-call path; left unsupported for v1 since the
    /// stdlib spec only documents the inline closure form.
    fn invoke_and_modify_closure(
        &mut self,
        closure_expr: &Expr,
        slot_ptr: PointerValue<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> Result<(), String> {
        let ExprKind::Closure { params, body, .. } = &closure_expr.kind else {
            return Err("entry chain: and_modify expects an inline closure expression".to_string());
        };
        // Closure must have exactly one user-side parameter — the `mut ref V`.
        let Some(param) = params.first() else {
            return Err("entry chain: and_modify closure has no parameter".to_string());
        };
        let PatternKind::Binding(param_name) = &param.pattern.kind else {
            return Err(
                "entry chain: and_modify closure parameter must be an identifier".to_string(),
            );
        };
        let fn_val = self.current_fn.unwrap();

        // Bind param to an alloca initialised from the slot. The body's
        // mutations through `param_name` write the alloca; we store back
        // to `slot_ptr` after the body exits.
        let local = self.create_entry_alloca(fn_val, param_name, val_ty);
        let initial = self
            .builder
            .build_load(val_ty, slot_ptr, "entry.am.load")
            .unwrap();
        self.builder.build_store(local, initial).unwrap();
        let saved_slot = self.variables.insert(
            param_name.clone(),
            VarSlot {
                ptr: local,
                ty: val_ty,
            },
        );

        // Compile the body in the enclosing scope so it can see captures
        // (the typical case: `|v| { v += 1 }` only reads param-local `v`).
        // body is an Expr; if it's a block we evaluate for side effects.
        let _body_val = self.compile_expr(body)?;

        // Restore the prior binding (if any) and write back the mutated V.
        match saved_slot {
            Some(prev) => {
                self.variables.insert(param_name.clone(), prev);
            }
            None => {
                self.variables.remove(param_name);
            }
        }
        let new_v = self
            .builder
            .build_load(val_ty, local, "entry.am.new")
            .unwrap();
        self.builder.build_store(slot_ptr, new_v).unwrap();
        Ok(())
    }

    /// Compile method calls on `Set[T]` variables. `Set[T]` lowers to
    /// `Map[T, ()]` at codegen so all Map runtime fns are reused; the
    /// value-side allocas are sized to the (zero-byte) unit type and the
    /// runtime's `(key_size + val_size).max(1)` makes the value-store a
    /// no-op.
    ///
    /// Handled methods: `len`, `is_empty`, `insert`, `contains`, `remove`,
    /// `clear`. `union` / `intersection` / `difference` are out-of-line in
    /// `compile_set_op_method` so this fn stays focused on the runtime-
    /// passthrough cases.
    fn compile_set_method(
        &mut self,
        var_name: &str,
        method: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let bool_t = self.context.bool_type();
        let i8_t = self.context.i8_type();

        let slot = self
            .variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown set variable '{var_name}'"))?;
        let set_handle = self
            .builder
            .build_load(ptr_ty, slot.ptr, "set.handle")
            .unwrap()
            .into_pointer_value();

        let elem_ty = self
            .set_elem_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        match method {
            "len" => {
                let len = self
                    .builder
                    .build_call(self.karac_map_len_fn, &[set_handle.into()], "set.len")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(len)
            }
            "is_empty" => {
                let len = self
                    .builder
                    .build_call(self.karac_map_len_fn, &[set_handle.into()], "set.len")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let zero = i64_t.const_int(0, false);
                let cmp = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::EQ, len, zero, "set.is_empty")
                    .unwrap();
                Ok(cmp.into())
            }
            "insert" => {
                if args.is_empty() {
                    return Err("Set.insert requires a value argument".to_string());
                }
                let elem_val = self.compile_expr(&args[0].value)?;
                // Move semantics for tracked Vec/String elements: the
                // bucket bit-copies the element's `{ptr, len, cap}` and
                // the `karac_map_free_with_drop_vec` cleanup (when
                // `key_is_vec = true` for `Set[Vec[T]]` / `Set[String]`)
                // would double-free against the source binding's own
                // scope-exit `FreeVecBuffer`. Suppress so the Set
                // becomes the unique owner. Mirrors the `Map.insert`
                // key-side suppression added alongside the recursive
                // key-drop path.
                self.suppress_source_vec_cleanup_for_arg(&args[0].value);
                let fn_val = self.current_fn.unwrap();
                let elem_slot = self.create_entry_alloca(fn_val, "set.insert.elem", elem_ty);
                self.builder.build_store(elem_slot, elem_val).unwrap();
                // val_size = 0, so dummy_unit / dummy_out can be a single
                // shared i8 alloca — the runtime store-of-zero-bytes is a
                // no-op regardless of the byte's contents.
                let dummy = self.create_entry_alloca(fn_val, "set.dummy", i8_t.into());
                let existed = self
                    .builder
                    .build_call(
                        self.karac_map_insert_old_fn,
                        &[
                            set_handle.into(),
                            elem_slot.into(),
                            dummy.into(),
                            dummy.into(),
                        ],
                        "set.insert.existed",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                // Set.insert returns true when the value was newly inserted
                // (matches Rust HashSet::insert), so flip `existed`.
                let one = bool_t.const_int(1, false);
                let inserted = self
                    .builder
                    .build_xor(existed, one, "set.insert.inserted")
                    .unwrap();
                Ok(inserted.into())
            }
            "contains" => {
                if args.is_empty() {
                    return Err("Set.contains requires a value argument".to_string());
                }
                let elem_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                let elem_slot = self.create_entry_alloca(fn_val, "set.contains.elem", elem_ty);
                self.builder.build_store(elem_slot, elem_val).unwrap();
                let found = self
                    .builder
                    .build_call(
                        self.karac_map_contains_fn,
                        &[set_handle.into(), elem_slot.into()],
                        "set.contains",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(found)
            }
            "remove" => {
                if args.is_empty() {
                    return Err("Set.remove requires a value argument".to_string());
                }
                let elem_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                let elem_slot = self.create_entry_alloca(fn_val, "set.remove.elem", elem_ty);
                self.builder.build_store(elem_slot, elem_val).unwrap();
                // val_size = 0 → dummy out slot is shared; contents irrelevant.
                let dummy = self.create_entry_alloca(fn_val, "set.dummy", i8_t.into());
                let existed = self
                    .builder
                    .build_call(
                        self.karac_map_remove_old_fn,
                        &[set_handle.into(), elem_slot.into(), dummy.into()],
                        "set.remove.existed",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(existed)
            }
            "clear" => {
                self.builder
                    .build_call(self.karac_map_clear_fn, &[set_handle.into()], "")
                    .unwrap();
                Ok(i64_t.const_int(0, false).into())
            }
            "union" | "intersection" | "difference" => {
                if args.is_empty() {
                    return Err(format!("Set.{method} requires another set as argument"));
                }
                let other_handle = self.compile_expr(&args[0].value)?.into_pointer_value();
                // Element TypeExpr drives clone/hash/eq fn synthesis. Without
                // it we can't deep-clone non-Copy elements (String, …) safely.
                let elem_te = self
                    .set_elem_type_exprs
                    .get(var_name)
                    .cloned()
                    .ok_or_else(|| {
                        format!("codegen: Set.{method} missing elem TypeExpr for '{var_name}'")
                    })?;

                let elem_size = elem_ty
                    .size_of()
                    .unwrap_or_else(|| i64_t.const_int(8, false));
                let val_size = i64_t.const_int(0, false);
                let hash_fn = self.emit_hash_fn_for_type_expr(&elem_te);
                let eq_fn = self.emit_eq_fn_for_type_expr(&elem_te);

                let new_handle = self
                    .builder
                    .build_call(
                        self.karac_map_new_fn,
                        &[
                            elem_size.into(),
                            val_size.into(),
                            hash_fn.as_global_value().as_pointer_value().into(),
                            eq_fn.as_global_value().as_pointer_value().into(),
                        ],
                        "set.op.new",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_pointer_value();

                match method {
                    "union" => {
                        // Clone all of self → dst (dst empty, no duplicates),
                        // then iterate other and insert clones for elements
                        // not already in self. The "skip if in self" check
                        // (rather than "skip if in dst") avoids a probe into
                        // the partially-built dst.
                        self.emit_set_op_iter(
                            set_handle,
                            new_handle,
                            SetOpFilter::Always,
                            &elem_te,
                        );
                        self.emit_set_op_iter(
                            other_handle,
                            new_handle,
                            SetOpFilter::NotContainsIn(set_handle),
                            &elem_te,
                        );
                    }
                    "intersection" => {
                        self.emit_set_op_iter(
                            set_handle,
                            new_handle,
                            SetOpFilter::ContainsIn(other_handle),
                            &elem_te,
                        );
                    }
                    "difference" => {
                        self.emit_set_op_iter(
                            set_handle,
                            new_handle,
                            SetOpFilter::NotContainsIn(other_handle),
                            &elem_te,
                        );
                    }
                    _ => unreachable!(),
                }
                Ok(new_handle.into())
            }
            _ => Err(format!("codegen: Set.{method} not yet implemented")),
        }
    }

    /// Iterate `src_handle`, optionally filter elements through `mode`,
    /// per-element-clone the survivors and insert them into `dst_handle`.
    /// Used by `Set.union` / `intersection` / `difference` codegen — each
    /// op materialises a fresh empty `dst_handle` and calls this once
    /// (intersection / difference) or twice (union: once unfiltered from
    /// `self`, once filtered against `self` from `other`).
    ///
    /// The "skip" branch jumps back to the iterator header, preserving the
    /// invariant that `karac_map_iter_free` runs exactly once per call —
    /// at the exit block, only after `karac_map_iter_next` returned false.
    /// Element clones for skipped survivors never happen, so there is no
    /// leak even when the per-element clone allocates (e.g. `String`).
    fn emit_set_op_iter(
        &mut self,
        src_handle: PointerValue<'ctx>,
        dst_handle: PointerValue<'ctx>,
        mode: SetOpFilter<'ctx>,
        elem_te: &TypeExpr,
    ) {
        let i8_t = self.context.i8_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);
        let clone_fn = self.emit_clone_fn_for_type_expr(elem_te);
        let fn_val = self.current_fn.unwrap();

        let elem_out = self.create_entry_alloca(fn_val, "setop.k.out", elem_ty);
        let clone_slot = self.create_entry_alloca(fn_val, "setop.k.clone", elem_ty);
        let dummy = self.create_entry_alloca(fn_val, "setop.dummy", i8_t.into());

        let iter_handle = self
            .builder
            .build_call(
                self.karac_map_iter_new_fn,
                &[src_handle.into()],
                "setop.iter",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let hdr_bb = self.context.append_basic_block(fn_val, "setop.iter.hdr");
        let bdy_bb = self.context.append_basic_block(fn_val, "setop.iter.bdy");
        let exit_bb = self.context.append_basic_block(fn_val, "setop.iter.exit");
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let has = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_handle.into(), elem_out.into(), dummy.into()],
                "setop.iter.has",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        match mode {
            SetOpFilter::Always => {}
            SetOpFilter::ContainsIn(other) | SetOpFilter::NotContainsIn(other) => {
                let pass_bb = self.context.append_basic_block(fn_val, "setop.iter.pass");
                let found = self
                    .builder
                    .build_call(
                        self.karac_map_contains_fn,
                        &[other.into(), elem_out.into()],
                        "setop.contains",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let cond = match mode {
                    SetOpFilter::ContainsIn(_) => found,
                    SetOpFilter::NotContainsIn(_) => self
                        .builder
                        .build_xor(
                            found,
                            self.context.bool_type().const_int(1, false),
                            "setop.neg",
                        )
                        .unwrap(),
                    SetOpFilter::Always => unreachable!(),
                };
                self.builder
                    .build_conditional_branch(cond, pass_bb, hdr_bb)
                    .unwrap();
                self.builder.position_at_end(pass_bb);
            }
        }
        self.builder
            .build_call(clone_fn, &[elem_out.into(), clone_slot.into()], "")
            .unwrap();
        self.builder
            .build_call(
                self.karac_map_insert_fn(),
                &[dst_handle.into(), clone_slot.into(), dummy.into()],
                "",
            )
            .unwrap();
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_handle.into()], "")
            .unwrap();
    }

    fn compile_array_literal(&mut self, elems: &[Expr]) -> Result<BasicValueEnum<'ctx>, String> {
        if elems.is_empty() {
            return Ok(self.context.i64_type().const_int(0, false).into());
        }
        let vals: Vec<BasicValueEnum<'ctx>> = elems
            .iter()
            .map(|e| self.compile_expr(e))
            .collect::<Result<_, _>>()?;
        let elem_ty = vals[0].get_type();
        let arr_ty = elem_ty.array_type(vals.len() as u32);
        let mut agg = arr_ty.get_undef();
        for (idx, val) in vals.iter().enumerate() {
            agg = self
                .builder
                .build_insert_value(agg, *val, idx as u32, "arr.elem")
                .unwrap()
                .into_array_value();
        }
        Ok(agg.into())
    }

    /// Let-binding fast path for `let buf: Array[T, N] = [zero; N]`.
    /// Returns `Some(Ok(()))` on success, `None` if the RHS doesn't match
    /// the literal-zero repeat pattern (caller falls through to the
    /// general `compile_expr` path), or `Some(Err)` on a structural
    /// problem (e.g. unsupported element type).
    ///
    /// Lowers to `alloca [N x T]; call @llvm.memset.*(alloca, 0, N*sizeof(T))`,
    /// bypassing the `store [N x T] zeroinitializer` IR that LLVM's downstream
    /// codegen passes crash on at large N. The memset is what LLVM would emit
    /// for the aggregate store anyway — this just sidesteps the codegen path
    /// that explodes the constant store into per-element machine instructions.
    ///
    /// Matched literal-zero shapes: `Integer(0)`, `Bool(false)`, `Float`
    /// whose IEEE bit pattern is all-zero (`+0.0`, not `-0.0`).
    fn try_emit_zero_init_array_let(
        &mut self,
        name: &str,
        value: &Expr,
        ty: Option<&TypeExpr>,
    ) -> Option<Result<(), String>> {
        let ExprKind::RepeatLiteral {
            type_name,
            value: rep_val,
            count,
        } = &value.kind
        else {
            return None;
        };
        // Vec form has its own heap-alloc shape — out of scope.
        if matches!(type_name.as_deref(), Some("Vec")) {
            return None;
        }
        // Literal-zero detection. Floats use bit-pattern equality so `-0.0`
        // doesn't take the path (would lose the sign bit).
        let is_zero_lit = match &rep_val.kind {
            ExprKind::Integer(0, _) => true,
            ExprKind::Bool(false) => true,
            ExprKind::Float(f, _) => f.to_bits() == 0,
            _ => false,
        };
        if !is_zero_lit {
            return None;
        }
        let n = match &count.kind {
            ExprKind::Integer(n, _) if *n > 0 => *n as u32,
            _ => return None,
        };
        // Element LLVM type: from `Array[T, N]` annotation if present, else
        // inferred from the literal's natural type.
        let elem_llvm_ty: BasicTypeEnum<'ctx> = if let Some(te) = ty {
            let TypeKind::Path(path) = &te.kind else {
                return None;
            };
            if path.segments.first().map(|s| s.as_str()) != Some("Array") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if args.len() != 2 {
                return None;
            }
            match &args[0] {
                GenericArg::Type(t) => self.llvm_type_for_type_expr(t),
                _ => return None,
            }
        } else {
            match &rep_val.kind {
                ExprKind::Integer(_, _) => self.context.i64_type().into(),
                ExprKind::Bool(_) => self.context.bool_type().into(),
                ExprKind::Float(_, _) => self.context.f64_type().into(),
                _ => return None,
            }
        };
        let arr_ty = elem_llvm_ty.array_type(n);
        let fn_val = self.current_fn?;
        let alloca = self.create_entry_alloca(fn_val, name, arr_ty.into());
        let total_size = arr_ty.size_of()?;
        let memset_result = self.builder.build_memset(
            alloca,
            1, // align 1 — LLVM picks up the alloca's natural alignment
            self.context.i8_type().const_zero(),
            total_size,
        );
        if let Err(e) = memset_result {
            return Some(Err(format!("build_memset failed: {:?}", e)));
        }
        self.variables.insert(
            name.to_string(),
            VarSlot {
                ptr: alloca,
                ty: arr_ty.into(),
            },
        );
        Some(Ok(()))
    }

    /// Compile `[value; count]` / `Array[value; count]`. Produces an LLVM
    /// array value `[N x T]` whose every element is the compiled `value`.
    /// `count` must be a non-negative integer literal (mirrors the
    /// typechecker's `Array[T, N]` size constraint).
    ///
    /// `Vec[v; n]` prefix form needs heap allocation + fill and is not
    /// implemented here yet — it errors with a clear message rather than
    /// silently producing the wrong shape.
    fn compile_repeat_literal(
        &mut self,
        type_name: Option<&str>,
        value: &Expr,
        count: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if matches!(type_name, Some("Vec")) {
            return Err("codegen: Vec[v; n] repeat literal not yet supported".to_string());
        }
        let n = match &count.kind {
            ExprKind::Integer(n, _) if *n >= 0 => *n as u32,
            _ => {
                return Err(
                    "repeat-literal count must be a non-negative integer literal".to_string(),
                );
            }
        };
        let val = self.compile_expr(value)?;
        let elem_ty = val.get_type();
        let arr_ty = elem_ty.array_type(n);

        // Zero-value fast path. When `val` is the zero/null/false value of
        // its type, emit a single LLVM `zeroinitializer` constant — a
        // single IR token regardless of N. Covers `[0; N]`, `[false; N]`,
        // `[0.0; N]`, `[null; N]` — the common stack-array initialization
        // shapes (lookup tables, sieve buffers, zero-filled work arrays).
        // O(1) compile time in N; works at any size LLVM can represent.
        let is_zero = match val {
            BasicValueEnum::IntValue(iv) => iv.get_zero_extended_constant() == Some(0),
            BasicValueEnum::FloatValue(fv) => {
                fv.get_constant().is_some_and(|(v, _)| v.to_bits() == 0)
            }
            BasicValueEnum::PointerValue(pv) => pv.is_null(),
            _ => false,
        };
        if is_zero {
            return Ok(arr_ty.const_zero().into());
        }

        // Non-zero compile-time constant: emit one LLVM `const_array`,
        // capped at SAFE_CONST_ARRAY_N. Above that cap LLVM's downstream
        // passes crash on the giant constant (verified SIGSEGV at
        // N=80_000+ on i64 / bool); the cap is conservative.
        const SAFE_CONST_ARRAY_N: u32 = 4096;
        if n <= SAFE_CONST_ARRAY_N {
            if let Some(agg) = match val {
                BasicValueEnum::IntValue(iv) if iv.is_const() => {
                    Some(iv.get_type().const_array(&vec![iv; n as usize]))
                }
                BasicValueEnum::FloatValue(fv) if fv.is_const() => {
                    Some(fv.get_type().const_array(&vec![fv; n as usize]))
                }
                BasicValueEnum::PointerValue(pv) if pv.is_const() => {
                    Some(pv.get_type().const_array(&vec![pv; n as usize]))
                }
                _ => None,
            } {
                return Ok(agg.into());
            }
        }

        // Above the cap or for runtime values: per-element `insertvalue`.
        // Also size-capped (each element adds an IR instruction). Beyond
        // the cap we error with a pointer to the workaround rather than
        // silently producing pathologically slow IR (or, worse, crashing
        // LLVM as the previous unbounded const_array path did).
        const SAFE_INSERT_N: u32 = 1024;
        if n > SAFE_INSERT_N {
            return Err(format!(
                "codegen: repeat literal `[v; {n}]` exceeds the safe size cap ({SAFE_INSERT_N}) \
                 for non-zero / runtime values. For large arrays, use a zero initializer \
                 (`[0; {n}]`, `[false; {n}]`, etc.) which compiles in O(1) regardless of size, \
                 then fill via a runtime for-loop: `let mut buf: Array[T, {n}] = [0; {n}]; \
                 for i in 0..{n} {{ buf[i] = v; }}`."
            ));
        }
        let mut agg = arr_ty.get_undef();
        for idx in 0..n {
            agg = self
                .builder
                .build_insert_value(agg, val, idx, "rep.elem")
                .unwrap()
                .into_array_value();
        }
        Ok(agg.into())
    }

    fn compile_index(
        &mut self,
        object: &Expr,
        index: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Range indexing (`v[a..b]`): produces a Slice[T] value regardless
        // of whether `v` is an Array, Vec, or Slice. The source element
        // type is inferred from the object variable.
        if let ExprKind::Range {
            start,
            end,
            inclusive,
        } = &index.kind
        {
            if let Some(elem_ty) = self.infer_elem_from_source(object) {
                return self.compile_range_slice(object, start, end, *inclusive, elem_ty);
            }
        }

        // Nested indexed read (`grid[i][j]` / `factors[v][0]`): the
        // outer container's element type is itself a Vec / Slice /
        // Array, so the inner Index expression yields an aggregate
        // value that the generic fall-through can't handle. Lower
        // the inner index to an element pointer via the existing
        // indexed-receiver machinery, mint a synth identifier so the
        // recursive dispatch sees a regular variable, and recurse.
        // Single-level nesting only — chained `a[i][j][k]` rejected
        // upstream by `compile_indexed_receiver_method`'s MR5 guard,
        // applied symmetrically here.
        if let ExprKind::Index {
            object: inner,
            index: inner_idx,
        } = &object.kind
        {
            return self.compile_nested_index_read(inner, inner_idx, index);
        }

        // Slice variable indexing: before the fast-path alloca lookup, check
        // whether the object is a slice variable. Slices use a 2-field
        // `{ptr, len}` representation and dispatch to a dedicated path.
        if let ExprKind::Identifier(name) = &object.kind {
            if self.slice_elem_types.contains_key(name.as_str()) {
                return self.compile_slice_index(name, index);
            }
        }

        // Map variable indexing: `m[k]` calls karac_map_get and panics on miss.
        // The key is hashed via the per-K hash_fn registered at Map construction;
        // it does NOT need to be an integer (unlike Array/Vec/Slice).
        if let ExprKind::Identifier(name) = &object.kind {
            if self.map_key_types.contains_key(name.as_str()) {
                return self.compile_map_index(name, index);
            }
        }

        // Vec variable indexing: route through `compile_vec_index` so both
        // owned and `ref Vec[T]` forms work. The downstream slot.ty branch
        // can't handle ref Vecs — for them slot.ty is `ptr`, not the Vec
        // struct type, so the StructType arm below would never fire.
        //
        // Bypass the Vec routing when the slot's LLVM type is `ArrayType` —
        // i.e. the `let a = [1, 2, 3]` shape where the typechecker recorded
        // "Vec" for the binding (synthesis-mode default) but
        // `compile_array_literal` produced an `[N x T]` aggregate that
        // bind_pattern alloca'd as ArrayType. Vec dispatch on an Array
        // alloca lays the `{ptr, i64, i64}` view over `[N x T]` bytes and
        // GEPs produce wild pointers (first i64 loaded as data ptr,
        // second i64 as len → out-of-bounds garbage writes / hangs at
        // runtime). Fall through to the Array path below in that case.
        if let ExprKind::Identifier(name) = &object.kind {
            if self.vec_elem_types.contains_key(name.as_str()) {
                let slot_is_array = self
                    .variables
                    .get(name.as_str())
                    .is_some_and(|s| matches!(s.ty, BasicTypeEnum::ArrayType(_)));
                if !slot_is_array {
                    return self.compile_vec_index(name, index);
                }
            }
        }

        let idx_val = self.compile_expr(index)?.into_int_value();
        let i64_t = self.context.i64_type();

        // Get a pointer to the array storage.
        // Fast path: if the object is a local variable, use its alloca directly.
        let (arr_ptr, arr_ty) = if let ExprKind::Identifier(name) = &object.kind {
            if let Some(slot) = self.variables.get(name.as_str()).copied() {
                (slot.ptr, slot.ty)
            } else {
                return Err(format!("Undefined variable '{}' in index expression", name));
            }
        } else {
            // Arbitrary expression: compile, store to temp alloca, use that pointer.
            let arr_val = self.compile_expr(object)?;
            let fn_val = self.current_fn.unwrap();
            let tmp = self.create_entry_alloca(fn_val, "arr.tmp", arr_val.get_type());
            self.builder.build_store(tmp, arr_val).unwrap();
            (tmp, arr_val.get_type())
        };

        // Bounds check: panic if index >= array_length.
        if let BasicTypeEnum::ArrayType(at) = arr_ty {
            let len = i64_t.const_int(at.len() as u64, false);
            let fn_val = self.current_fn.unwrap();
            let oob_bb = self.context.append_basic_block(fn_val, "idx.oob");
            let ok_bb = self.context.append_basic_block(fn_val, "idx.ok");
            let cmp = self
                .builder
                .build_int_compare(inkwell::IntPredicate::UGE, idx_val, len, "bounds")
                .unwrap();
            self.builder
                .build_conditional_branch(cmp, oob_bb, ok_bb)
                .unwrap();

            // OOB path: call abort or unreachable.
            self.builder.position_at_end(oob_bb);
            self.emit_panic("array index out of bounds");
            self.builder.build_unreachable().unwrap();

            // OK path: GEP + load.
            self.builder.position_at_end(ok_bb);
            let zero = i64_t.const_int(0, false);
            let elem_ptr = unsafe {
                self.builder
                    .build_gep(arr_ty, arr_ptr, &[zero, idx_val], "arr.elem.ptr")
                    .unwrap()
            };
            let elem_ty = at.get_element_type();
            let val = self
                .builder
                .build_load(elem_ty, elem_ptr, "arr.elem")
                .unwrap();
            Ok(val)
        } else {
            // Vec, Slice, Map already routed through their dedicated paths
            // above. Anything still reaching here is genuinely not indexable.
            Err("Index operator applied to non-array type".to_string())
        }
    }

    /// Index into a `Vec[T]` variable: `v[i]`. Handles both owned Vec values
    /// (slot is the `{ptr,len,cap}` struct) and `ref Vec[T]` parameters
    /// (slot is a `ptr` to the caller's struct) by routing the struct-base
    /// pointer through `get_data_ptr`. Loads `len`, bounds-checks, then
    /// GEPs `data[i]` and loads the element.
    fn compile_vec_index(
        &mut self,
        name: &str,
        index: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.vec_elem_type_for_var(name);

        let vec_ptr = self
            .get_data_ptr(name)
            .ok_or_else(|| format!("Undefined Vec variable '{}' in index expression", name))?;
        // Source-level elision: if the index is a bare identifier whose
        // bounds are proven by a dominating loop guard (recorded in
        // `asserted_index_bounds`), drop the matching half(s) of the
        // bounds check. Captured here BEFORE compiling the index so we
        // don't pay for the lookup when it can't fire (compound indices,
        // method-call indices, etc. immediately default to no elision).
        let (lower_proven, upper_proven) = self.index_bounds_already_proven(index, name);

        let idx_val = self.compile_expr(index)?.into_int_value();

        let data_pp = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 0, "v.data.ptr")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "v.data")
            .unwrap()
            .into_pointer_value();

        // Emit whichever halves of the bounds check the source-level
        // analysis didn't prove. The runtime panic path is reachable iff
        // some unproven half fails; both halves proven → no runtime
        // check at all (status quo for `unsafe { v.get_unchecked(i) }`).
        self.emit_split_bounds_check("vidx", idx_val, vec_ty, vec_ptr, lower_proven, upper_proven);

        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[idx_val], "v.elem.ptr")
                .unwrap()
        };
        let val = self
            .builder
            .build_load(elem_ty, elem_ptr, "v.elem")
            .unwrap();
        Ok(val)
    }

    /// Decide whether the dominating loop guard already proves either half
    /// of a `vec_var[idx]` safety check. Returns `(lower_proven, upper_proven)`:
    /// `lower_proven` ⇒ `idx >= 0` known; the negative-idx half can be
    /// dropped. `upper_proven` ⇒ `idx < vec_var.len()` known; the
    /// out-of-range half can be dropped.
    ///
    /// Only fires for bare-identifier indices (`v[i]`, never `v[i + 1]`).
    /// The kata's `chars[lo]` / `chars[hi]` shape passes; compound forms
    /// fall through to the full runtime check. Tightening to handle
    /// `v[i ± k]` for small known k is a follow-up; many real workloads
    /// don't need it (e.g. iterator-driven loops use bare-identifier
    /// indices), and the conservative default just means "no elision",
    /// not "wrong".
    fn index_bounds_already_proven(&self, index: &Expr, vec_var: &str) -> (bool, bool) {
        let idx_name = match &index.kind {
            ExprKind::Identifier(name) => name.as_str(),
            _ => return (false, false),
        };
        let mut lower = false;
        let mut upper = false;
        for fact in &self.asserted_index_bounds {
            match fact {
                AssertedIndexBound::LowerBound { idx_var } if idx_var == idx_name => {
                    lower = true;
                }
                AssertedIndexBound::UpperBound {
                    idx_var,
                    vec_var: bound_vec,
                } if idx_var == idx_name && bound_vec == vec_var => {
                    upper = true;
                }
                _ => {}
            }
        }
        (lower, upper)
    }

    /// Emit the runtime bounds check for `vec_ptr[idx]`, dropping
    /// whichever half(s) the caller's `lower_proven` / `upper_proven`
    /// flags say are already established. The remaining branches still
    /// route OOB cases through `emit_panic("vec index out of bounds")`
    /// for safety; only the redundant compares are elided.
    ///
    /// When both halves are proven, this emits no bounds-check code at
    /// all — the caller's GEP+load runs straight through, matching the
    /// shape of `Vec.get_unchecked` for safe code that the source-level
    /// guard already justifies.
    fn emit_split_bounds_check(
        &mut self,
        label_prefix: &str,
        idx_val: inkwell::values::IntValue<'ctx>,
        vec_ty: StructType<'ctx>,
        vec_ptr: PointerValue<'ctx>,
        lower_proven: bool,
        upper_proven: bool,
    ) {
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();

        // No check at all — both halves are pre-proven. Saves the load of
        // len and any branch / panic-block emission.
        if lower_proven && upper_proven {
            return;
        }

        // Neither half proven — fall back to the original single unsigned
        // bounds check. `icmp uge idx, len` catches both negative-idx (which
        // wraps to a huge unsigned value > any reasonable len) and
        // idx >= len in one compare + branch. Splitting into signed lower +
        // signed upper here would add an instruction without any elision
        // benefit (regression measured on kata-88's `nums1[k]` indexing,
        // where neither bound is asserted by the source guards).
        if !lower_proven && !upper_proven {
            let len_ptr = self
                .builder
                .build_struct_gep(vec_ty, vec_ptr, 1, "v.len.ptr")
                .unwrap();
            let len = self
                .builder
                .build_load(i64_t, len_ptr, "v.len")
                .unwrap()
                .into_int_value();
            let oob_bb = self
                .context
                .append_basic_block(fn_val, &format!("{label_prefix}.oob"));
            let ok_bb = self
                .context
                .append_basic_block(fn_val, &format!("{label_prefix}.ok"));
            let cmp = self
                .builder
                .build_int_compare(inkwell::IntPredicate::UGE, idx_val, len, "bounds")
                .unwrap();
            self.builder
                .build_conditional_branch(cmp, oob_bb, ok_bb)
                .unwrap();
            self.builder.position_at_end(oob_bb);
            self.emit_panic("vec index out of bounds");
            self.builder.build_unreachable().unwrap();
            self.builder.position_at_end(ok_bb);
            return;
        }

        // Lower-bound half: `idx < 0`. Skipped when the guard proved
        // `idx >= 0`; the load of `len` below is then loop-invariant
        // and LLVM will likely hoist it if both halves are emitted but
        // only the upper one is reached on the hot path.
        if !lower_proven {
            let oob_bb = self
                .context
                .append_basic_block(fn_val, &format!("{label_prefix}.oob.neg"));
            let ok_bb = self
                .context
                .append_basic_block(fn_val, &format!("{label_prefix}.lower.ok"));
            let neg = self
                .builder
                .build_int_compare(
                    inkwell::IntPredicate::SLT,
                    idx_val,
                    i64_t.const_zero(),
                    "bounds.neg",
                )
                .unwrap();
            self.builder
                .build_conditional_branch(neg, oob_bb, ok_bb)
                .unwrap();
            self.builder.position_at_end(oob_bb);
            self.emit_panic("vec index out of bounds");
            self.builder.build_unreachable().unwrap();
            self.builder.position_at_end(ok_bb);
        }

        // Upper-bound half: `idx >= len`. Skipped when the guard proved
        // `idx < vec_var.len()`. The signed `sge` predicate matches the
        // source-level signed loop guard's `slt` — LLVM's instcombine
        // folds the per-iteration redundant compare with the loop guard's
        // back-edge cmp when both have the same operands and predicate
        // family, which is the structural fix the `llvm.assume` spike
        // failed to trigger.
        if !upper_proven {
            let len_ptr = self
                .builder
                .build_struct_gep(vec_ty, vec_ptr, 1, "v.len.ptr")
                .unwrap();
            let len = self
                .builder
                .build_load(i64_t, len_ptr, "v.len")
                .unwrap()
                .into_int_value();
            let oob_bb = self
                .context
                .append_basic_block(fn_val, &format!("{label_prefix}.oob.upper"));
            let ok_bb = self
                .context
                .append_basic_block(fn_val, &format!("{label_prefix}.upper.ok"));
            let upper = if lower_proven {
                // Guard proved `idx >= 0`, so `idx u>= len` is equivalent
                // to `idx s>= len`. Use the signed form to match the
                // loop guard's predicate family for CSE.
                self.builder
                    .build_int_compare(inkwell::IntPredicate::SGE, idx_val, len, "bounds.upper")
                    .unwrap()
            } else {
                // Unreachable per the early-return above, but keep the
                // arm sound in case the caller's logic changes.
                self.builder
                    .build_int_compare(inkwell::IntPredicate::UGE, idx_val, len, "bounds")
                    .unwrap()
            };
            self.builder
                .build_conditional_branch(upper, oob_bb, ok_bb)
                .unwrap();
            self.builder.position_at_end(oob_bb);
            self.emit_panic("vec index out of bounds");
            self.builder.build_unreachable().unwrap();
            self.builder.position_at_end(ok_bb);
        }
    }

    fn compile_vec_index_store(
        &mut self,
        var_name: &str,
        index: &Expr,
        val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.vec_elem_type_for_var(var_name);
        let vec_ptr = self
            .get_data_ptr(var_name)
            .ok_or_else(|| format!("Undefined Vec variable '{}' in index store", var_name))?;
        let (lower_proven, upper_proven) = self.index_bounds_already_proven(index, var_name);
        let idx_val = self.compile_expr(index)?.into_int_value();

        let data_pp = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 0, "v.st.data.ptr")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "v.st.data")
            .unwrap()
            .into_pointer_value();

        self.emit_split_bounds_check("v.st", idx_val, vec_ty, vec_ptr, lower_proven, upper_proven);
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[idx_val], "v.st.elem.ptr")
                .unwrap()
        };
        self.builder.build_store(elem_ptr, val).unwrap();
        Ok(())
    }

    fn compile_slice_index_store(
        &mut self,
        var_name: &str,
        index: &Expr,
        val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();
        let elem_ty = *self.slice_elem_types.get(var_name).unwrap();
        let slice_ptr = self.get_data_ptr(var_name).unwrap();
        let (lower_proven, upper_proven) = self.index_bounds_already_proven(index, var_name);
        let idx_val = self.compile_expr(index)?.into_int_value();

        let data_pp = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 0, "s.st.data.pp")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "s.st.data")
            .unwrap()
            .into_pointer_value();

        // Slice layout `{ptr, i64}` has len at field 1, same offset as
        // Vec's `{ptr, i64, i64}` — the helper's struct-gep only touches
        // field 1, so passing slice_ty is sound. The OOB diagnostic is
        // shared with Vec (`vec index out of bounds`) per the kata-5
        // precedent; users routing through `Slice.get` get the typed
        // diagnostic via the safe path, this is the unsafe-form panic.
        self.emit_split_bounds_check(
            "s.st",
            idx_val,
            slice_ty,
            slice_ptr,
            lower_proven,
            upper_proven,
        );
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[idx_val], "s.st.elem.ptr")
                .unwrap()
        };
        self.builder.build_store(elem_ptr, val).unwrap();
        Ok(())
    }

    fn compile_slice_index(
        &mut self,
        var_name: &str,
        index: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();
        let elem_ty = *self.slice_elem_types.get(var_name).unwrap();
        let slice_ptr = self.get_data_ptr(var_name).unwrap();
        // Source-level elision: bare-identifier index whose bounds are
        // proven by an enclosing while-guard / short-circuit `and` skips
        // the matching half of the runtime check. Mirrors the Vec read
        // path. Captured before compiling the index so compound-index
        // shapes (`v[i + 1]`) drop straight to (false, false) — the
        // index-name match in `index_bounds_already_proven` requires a
        // bare `Identifier` source-level node.
        let (lower_proven, upper_proven) = self.index_bounds_already_proven(index, var_name);
        let idx_val = self.compile_expr(index)?.into_int_value();

        let data_pp = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 0, "s.data.pp")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "s.data")
            .unwrap()
            .into_pointer_value();

        self.emit_split_bounds_check(
            "sidx",
            idx_val,
            slice_ty,
            slice_ptr,
            lower_proven,
            upper_proven,
        );
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[idx_val], "s.elem.ptr")
                .unwrap()
        };
        let val = self
            .builder
            .build_load(elem_ty, elem_ptr, "s.elem")
            .unwrap();
        Ok(val)
    }

    fn compile_index_store(
        &mut self,
        object: &Expr,
        index: &Expr,
        val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        // Slice[T] / mut Slice[T] element store: the slice is a `{ptr, i64}`
        // value; the index path GEPs through the stored data pointer. The
        // ownership checker is responsible for rejecting stores through a
        // read-only Slice[T] — codegen treats the write path uniformly.
        if let ExprKind::Identifier(name) = &object.kind {
            if self.slice_elem_types.contains_key(name.as_str()) {
                return self.compile_slice_index_store(name, index, val);
            }
        }

        // Map[K, V] element store: `m[k] = v` lowers to karac_map_insert_old
        // discarding the previous-value out-slot. Fresh-insert and overwrite
        // are both handled by the same runtime call.
        if let ExprKind::Identifier(name) = &object.kind {
            if self.map_key_types.contains_key(name.as_str()) {
                return self.compile_map_index_store(name, index, val);
            }
        }

        // Vec[T] element store: bounds-check against `len` (not `cap`) and
        // GEP `data[i]`. Mirrors the read-path in `compile_vec_index`.
        //
        // Same ArrayType-slot guard as compile_index: when the typechecker
        // registered "Vec" for a binding (synthesis-mode bare ArrayLiteral)
        // but the alloca is sized as `[N x T]`, the Vec dispatch produces
        // wild GEPs. Fall through to the Array path below.
        if let ExprKind::Identifier(name) = &object.kind {
            if self.vec_elem_types.contains_key(name.as_str()) {
                let slot_is_array = self
                    .variables
                    .get(name.as_str())
                    .is_some_and(|s| matches!(s.ty, BasicTypeEnum::ArrayType(_)));
                if !slot_is_array {
                    return self.compile_vec_index_store(name, index, val);
                }
            }
        }

        let idx_val = self.compile_expr(index)?.into_int_value();
        let i64_t = self.context.i64_type();

        let (arr_ptr, arr_ty) = if let ExprKind::Identifier(name) = &object.kind {
            if let Some(slot) = self.variables.get(name.as_str()).copied() {
                (slot.ptr, slot.ty)
            } else {
                return Err(format!("Undefined variable '{}' in index store", name));
            }
        } else {
            return Err("Index assignment target must be a variable".to_string());
        };

        if let BasicTypeEnum::ArrayType(at) = arr_ty {
            let len = i64_t.const_int(at.len() as u64, false);
            let fn_val = self.current_fn.unwrap();
            let oob_bb = self.context.append_basic_block(fn_val, "idx_s.oob");
            let ok_bb = self.context.append_basic_block(fn_val, "idx_s.ok");
            let cmp = self
                .builder
                .build_int_compare(inkwell::IntPredicate::UGE, idx_val, len, "bounds")
                .unwrap();
            self.builder
                .build_conditional_branch(cmp, oob_bb, ok_bb)
                .unwrap();

            self.builder.position_at_end(oob_bb);
            self.emit_panic("array index out of bounds");
            self.builder.build_unreachable().unwrap();

            self.builder.position_at_end(ok_bb);
            let zero = i64_t.const_int(0, false);
            let elem_ptr = unsafe {
                self.builder
                    .build_gep(arr_ty, arr_ptr, &[zero, idx_val], "arr.store.ptr")
                    .unwrap()
            };
            self.builder.build_store(elem_ptr, val).unwrap();
            Ok(())
        } else {
            Err("Index store on non-array type".to_string())
        }
    }

    fn compile_tuple(&mut self, elems: &[Expr]) -> Result<BasicValueEnum<'ctx>, String> {
        let vals: Vec<BasicValueEnum<'ctx>> = elems
            .iter()
            .map(|e| self.compile_expr(e))
            .collect::<Result<_, _>>()?;
        let types: Vec<BasicTypeEnum<'ctx>> = vals.iter().map(|v| v.get_type()).collect();
        let st = self.context.struct_type(&types, false);
        let mut agg = st.get_undef();
        for (idx, val) in vals.iter().enumerate() {
            agg = self
                .builder
                .build_insert_value(agg, *val, idx as u32, "elem")
                .unwrap()
                .into_struct_value();
        }
        Ok(agg.into())
    }

    fn compile_field_access(
        &mut self,
        object: &Expr,
        field: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Primitive-type associated constants — `i64.MAX` /
        // `f64.INFINITY` / `usize.MAX` etc. parse as
        // `FieldAccess(Identifier("i64"), "MAX")`. Intercept before the
        // normal field-access path so the bare primitive identifier
        // doesn't fall through to a generic compile_expr that would
        // either panic or produce wrong codegen. Mirrors the typechecker
        // and interpreter early-intercepts for the same expression
        // shape.
        if let ExprKind::Identifier(name) = &object.kind {
            if let Some(cv) = crate::prelude::lookup_primitive_const(name, field) {
                return Ok(self.compile_primitive_const(cv));
            }
        }
        // Indexed-shared-struct receiver: `nodes[i].field` where
        // `nodes: Vec[Shared(N)]`. Mirror of `compile_field_store`'s
        // Index branch — load the heap pointer at `nodes[i]`, GEP into
        // the heap struct's field, return the typed load. Without this,
        // the access falls through to the generic Struct-value extract
        // path which returns `i64 0` for any shared-struct receiver.
        if let ExprKind::Index {
            object: inner,
            index,
        } = &object.kind
        {
            if let ExprKind::Identifier(outer_name) = &inner.kind {
                if let Some(elem_te) = self.var_elem_type_exprs.get(outer_name.as_str()).cloned() {
                    if let TypeKind::Path(path) = &elem_te.kind {
                        if let Some(seg) = path.segments.first() {
                            if let Some(info) = self.shared_types.get(seg.as_str()).cloned() {
                                if !info.is_enum {
                                    let outer_name = outer_name.clone();
                                    let (elem_ptr, _) =
                                        if self.vec_elem_types.contains_key(outer_name.as_str()) {
                                            self.lower_indexed_elem_ptr_vec(&outer_name, index)?
                                        } else if self
                                            .slice_elem_types
                                            .contains_key(outer_name.as_str())
                                        {
                                            self.lower_indexed_elem_ptr_slice(&outer_name, index)?
                                        } else {
                                            let zero = self.context.i64_type().const_zero();
                                            return Ok(zero.into());
                                        };
                                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                                    let heap_ptr = self
                                        .builder
                                        .build_load(ptr_ty, elem_ptr, "idx.shared.read")
                                        .unwrap()
                                        .into_pointer_value();
                                    if let Some(names) = self.struct_field_names.get(seg) {
                                        if let Some(idx) = names.iter().position(|n| n == field) {
                                            let field_ptr = self
                                                .builder
                                                .build_struct_gep(
                                                    info.heap_type,
                                                    heap_ptr,
                                                    (idx + 1) as u32,
                                                    &format!("sh_idx_{}", field),
                                                )
                                                .unwrap();
                                            let field_ty = info
                                                .heap_type
                                                .get_field_type_at_index((idx + 1) as u32)
                                                .unwrap();
                                            return Ok(self
                                                .builder
                                                .build_load(field_ty, field_ptr, field)
                                                .unwrap());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Shared type: object compiles to a pointer; field access via GEP.
        if let Some((type_name, info)) = self.shared_type_for_expr(object) {
            if !info.is_enum {
                let ptr = self.compile_expr(object)?.into_pointer_value();
                if let Some(names) = self.struct_field_names.get(&type_name) {
                    if let Some(idx) = names.iter().position(|n| n == field) {
                        // Fields start at heap index 1 (index 0 is refcount).
                        let field_ptr = self
                            .builder
                            .build_struct_gep(
                                info.heap_type,
                                ptr,
                                (idx + 1) as u32,
                                &format!("sh_{}", field),
                            )
                            .unwrap();
                        let field_ty = info
                            .heap_type
                            .get_field_type_at_index((idx + 1) as u32)
                            .unwrap();
                        return Ok(self.builder.build_load(field_ty, field_ptr, field).unwrap());
                    }
                }
            }
        }

        let obj_val = self.compile_expr(object)?;
        if let BasicValueEnum::StructValue(sv) = obj_val {
            // Look up field index from struct type name in object's identifier
            let field_idx = self.field_index_for(object, field);
            if let Some(idx) = field_idx {
                return Ok(self.builder.build_extract_value(sv, idx, field).unwrap());
            }
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_field_store(
        &mut self,
        object: &Expr,
        field: &str,
        new_val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        // Indexed-shared-struct receiver: `nodes[i].field = X` where
        // `nodes: Vec[Shared(N)]`. Load the heap pointer at `nodes[i]`
        // (the element slot stores the RC pointer cast to its LLVM
        // type), then GEP into the heap struct and store. Without this
        // branch the assignment silently falls through to the no-op
        // `Ok(())` exit at the function tail — the field write compiles
        // clean but does not persist, so a subsequent `nodes[i].field`
        // read returns the stale value.
        if let ExprKind::Index {
            object: inner,
            index,
        } = &object.kind
        {
            if let ExprKind::Identifier(outer_name) = &inner.kind {
                if let Some(elem_te) = self.var_elem_type_exprs.get(outer_name.as_str()).cloned() {
                    if let TypeKind::Path(path) = &elem_te.kind {
                        if let Some(seg) = path.segments.first() {
                            if let Some(info) = self.shared_types.get(seg.as_str()).cloned() {
                                if !info.is_enum {
                                    let outer_name = outer_name.clone();
                                    let (elem_ptr, _) =
                                        if self.vec_elem_types.contains_key(outer_name.as_str()) {
                                            self.lower_indexed_elem_ptr_vec(&outer_name, index)?
                                        } else if self
                                            .slice_elem_types
                                            .contains_key(outer_name.as_str())
                                        {
                                            self.lower_indexed_elem_ptr_slice(&outer_name, index)?
                                        } else {
                                            return Ok(());
                                        };
                                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                                    let heap_ptr = self
                                        .builder
                                        .build_load(ptr_ty, elem_ptr, "idx.shared.ptr")
                                        .unwrap()
                                        .into_pointer_value();
                                    if let Some(names) = self.struct_field_names.get(seg) {
                                        if let Some(idx) = names.iter().position(|n| n == field) {
                                            let field_ptr = self
                                                .builder
                                                .build_struct_gep(
                                                    info.heap_type,
                                                    heap_ptr,
                                                    (idx + 1) as u32,
                                                    &format!("sh_idx_{}_ptr", field),
                                                )
                                                .unwrap();
                                            self.builder.build_store(field_ptr, new_val).unwrap();
                                        }
                                    }
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
        }

        // `self.field = …` parses as `FieldAccess { object: SelfValue, … }`,
        // and `self` is bound as a regular local named "self" — same lookup
        // path as a plain Identifier. Treat both shapes uniformly so
        // ref-self method bodies can mutate through the receiver.
        let var_name_owned: Option<String> = match &object.kind {
            ExprKind::Identifier(n) => Some(n.clone()),
            ExprKind::SelfValue => Some("self".to_string()),
            _ => None,
        };
        if let Some(var_name) = var_name_owned.as_deref() {
            // Shared type: store directly into the heap object via GEP.
            if let Some(type_name) = self.var_type_names.get(var_name).cloned() {
                if let Some(info) = self.shared_types.get(&type_name).cloned() {
                    if !info.is_enum {
                        if let Some(slot) = self.variables.get(var_name).copied() {
                            let ptr = self
                                .builder
                                .build_load(
                                    self.context.ptr_type(AddressSpace::default()),
                                    slot.ptr,
                                    var_name,
                                )
                                .unwrap()
                                .into_pointer_value();
                            if let Some(names) = self.struct_field_names.get(&type_name) {
                                if let Some(idx) = names.iter().position(|n| n == field) {
                                    let field_ptr = self
                                        .builder
                                        .build_struct_gep(
                                            info.heap_type,
                                            ptr,
                                            (idx + 1) as u32,
                                            &format!("sh_{}_ptr", field),
                                        )
                                        .unwrap();
                                    self.builder.build_store(field_ptr, new_val).unwrap();
                                }
                            }
                        }
                        return Ok(());
                    }
                }
            }

            // Ref / mut-ref struct param: write through the pointer so the
            // caller's storage observes the update. The owned-param path
            // below would mutate a local copy of the struct value, so the
            // caller never sees the change — the `mut ref self` mutation
            // bug fixed in this slice. `get_data_ptr` returns the alloca
            // for owned bindings and the dereferenced pointer for ref
            // params, so we use it uniformly when GEP'ing into a struct.
            if let Some(&BasicTypeEnum::StructType(struct_ty)) = self.ref_params.get(var_name) {
                if let Some(idx) = self.field_index_for(object, field) {
                    if let Some(ptr) = self.get_data_ptr(var_name) {
                        let field_ptr = self
                            .builder
                            .build_struct_gep(struct_ty, ptr, idx, &format!("ref_{}_ptr", field))
                            .unwrap();
                        self.builder.build_store(field_ptr, new_val).unwrap();
                        return Ok(());
                    }
                }
            }

            if let Some(slot) = self.variables.get(var_name).copied() {
                let obj_val = self
                    .builder
                    .build_load(slot.ty, slot.ptr, var_name)
                    .unwrap();
                if let BasicValueEnum::StructValue(sv) = obj_val {
                    let field_idx = self.field_index_for(object, field);
                    if let Some(idx) = field_idx {
                        let updated = self
                            .builder
                            .build_insert_value(sv, new_val, idx, field)
                            .unwrap();
                        self.builder.build_store(slot.ptr, updated).unwrap();
                    }
                }
            }
        }
        Ok(())
    }

    fn compile_tuple_index(
        &mut self,
        object: &Expr,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let obj_val = self.compile_expr(object)?;
        if let BasicValueEnum::StructValue(sv) = obj_val {
            return Ok(self
                .builder
                .build_extract_value(sv, index as u32, "tidx")
                .unwrap());
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn field_index_for(&self, object: &Expr, field: &str) -> Option<u32> {
        // Try to resolve by walking the object expression to its
        // user-type name, then looking up `field` in that struct's
        // field registry. Chained `o.inner.name` requires walking the
        // inner FieldAccess to recover `o.inner`'s declared type from
        // `struct_field_type_names`. See `type_name_of_expr`.
        if let Some(type_name) = self.type_name_of_expr(object) {
            if let Some(names) = self.struct_field_names.get(type_name.as_str()) {
                if let Some(idx) = names.iter().position(|n| n == field) {
                    return Some(idx as u32);
                }
            }
        }
        // Fall back: numeric index for tuple fields like `.0`, `.1`
        field.parse::<u32>().ok()
    }

    /// Resolve the user-type name of an arbitrary expression by walking
    /// `Identifier` / `SelfValue` / `FieldAccess` chains. Returns
    /// `None` for primitive-typed expressions, calls whose return type
    /// isn't a known struct, or any shape outside this trio. Companion
    /// to `type_name_of` (which only handles direct identifiers and
    /// struct literals).
    fn type_name_of_expr(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(n) => self.var_type_names.get(n.as_str()).cloned(),
            ExprKind::SelfValue => self.var_type_names.get("self").cloned(),
            ExprKind::StructLiteral { path, .. } => path.last().cloned(),
            ExprKind::FieldAccess { object, field } => {
                let obj_ty = self.type_name_of_expr(object)?;
                let field_names = self.struct_field_names.get(obj_ty.as_str())?;
                let idx = field_names.iter().position(|n| n == field)?;
                let field_ty_names = self.struct_field_type_names.get(obj_ty.as_str())?;
                field_ty_names.get(idx).and_then(|n| n.clone())
            }
            _ => None,
        }
    }

    /// Return the Kāra type name for a compiled expression, if known.
    fn type_name_of(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(n) => self.var_type_names.get(n.as_str()).cloned(),
            ExprKind::StructLiteral { path, .. } => path.last().cloned(),
            _ => None,
        }
    }

    // ── Cast ──────────────────────────────────────────────────────

    fn compile_cast(
        &self,
        val: BasicValueEnum<'ctx>,
        target: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match (val, target) {
            (BasicValueEnum::IntValue(iv), BasicTypeEnum::IntType(tt)) => {
                let result = self.builder.build_int_cast(iv, tt, "cast").unwrap();
                Ok(result.into())
            }
            (BasicValueEnum::IntValue(iv), BasicTypeEnum::FloatType(ft)) => {
                let result = self
                    .builder
                    .build_signed_int_to_float(iv, ft, "cast")
                    .unwrap();
                Ok(result.into())
            }
            (BasicValueEnum::FloatValue(fv), BasicTypeEnum::IntType(it)) => {
                let result = self
                    .builder
                    .build_float_to_signed_int(fv, it, "cast")
                    .unwrap();
                Ok(result.into())
            }
            (BasicValueEnum::FloatValue(fv), BasicTypeEnum::FloatType(ft)) => {
                let result = self.builder.build_float_cast(fv, ft, "cast").unwrap();
                Ok(result.into())
            }
            _ => Ok(val),
        }
    }

    // ── Binary / unary operators ──────────────────────────────────

    /// Emit short-circuit `and` / `or` per documented design intent
    /// (roadmap.md:425, 429): the RHS is only compiled into a basic
    /// block reachable when the LHS doesn't already determine the
    /// result. Without this, the RHS would emit unconditionally and
    /// its side-effects (panicking index, dropped fn call) would fire
    /// even when short-circuited — same shape as the interpreter's
    /// eager-eval bug fixed in lockstep.
    fn compile_short_circuit(
        &mut self,
        op: &BinOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let lhs_val = self.compile_expr(left)?.into_int_value();
        let lhs_end_bb = self.builder.get_insert_block().unwrap();

        let rhs_bb = self.context.append_basic_block(fn_val, "sc.rhs");
        let merge_bb = self.context.append_basic_block(fn_val, "sc.merge");

        // `and`: lhs true → eval rhs; lhs false → short-circuit to false.
        // `or`:  lhs true → short-circuit to true; lhs false → eval rhs.
        let (true_dest, false_dest) = match op {
            BinOp::And => (rhs_bb, merge_bb),
            BinOp::Or => (merge_bb, rhs_bb),
            _ => unreachable!("compile_short_circuit only handles And/Or"),
        };
        self.builder
            .build_conditional_branch(lhs_val, true_dest, false_dest)
            .unwrap();

        // Bounds-check-elision propagation: when the RHS of `lhs and rhs`
        // fires, we've branch-proved that lhs holds. Any index-safety fact
        // asserted by lhs is in scope for rhs's compilation. This is how
        // the kata's `while lo >= 0 and hi < n and chars[lo] == chars[hi]`
        // pattern lets the indexing in the third conjunct skip its
        // bounds check — `lo >= 0` and `hi < n` are conjuncts evaluated
        // first under short-circuit, so by the time chars[lo] / chars[hi]
        // lower (in compile_vec_index), the facts are on the stack.
        let pushed = if matches!(op, BinOp::And) {
            let facts = self.collect_asserted_bounds_from_guard(left);
            let n = facts.len();
            self.asserted_index_bounds.extend(facts);
            n
        } else {
            0
        };

        self.builder.position_at_end(rhs_bb);
        let rhs_val = self.compile_expr(right)?.into_int_value();
        let rhs_end_bb = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge_bb).unwrap();

        // Pop the temporarily-asserted facts so the merge / surrounding
        // scope sees only its own bounds. Compile_while's body-entry push
        // re-establishes them for body code on the long-lived path.
        for _ in 0..pushed {
            self.asserted_index_bounds.pop();
        }

        self.builder.position_at_end(merge_bb);
        let bool_ty = self.context.bool_type();
        let short_const = match op {
            BinOp::And => bool_ty.const_int(0, false),
            BinOp::Or => bool_ty.const_int(1, false),
            _ => unreachable!(),
        };
        let phi = self.builder.build_phi(bool_ty, "sc.result").unwrap();
        phi.add_incoming(&[(&short_const, lhs_end_bb), (&rhs_val, rhs_end_bb)]);
        Ok(phi.as_basic_value())
    }

    fn compile_binop(
        &mut self,
        op: &BinOp,
        lhs: BasicValueEnum<'ctx>,
        rhs: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Struct path: strings or user-defined structs.
        if lhs.is_struct_value() && rhs.is_struct_value() {
            let ls = lhs.into_struct_value();
            let rs = rhs.into_struct_value();
            let field_count = ls.get_type().count_fields();
            let vec_fields = self.vec_struct_type().count_fields();
            // String/Vec layout ({ ptr, i64, i64 }) — 3 fields.
            if field_count == vec_fields {
                return self.compile_string_binop(op, ls, rs);
            }
            // User struct equality: field-by-field comparison.
            if matches!(op, BinOp::Eq | BinOp::NotEq) {
                return self.compile_struct_eq(op, ls, rs);
            }
            return Err(format!("Unsupported struct binary op: {:?}", op));
        }

        // Float path
        if lhs.is_float_value() || rhs.is_float_value() {
            let lf = self.to_float(lhs)?;
            let rf = self.to_float(rhs)?;
            return self.compile_float_binop(op, lf, rf);
        }

        let lv = lhs.into_int_value();
        let rv = rhs.into_int_value();
        let result = match op {
            BinOp::Add => self.builder.build_int_nsw_add(lv, rv, "add").unwrap(),
            BinOp::Sub => self.builder.build_int_nsw_sub(lv, rv, "sub").unwrap(),
            BinOp::Mul => self.builder.build_int_nsw_mul(lv, rv, "mul").unwrap(),
            BinOp::Div => self.builder.build_int_signed_div(lv, rv, "div").unwrap(),
            BinOp::Mod => self.builder.build_int_signed_rem(lv, rv, "mod").unwrap(),
            BinOp::Eq => self
                .builder
                .build_int_compare(IntPredicate::EQ, lv, rv, "eq")
                .unwrap(),
            BinOp::NotEq => self
                .builder
                .build_int_compare(IntPredicate::NE, lv, rv, "ne")
                .unwrap(),
            BinOp::Lt => self
                .builder
                .build_int_compare(IntPredicate::SLT, lv, rv, "lt")
                .unwrap(),
            BinOp::LtEq => self
                .builder
                .build_int_compare(IntPredicate::SLE, lv, rv, "le")
                .unwrap(),
            BinOp::Gt => self
                .builder
                .build_int_compare(IntPredicate::SGT, lv, rv, "gt")
                .unwrap(),
            BinOp::GtEq => self
                .builder
                .build_int_compare(IntPredicate::SGE, lv, rv, "ge")
                .unwrap(),
            BinOp::And => self.builder.build_and(lv, rv, "and").unwrap(),
            BinOp::Or => self.builder.build_or(lv, rv, "or").unwrap(),
            BinOp::BitAnd => self.builder.build_and(lv, rv, "bitand").unwrap(),
            BinOp::BitOr => self.builder.build_or(lv, rv, "bitor").unwrap(),
            BinOp::BitXor => self.builder.build_xor(lv, rv, "bitxor").unwrap(),
            BinOp::Shl => self.builder.build_left_shift(lv, rv, "shl").unwrap(),
            BinOp::Shr => self.builder.build_right_shift(lv, rv, true, "shr").unwrap(),
            _ => return Err(format!("Unsupported binary op: {:?}", op)),
        };
        Ok(result.into())
    }

    fn compile_struct_eq(
        &mut self,
        op: &BinOp,
        lhs: inkwell::values::StructValue<'ctx>,
        rhs: inkwell::values::StructValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let field_count = lhs.get_type().count_fields();
        let bool_t = self.context.bool_type();
        let mut result = bool_t.const_int(1, false); // start true (all equal)

        for i in 0..field_count {
            let l_field = self
                .builder
                .build_extract_value(lhs, i, &format!("l.f{}", i))
                .unwrap();
            let r_field = self
                .builder
                .build_extract_value(rhs, i, &format!("r.f{}", i))
                .unwrap();
            // Recursively compare the field.
            let field_eq = self.compile_binop(&BinOp::Eq, l_field, r_field)?;
            result = self
                .builder
                .build_and(result, field_eq.into_int_value(), &format!("eq.f{}", i))
                .unwrap();
        }

        if matches!(op, BinOp::NotEq) {
            Ok(self.builder.build_not(result, "struct_ne").unwrap().into())
        } else {
            Ok(result.into())
        }
    }

    fn compile_string_binop(
        &self,
        op: &BinOp,
        lhs: inkwell::values::StructValue<'ctx>,
        rhs: inkwell::values::StructValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i32_t = self.context.i32_type();

        // Extract ptr and len from each string struct.
        let l_ptr = self
            .builder
            .build_extract_value(lhs, 0, "l.ptr")
            .unwrap()
            .into_pointer_value();
        let l_len = self
            .builder
            .build_extract_value(lhs, 1, "l.len")
            .unwrap()
            .into_int_value();
        let r_ptr = self
            .builder
            .build_extract_value(rhs, 0, "r.ptr")
            .unwrap()
            .into_pointer_value();
        let r_len = self
            .builder
            .build_extract_value(rhs, 1, "r.len")
            .unwrap()
            .into_int_value();

        match op {
            BinOp::Eq | BinOp::NotEq => {
                // Fast reject: if lengths differ, strings are not equal.
                let len_eq = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, l_len, r_len, "len_eq")
                    .unwrap();
                // memcmp the data.
                let cmp_result = self
                    .builder
                    .build_call(
                        self.memcmp_fn,
                        &[l_ptr.into(), r_ptr.into(), l_len.into()],
                        "memcmp",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let data_eq = self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        cmp_result,
                        i32_t.const_int(0, false),
                        "data_eq",
                    )
                    .unwrap();
                let is_eq = self.builder.build_and(len_eq, data_eq, "str_eq").unwrap();
                if matches!(op, BinOp::NotEq) {
                    Ok(self.builder.build_not(is_eq, "str_ne").unwrap().into())
                } else {
                    Ok(is_eq.into())
                }
            }
            BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                // Lexicographic comparison: memcmp on min(l_len, r_len), then compare lengths.
                let cmp_lens = self
                    .builder
                    .build_int_compare(IntPredicate::ULT, l_len, r_len, "l_shorter")
                    .unwrap();
                let min_len = self
                    .builder
                    .build_select(cmp_lens, l_len, r_len, "min_len")
                    .unwrap()
                    .into_int_value();
                let cmp_result = self
                    .builder
                    .build_call(
                        self.memcmp_fn,
                        &[l_ptr.into(), r_ptr.into(), min_len.into()],
                        "memcmp",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let zero = i32_t.const_int(0, false);
                // If memcmp != 0, use its sign. If memcmp == 0, shorter string is "less".
                let cmp_is_zero = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, cmp_result, zero, "cmp_zero")
                    .unwrap();
                // When cmp == 0, compare lengths as signed i64 difference.
                let len_diff = self
                    .builder
                    .build_int_sub(l_len, r_len, "len_diff")
                    .unwrap();
                let len_diff_i32 = self
                    .builder
                    .build_int_truncate(len_diff, i32_t, "len_diff32")
                    .unwrap();
                let effective_cmp = self
                    .builder
                    .build_select(cmp_is_zero, len_diff_i32, cmp_result, "eff_cmp")
                    .unwrap()
                    .into_int_value();
                let pred = match op {
                    BinOp::Lt => IntPredicate::SLT,
                    BinOp::LtEq => IntPredicate::SLE,
                    BinOp::Gt => IntPredicate::SGT,
                    BinOp::GtEq => IntPredicate::SGE,
                    _ => unreachable!(),
                };
                let result = self
                    .builder
                    .build_int_compare(pred, effective_cmp, zero, "str_cmp")
                    .unwrap();
                Ok(result.into())
            }
            BinOp::Add => {
                // String concatenation: allocate new buffer, copy both, return new string.
                let new_len = self.builder.build_int_add(l_len, r_len, "cat_len").unwrap();
                let new_buf = self
                    .builder
                    .build_call(self.malloc_fn, &[new_len.into()], "cat_buf")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_pointer_value();
                // Copy left.
                self.builder
                    .build_memcpy(new_buf, 1, l_ptr, 1, l_len)
                    .unwrap();
                // Copy right after left.
                let i8_ty = self.context.i8_type();
                let dest2 = unsafe {
                    self.builder
                        .build_gep(i8_ty, new_buf, &[l_len], "cat_dest2")
                        .unwrap()
                };
                self.builder
                    .build_memcpy(dest2, 1, r_ptr, 1, r_len)
                    .unwrap();
                // Build result string struct.
                let str_ty = self.vec_struct_type();
                let mut agg = str_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, new_buf, 0, "cat.ptr")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, new_len, 1, "cat.len")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, new_len, 2, "cat.cap")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            _ => Err(format!("Unsupported string binary op: {:?}", op)),
        }
    }

    fn compile_float_binop(
        &self,
        op: &BinOp,
        lf: inkwell::values::FloatValue<'ctx>,
        rf: inkwell::values::FloatValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match op {
            BinOp::Add => Ok(self.builder.build_float_add(lf, rf, "fadd").unwrap().into()),
            BinOp::Sub => Ok(self.builder.build_float_sub(lf, rf, "fsub").unwrap().into()),
            BinOp::Mul => Ok(self.builder.build_float_mul(lf, rf, "fmul").unwrap().into()),
            BinOp::Div => Ok(self.builder.build_float_div(lf, rf, "fdiv").unwrap().into()),
            BinOp::Mod => Ok(self.builder.build_float_rem(lf, rf, "frem").unwrap().into()),
            BinOp::Eq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OEQ, lf, rf, "feq")
                .unwrap()
                .into()),
            BinOp::NotEq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::ONE, lf, rf, "fne")
                .unwrap()
                .into()),
            BinOp::Lt => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OLT, lf, rf, "flt")
                .unwrap()
                .into()),
            BinOp::LtEq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OLE, lf, rf, "fle")
                .unwrap()
                .into()),
            BinOp::Gt => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OGT, lf, rf, "fgt")
                .unwrap()
                .into()),
            BinOp::GtEq => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OGE, lf, rf, "fge")
                .unwrap()
                .into()),
            _ => Err(format!("Unsupported float binary op: {:?}", op)),
        }
    }

    fn to_float(
        &self,
        val: BasicValueEnum<'ctx>,
    ) -> Result<inkwell::values::FloatValue<'ctx>, String> {
        match val {
            BasicValueEnum::FloatValue(f) => Ok(f),
            BasicValueEnum::IntValue(i) => Ok(self
                .builder
                .build_signed_int_to_float(i, self.context.f64_type(), "itof")
                .unwrap()),
            _ => Err(format!("Cannot convert {:?} to float", val.get_type())),
        }
    }

    fn compile_unaryop(
        &mut self,
        op: &UnaryOp,
        val: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match op {
            UnaryOp::Neg => {
                if val.is_float_value() {
                    Ok(self
                        .builder
                        .build_float_neg(val.into_float_value(), "fneg")
                        .unwrap()
                        .into())
                } else {
                    Ok(self
                        .builder
                        .build_int_neg(val.into_int_value(), "neg")
                        .unwrap()
                        .into())
                }
            }
            UnaryOp::Not | UnaryOp::BitNot => Ok(self
                .builder
                .build_not(val.into_int_value(), "not")
                .unwrap()
                .into()),
            // Deref is handled in compile_expr before reaching here.
            UnaryOp::Deref => Err("unreachable: Deref handled in compile_expr".into()),
        }
    }

    // ── Slice coercion ────────────────────────────────────────────

    /// Synthesize a `{ptr, i64}` slice header at a call site when the
    /// argument is an Array, Vec, or Slice value and the callee parameter
    /// expects `Slice[T]` / `mut Slice[T]`.
    ///
    /// Returns `Ok(None)` when the argument is not a recognized
    /// sequence source, signalling the caller to fall back to the
    /// default argument-passing path.
    fn coerce_to_slice(
        &mut self,
        arg: &Expr,
        elem_ty: BasicTypeEnum<'ctx>,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();

        // Fast path: the argument is a named local variable whose
        // representation we already understand.
        if let ExprKind::Identifier(var_name) = &arg.kind {
            if let Some(slot) = self.variables.get(var_name.as_str()).copied() {
                // Owned Array[T, N]: point at the alloca, length is N.
                if let BasicTypeEnum::ArrayType(at) = slot.ty {
                    let len = i64_t.const_int(at.len() as u64, false);
                    return Ok(Some(self.build_slice_header(slice_ty, slot.ptr, len)));
                }
                // Already a slice: load and pass through.
                if self.slice_elem_types.contains_key(var_name.as_str()) {
                    let loaded = self
                        .builder
                        .build_load(slice_ty, slot.ptr, "slice.arg")
                        .unwrap();
                    return Ok(Some(loaded));
                }
                // Owned Vec[T]: the alloca holds the 3-field struct; load
                // its data-ptr and len fields, rebuild as a 2-field slice.
                if self.vec_elem_types.contains_key(var_name.as_str()) {
                    let vec_ty = self.vec_struct_type();
                    let data_ptr_ptr = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 0, "coerce.v.data.ptr")
                        .unwrap();
                    let data = self
                        .builder
                        .build_load(ptr_ty, data_ptr_ptr, "coerce.v.data")
                        .unwrap()
                        .into_pointer_value();
                    let len_ptr = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 1, "coerce.v.len.ptr")
                        .unwrap();
                    let len = self
                        .builder
                        .build_load(i64_t, len_ptr, "coerce.v.len")
                        .unwrap()
                        .into_int_value();
                    return Ok(Some(self.build_slice_header(slice_ty, data, len)));
                }
            }
            // Ref parameter: pointer-to-data is in ref_params.
            if let Some(&BasicTypeEnum::ArrayType(at)) = self.ref_params.get(var_name.as_str()) {
                let data = self.get_data_ptr(var_name).unwrap();
                let len = i64_t.const_int(at.len() as u64, false);
                return Ok(Some(self.build_slice_header(slice_ty, data, len)));
            }
        }

        // Range-indexing at a call boundary — e.g. `sum(a[1..4])`. Produce
        // a slice header with pointer-into-source and length `end - start`.
        if let ExprKind::Index { object, index } = &arg.kind {
            if let ExprKind::Range {
                start,
                end,
                inclusive,
            } = &index.kind
            {
                return self
                    .compile_range_slice(object, start, end, *inclusive, elem_ty)
                    .map(Some);
            }
        }

        let _ = elem_ty;
        Ok(None)
    }

    /// Assemble a two-field slice struct value from a data pointer and an
    /// i64 length.
    fn build_slice_header(
        &self,
        slice_ty: StructType<'ctx>,
        data_ptr: PointerValue<'ctx>,
        len: inkwell::values::IntValue<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        let mut agg = slice_ty.get_undef();
        agg = self
            .builder
            .build_insert_value(agg, data_ptr, 0, "slice.ptr")
            .unwrap()
            .into_struct_value();
        agg = self
            .builder
            .build_insert_value(agg, len, 1, "slice.len")
            .unwrap()
            .into_struct_value();
        agg.into()
    }

    /// Construct a slice from a `collection[start..end]` expression —
    /// emits a bounds check and produces a `{ptr + start*stride, end - start}`
    /// slice header.
    fn compile_range_slice(
        &mut self,
        object: &Expr,
        start: &Option<Box<Expr>>,
        end: &Option<Box<Expr>>,
        inclusive: bool,
        elem_ty: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();

        let start_val = if let Some(s) = start {
            self.compile_expr(s)?.into_int_value()
        } else {
            i64_t.const_int(0, false)
        };
        // Compile end expression now if present; defer to src_len for open-end
        // forms (`a..` and `..`). Inclusive adjustment applied after src_len
        // is resolved.
        let compiled_end = if let Some(e) = end {
            Some(self.compile_expr(e)?.into_int_value())
        } else {
            None
        };

        // Resolve the object to (base_ptr, length).
        let (base_ptr, src_len) = if let ExprKind::Identifier(name) = &object.kind {
            if let Some(slot) = self.variables.get(name.as_str()).copied() {
                if let BasicTypeEnum::ArrayType(at) = slot.ty {
                    (slot.ptr, i64_t.const_int(at.len() as u64, false))
                } else if self.slice_elem_types.contains_key(name.as_str()) {
                    let data_pp = self
                        .builder
                        .build_struct_gep(slice_ty, slot.ptr, 0, "rs.s.data.pp")
                        .unwrap();
                    let data = self
                        .builder
                        .build_load(ptr_ty, data_pp, "rs.s.data")
                        .unwrap()
                        .into_pointer_value();
                    let len_p = self
                        .builder
                        .build_struct_gep(slice_ty, slot.ptr, 1, "rs.s.len.p")
                        .unwrap();
                    let len = self
                        .builder
                        .build_load(i64_t, len_p, "rs.s.len")
                        .unwrap()
                        .into_int_value();
                    (data, len)
                } else if self.vec_elem_types.contains_key(name.as_str()) {
                    let vec_ty = self.vec_struct_type();
                    let data_pp = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 0, "rs.v.data.pp")
                        .unwrap();
                    let data = self
                        .builder
                        .build_load(ptr_ty, data_pp, "rs.v.data")
                        .unwrap()
                        .into_pointer_value();
                    let len_p = self
                        .builder
                        .build_struct_gep(vec_ty, slot.ptr, 1, "rs.v.len.p")
                        .unwrap();
                    let len = self
                        .builder
                        .build_load(i64_t, len_p, "rs.v.len")
                        .unwrap()
                        .into_int_value();
                    (data, len)
                } else {
                    return Err(format!(
                        "range-slice requires Array, Vec, or Slice source; variable '{}' is neither",
                        name
                    ));
                }
            } else if self.ref_params.contains_key(name.as_str()) {
                // Ref-parameter path: pointer to inner data.
                let inner = *self.ref_params.get(name.as_str()).unwrap();
                if let BasicTypeEnum::ArrayType(at) = inner {
                    let data = self.get_data_ptr(name).unwrap();
                    (data, i64_t.const_int(at.len() as u64, false))
                } else {
                    return Err("range-slice on ref parameter requires ref Array".into());
                }
            } else {
                return Err(format!("Undefined variable '{}' in range-slice", name));
            }
        } else {
            return Err("range-slice requires a named source variable".into());
        };

        // Resolve end: open-end (`a..`, `..`) uses src_len; inclusive adds 1.
        let mut end_val = compiled_end.unwrap_or(src_len);
        if inclusive {
            end_val = self
                .builder
                .build_int_add(end_val, i64_t.const_int(1, false), "end.incl")
                .unwrap();
        }

        // Bounds check: start <= end && end <= len.
        let fn_val = self.current_fn.unwrap();
        let oob_bb = self.context.append_basic_block(fn_val, "slice.oob");
        let ok_bb = self.context.append_basic_block(fn_val, "slice.ok");
        let se_bad = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGT, start_val, end_val, "s.le.e")
            .unwrap();
        let el_bad = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGT, end_val, src_len, "e.le.len")
            .unwrap();
        let any_bad = self.builder.build_or(se_bad, el_bad, "slice.bad").unwrap();
        self.builder
            .build_conditional_branch(any_bad, oob_bb, ok_bb)
            .unwrap();
        self.builder.position_at_end(oob_bb);
        self.emit_panic("slice range out of bounds");
        self.builder.build_unreachable().unwrap();
        self.builder.position_at_end(ok_bb);

        // For an Array source, `base_ptr` is the alloca of `[N x T]` —
        // compute the element pointer via two-index GEP. For a Vec / Slice
        // source, `base_ptr` is already an element pointer, so we use a
        // one-index GEP. We distinguish by asking whether the source var is
        // an array alloca (known type) or a loaded data pointer.
        let source_is_array = if let ExprKind::Identifier(name) = &object.kind {
            if let Some(slot) = self.variables.get(name.as_str()) {
                matches!(slot.ty, BasicTypeEnum::ArrayType(_))
            } else if let Some(&inner) = self.ref_params.get(name.as_str()) {
                matches!(inner, BasicTypeEnum::ArrayType(_))
            } else {
                false
            }
        } else {
            false
        };

        let elem_ptr = if source_is_array {
            // GEP into `[N x T]*` using [0, start].
            let arr_ty = if let ExprKind::Identifier(name) = &object.kind {
                if let Some(slot) = self.variables.get(name.as_str()).copied() {
                    slot.ty
                } else if let Some(&inner) = self.ref_params.get(name.as_str()) {
                    inner
                } else {
                    return Err("range-slice: lost array type".into());
                }
            } else {
                return Err("range-slice: non-identifier array source".into());
            };
            let zero = i64_t.const_int(0, false);
            unsafe {
                self.builder
                    .build_gep(arr_ty, base_ptr, &[zero, start_val], "slice.elem.ptr")
                    .unwrap()
            }
        } else {
            // GEP into `T*` using [start].
            unsafe {
                self.builder
                    .build_gep(elem_ty, base_ptr, &[start_val], "slice.elem.ptr")
                    .unwrap()
            }
        };

        let new_len = self
            .builder
            .build_int_sub(end_val, start_val, "slice.new.len")
            .unwrap();
        Ok(self.build_slice_header(slice_ty, elem_ptr, new_len))
    }

    // ── Call ──────────────────────────────────────────────────────

    /// Lower a `size_of[T]()` / `align_of[T]()` call to the matching
    /// LLVM constant. `size_of` uses inkwell's `BasicTypeEnum::size_of()`
    /// (a constant-expr returning i64). `align_of` uses
    /// `TargetData::get_abi_alignment()` (a `u32` ABI alignment for the
    /// host target) materialized as an i64 constant. Both return `usize`
    /// to match the typechecker's signature, which lowers to i64 on the
    /// 64-bit-only target the rest of codegen assumes.
    fn compile_layout_query_intrinsic(
        &mut self,
        name: &str,
        explicit_args: &[GenericArg],
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // The typechecker has already validated argument shape; do a
        // defensive check here so a divergent path (e.g., direct codegen
        // invocation in tests) doesn't crash.
        for arg in args {
            self.compile_expr(&arg.value)?;
        }
        let ty_expr = match explicit_args {
            [GenericArg::Type(te)] => te,
            _ => {
                return Ok(self.context.i64_type().const_int(0, false).into());
            }
        };
        let llvm_ty = self.llvm_type_for_type_expr(ty_expr);
        let i64_ty = self.context.i64_type();
        match name {
            "size_of" => {
                let size = llvm_ty
                    .size_of()
                    .ok_or_else(|| "size_of[T]: type is not sized".to_string())?;
                Ok(size.into())
            }
            "align_of" => {
                let target_data = self.ensure_target_data()?;
                let align = target_data.get_abi_alignment(&llvm_ty);
                Ok(i64_ty.const_int(u64::from(align), false).into())
            }
            _ => unreachable!("compile_layout_query_intrinsic dispatched on unknown name"),
        }
    }

    fn compile_call(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Cooperative cancel check before each call inside a par-branch.
        // No-op when not inside a par branch. Narrowed against the
        // `callee_effectful` side-table when the callee name is statically
        // recoverable (free fn or `Type.assoc`); other shapes (closure, FFI
        // through identifier resolved at link time, etc.) fall back to the
        // conservative "always fire" path via `None`.
        let callee_key: Option<String> = match &callee.kind {
            ExprKind::Identifier(n) => Some(n.clone()),
            ExprKind::Path { segments, .. } if segments.len() == 2 => {
                Some(format!("{}.{}", segments[0], segments[1]))
            }
            _ => None,
        };
        self.emit_branch_cancel_check("call", callee_key.as_deref());

        // Theme 6 sub-step 3: `with_provider[R](provider, ||body)`.
        // Recognize the call shape before the generic dispatch below — the
        // callee is an `Index` expression which would otherwise fall through
        // to the unknown-callee path and return const-0. The lowering pushes
        // a `ProviderFrame` onto the runtime stack, runs the body, pops, and
        // yields the body's value.
        if let Some((resource, provider_expr, closure_expr)) =
            match_with_provider_call(callee, args)
        {
            return self.compile_with_provider(&resource, provider_expr, closure_expr);
        }

        // Const generics slice 1c: `f[8]()` parses as
        // `Call { callee: Index { object: Identifier(name), index: literal }, args }`.
        // The typechecker disambiguation routes through a synthetic
        // Path-with-generic-args callee at type-check time, but the
        // codegen sees the original AST. Apply the same rewrite here
        // when the indexed object resolves to a generic free function
        // in `generic_fns`. (`callbacks[0]()` keeps its Index-then-Call
        // shape because `callbacks` isn't in `generic_fns`.)
        if let ExprKind::Index { object, index } = &callee.kind {
            if let ExprKind::Identifier(name) = &object.kind {
                let is_literal_index = matches!(
                    &index.kind,
                    ExprKind::Integer(_, _) | ExprKind::Bool(_) | ExprKind::CharLit(_)
                );
                if is_literal_index && self.generic_fns.contains_key(name) {
                    let explicit_args = vec![GenericArg::Const((**index).clone())];
                    return self.compile_generic_call(name, args, Some(&explicit_args));
                }
            }
        }

        // Layout-introspection intrinsics (`size_of[T]()` / `align_of[T]()`)
        // single-arg shape. The parser produces `Call { Index { Ident,
        // T_expr } }` because `lookahead_generic_args_call` requires a
        // top-level comma; recover the type expression from the value-
        // position `Expr` and dispatch the intrinsic. The typechecker
        // handles the matching shape in `infer_call`; this codegen mirror
        // is here so the placeholder body in
        // `runtime/stdlib/intrinsics.kara` is never lowered.
        if let ExprKind::Index { object, index } = &callee.kind {
            if let ExprKind::Identifier(name) = &object.kind {
                if (name == "size_of" || name == "align_of") && args.is_empty() {
                    if let Some(te) = expr_as_type_expr_codegen(index) {
                        let synth = vec![GenericArg::Type(te)];
                        return self.compile_layout_query_intrinsic(name, &synth, args);
                    }
                }
            }
        }

        // Associated function calls: Vec::new(), etc. Theme 6 sub-step 4
        // intercepts `R.method(args)` where R is an `effect resource R: T`
        // before assoc-call dispatch: those go through the runtime stack
        // via `karac_provider_lookup` + indirect vtable call. Any other
        // 2-segment path (Vec::new, T.from, primitive ops, user
        // `Type.method`, …) falls through to `compile_assoc_call`.
        if let ExprKind::Path { segments, .. } = &callee.kind {
            if segments.len() == 2 {
                if let Some(value) =
                    self.try_compile_provider_dispatch(&segments[0], &segments[1], args)?
                {
                    return Ok(value);
                }
                return self.compile_assoc_call(&segments[0], &segments[1], args);
            }
        }

        // Const generics slice 1b: `make_arr[i64, 4]()` parses callee
        // as `Path { segments: [name], generic_args: Some(args) }` (a
        // bare identifier with explicit generic args). Extract the
        // name + explicit generic args so the generic-call path can
        // bind the user-supplied const-args into the mango key.
        let (name, explicit_generic_args): (String, Option<Vec<GenericArg>>) = match &callee.kind {
            ExprKind::Identifier(n) => (n.clone(), None),
            ExprKind::Path {
                segments,
                generic_args: Some(ga),
            } if segments.len() == 1 => (segments[0].clone(), Some(ga.clone())),
            _ => return Ok(self.context.i64_type().const_int(0, false).into()),
        };

        if name == "println" || name == "print" {
            return self.compile_print(&name, args);
        }

        // Layout-introspection intrinsics. Intercepted before the
        // generic-call lookup so the `{ 0 }` placeholder body in
        // `runtime/stdlib/intrinsics.kara` is never lowered. The
        // typechecker has already rejected opaque foreign type args
        // with `E_OPAQUE_TYPE_NO_KNOWN_SIZE`, so the type lowered here
        // is sized by construction.
        if name == "size_of" || name == "align_of" {
            if let Some(ga) = explicit_generic_args.as_deref() {
                return self.compile_layout_query_intrinsic(&name, ga, args);
            }
        }

        // Check if this is an enum variant constructor (tuple variant)
        if let Some(enum_val) = self.try_compile_enum_variant(&name, args)? {
            return Ok(enum_val);
        }

        // Check if this is a call to a generic function (monomorphize on demand)
        if self.generic_fns.contains_key(&name) {
            return self.compile_generic_call(&name, args, explicit_generic_args.as_deref());
        }

        // Check if this is an indirect call through a closure variable.
        if self.closure_fn_types.contains_key(&name) {
            return self.compile_closure_call(&name, args);
        }

        let func = match self.module.get_function(&name) {
            Some(f) => f,
            None => {
                // Unknown function — silently return 0 (e.g. stdlib builtins not yet codegen'd)
                return Ok(self.context.i64_type().const_int(0, false).into());
            }
        };

        let ref_flags = self.fn_param_ref.get(&name).cloned().unwrap_or_default();
        let slice_elems = self
            .fn_param_slice_elem
            .get(&name)
            .cloned()
            .unwrap_or_default();
        let mut compiled_args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let is_ref = ref_flags.get(i).copied().unwrap_or(false);
            if is_ref {
                // Pass a pointer to the variable's data instead of the loaded value.
                if let ExprKind::Identifier(var_name) = &a.value.kind {
                    if let Some(ptr) = self.get_data_ptr(var_name) {
                        compiled_args.push(ptr.into());
                        continue;
                    }
                }
            }
            // Slice-parameter coercion: if this parameter slot expects
            // Slice[T] / mut Slice[T] and the argument is an Array[T, N],
            // Vec[T], or already a slice, synthesize the `{ptr, i64}`
            // slice header at the call site. See design.md § Slices.
            if let Some(Some(elem_ty)) = slice_elems.get(i).cloned() {
                if let Some(slice_val) = self.coerce_to_slice(&a.value, elem_ty)? {
                    compiled_args.push(slice_val.into());
                    continue;
                }
            }
            let val = self.compile_expr(&a.value)?;
            compiled_args.push(BasicMetadataValueEnum::from(val));
        }

        let call = self
            .builder
            .build_call(func, &compiled_args, "call")
            .unwrap();

        let basic_val = call.try_as_basic_value();
        if basic_val.is_instruction() {
            Ok(self.context.i64_type().const_int(0, false).into())
        } else {
            Ok(basic_val.unwrap_basic())
        }
    }

    /// Try to construct an enum variant value if `name` matches a known variant.
    /// Returns `None` if `name` is not an enum variant.
    fn try_compile_enum_variant(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        // Find which enum this variant belongs to. Prefer
        // user-declared enums over the seeded built-ins (`Option`,
        // `Result`) when a variant name (`Some` / `None` / `Ok` /
        // `Err`) collides — without this preference, HashMap
        // iteration order non-deterministically picks the wider seeded
        // `Option` layout for a user-defined `MyOption.Some(...)`
        // construction, producing a {i64,i64,i64,i64} value where
        // the user fn expects {i64, i64}. Symmetric to the destructure
        // disambiguation in `bind_pattern_values`.
        let enum_name = {
            let mut user_match: Option<String> = None;
            let mut seed_match: Option<String> = None;
            for (en, layout) in &self.enum_layouts {
                if layout.tags.contains_key(name) {
                    if en == "Option" || en == "Result" {
                        seed_match.get_or_insert_with(|| en.clone());
                    } else {
                        user_match.get_or_insert_with(|| en.clone());
                    }
                }
            }
            user_match.or(seed_match)
        };

        let enum_name = match enum_name {
            Some(n) => n,
            None => return Ok(None),
        };

        let (tag, llvm_type) = {
            let layout = &self.enum_layouts[&enum_name];
            (*layout.tags.get(name).unwrap(), layout.llvm_type)
        };

        let i64_t = self.context.i64_type();

        // Shared enum: heap-allocate with refcount header.
        if let Some(info) = self.shared_types.get(&enum_name).cloned() {
            let ptr = self.emit_rc_alloc(info.heap_type);
            // Tag at heap index 1 (index 0 is refcount).
            let tag_ptr = self
                .builder
                .build_struct_gep(info.heap_type, ptr, 1, "sh_tag")
                .unwrap();
            self.builder
                .build_store(tag_ptr, i64_t.const_int(tag, false))
                .unwrap();
            // Payload words at heap indices 2, 3, … . Shared enums share
            // the same per-variant `field_word_offsets` layout as
            // non-shared enums; the heap struct's payload-word count is
            // sized to `max_payload_words` at declare time. Each source
            // field decomposes into its assigned word range.
            let offsets: Vec<(usize, usize)> = self.enum_layouts[&enum_name]
                .field_word_offsets
                .get(name)
                .cloned()
                .unwrap_or_default();
            for (i, arg) in args.iter().enumerate() {
                let val = self.compile_expr(&arg.value)?;
                let (start_word, num_words) = offsets.get(i).copied().unwrap_or((i, 1));
                let words = self.coerce_to_payload_words(val, num_words)?;
                for (j, w) in words.into_iter().enumerate() {
                    let word_ptr = self
                        .builder
                        .build_struct_gep(
                            info.heap_type,
                            ptr,
                            (start_word + j + 2) as u32, // +2 for refcount + tag
                            "sh_word",
                        )
                        .unwrap();
                    self.builder.build_store(word_ptr, w).unwrap();
                }
                // Phase 7.2 Slice DP — move-suppression for the source
                // binding when the arg is an Identifier referencing a
                // tracked Vec/String variable. Zeroing the source's
                // `cap` field neutralizes the existing
                // `FreeVecBuffer` cleanup at scope exit (it's gated
                // on `cap > 0`), preventing a double-free against the
                // payload buffer the new enum binding now owns. See
                // `suppress_source_vec_cleanup_for_arg` for the
                // shape-detection path.
                self.suppress_source_vec_cleanup_for_arg(&arg.value);
            }
            return Ok(Some(ptr.into()));
        }

        // Non-shared enum: stack-allocated aggregate.
        let mut agg = llvm_type.get_undef();

        // Store tag as field 0
        agg = self
            .builder
            .build_insert_value(agg, i64_t.const_int(tag, false), 0, "tag")
            .unwrap()
            .into_struct_value();

        // Compound-payload enum codegen (CP4): consult the variant's
        // `field_word_offsets` so each source field's value is written
        // into its assigned word range (start_word .. start_word +
        // num_words). Multi-word aggregates (String / Vec / user
        // structs / tuples) decompose to a sequence of i64 words via
        // `coerce_to_payload_words`; primitives produce a single word
        // and match the legacy `coerce_to_i64` path. Reading back is
        // the destructure path's job (see `bind_pattern_values`).
        let offsets: Vec<(usize, usize)> = self.enum_layouts[&enum_name]
            .field_word_offsets
            .get(name)
            .cloned()
            .unwrap_or_default();
        for (i, arg) in args.iter().enumerate() {
            let val = self.compile_expr(&arg.value)?;
            let (start_word, num_words) = offsets.get(i).copied().unwrap_or((i, 1)); // legacy fallback if layout missing
            let words = self.coerce_to_payload_words(val, num_words)?;
            for (j, w) in words.into_iter().enumerate() {
                agg = self
                    .builder
                    .build_insert_value(
                        agg,
                        w,
                        (start_word + j + 1) as u32, // +1 for tag field
                        "word",
                    )
                    .unwrap()
                    .into_struct_value();
            }
            // Phase 7.2 Slice DP — move-suppression. Same shape as the
            // shared-enum branch above; zero the source binding's
            // `cap` so its scope-exit `FreeVecBuffer` becomes a no-op.
            // The new enum binding owns the buffer.
            self.suppress_source_vec_cleanup_for_arg(&arg.value);
        }

        Ok(Some(agg.into()))
    }

    /// Phase 7.2 Slice DP — move-suppression helper. When an enum-
    /// variant constructor's argument is an Identifier referencing a
    /// tracked Vec/String binding, zero the source binding's `cap`
    /// field. The existing `CleanupAction::FreeVecBuffer` drain checks
    /// `cap > 0` before invoking `free`, so a zeroed cap turns the
    /// scope-exit cleanup into a no-op for that source. The new enum
    /// binding's `EnumDrop` cleanup then owns the buffer's free.
    ///
    /// No-op for non-Identifier args (rvalue / literal / call result —
    /// no source alloca to mutate; the buffer is already an rvalue
    /// owned solely by the new enum) and for Identifier args that
    /// don't resolve to a tracked Vec/String variable (slice / int /
    /// struct / etc.).
    ///
    /// This mirrors the slice-A return-slot mechanism's cleanup
    /// strategy at `compile_function_body` (around line 4343), which
    /// also opts not to register a parent-side cleanup when the slot
    /// value is moved into a downstream consumer — the consumer
    /// becomes the unique cleanup owner.
    /// Move-aware scope-exit cleanup suppression for the function's
    /// tail-expression return. When the body's final expression is
    /// an `Identifier` naming a tracked Vec / String binding, the
    /// returned struct value carries the binding's data pointer out
    /// — but the let-site's `track_vec_var` queued a scope-exit
    /// `FreeVecBuffer` that would `free` that buffer before the
    /// caller can use it. Zero the source's `cap` field so the
    /// cleanup's `cap > 0` guard skips the free; the loaded return
    /// struct retains the original cap, and the caller's own
    /// scope-exit cleanup frees the buffer exactly once.
    fn suppress_cleanup_for_tail_return(&self, body: &Block) {
        // Walk the tail of the body: if the final expression of the
        // block (or the value of the last `return expr;` statement)
        // is a bare Identifier for a tracked Vec / String, suppress.
        let from_final: Option<&Expr> = body.final_expr.as_deref();
        let from_last_stmt: Option<&Expr> = body.stmts.last().and_then(|s| match &s.kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Return(Some(boxed)) => Some(boxed.as_ref()),
                _ => Some(e),
            },
            _ => None,
        });
        if let Some(expr) = from_final.or(from_last_stmt) {
            self.suppress_source_vec_cleanup_for_arg(expr);
        }
    }

    fn suppress_source_vec_cleanup_for_arg(&self, arg_expr: &Expr) {
        let var_name = match &arg_expr.kind {
            ExprKind::Identifier(n) => n.as_str(),
            _ => return,
        };
        let slot = match self.variables.get(var_name) {
            Some(s) => *s,
            None => return,
        };
        let vec_ty = self.vec_struct_type();
        let i64_t = self.context.i64_type();
        // Vec / String binding: zero the source's `cap` so the source's
        // `FreeVecBuffer` cleanup's `cap > 0` guard skips. The consumer
        // now owns the buffer.
        if self.vec_elem_types.contains_key(var_name) {
            if let Ok(cap_ptr) = self
                .builder
                .build_struct_gep(vec_ty, slot.ptr, 2, "move.cap.p")
            {
                let zero = i64_t.const_int(0, false);
                let _ = self.builder.build_store(cap_ptr, zero);
            }
            return;
        }
        // Struct binding (slice γ, 2026-05-14): when the source is a
        // tracked non-shared struct, walk its fields and zero each
        // Vec/String field's `cap`. The struct's `StructDrop` cleanup
        // will then no-op on each freed field — the consumer (caller
        // / new binding / struct constructor) now owns the heap content.
        // Without this, returning a struct-with-Vec from a function
        // double-frees the inner buffer against the caller's own
        // tracked-struct cleanup. Map/Set field handles are NOT zeroed
        // by this helper today — they need a `null`-marker convention
        // through `karac_map_free` to no-op, which would be a separate
        // runtime change (filed under slice δ as the per-field K/V
        // type-info-aware drop work).
        if let Some(type_name) = self.var_type_names.get(var_name).cloned() {
            if let Some(&st) = self.struct_types.get(&type_name) {
                if let Some(field_names) = self.struct_field_type_names.get(&type_name) {
                    for (i, opt_name) in field_names.iter().enumerate() {
                        let is_vec_field = matches!(
                            opt_name.as_deref(),
                            Some("Vec") | Some("VecDeque") | Some("String")
                        );
                        if !is_vec_field {
                            continue;
                        }
                        if let Ok(field_ptr) = self.builder.build_struct_gep(
                            st,
                            slot.ptr,
                            i as u32,
                            &format!("move.field{i}.p"),
                        ) {
                            if let Ok(cap_ptr) = self.builder.build_struct_gep(
                                vec_ty,
                                field_ptr,
                                2,
                                &format!("move.field{i}.cap.p"),
                            ) {
                                let zero = i64_t.const_int(0, false);
                                let _ = self.builder.build_store(cap_ptr, zero);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Compound-payload enum codegen (CP4 helper) — decompose an
    /// arbitrary `BasicValueEnum` into exactly `num_words` i64 words
    /// suitable for storage in an enum payload area. Primitives (bool /
    /// int / float / pointer) always produce one word via `coerce_to_i64`;
    /// `num_words == 1` therefore short-circuits to the existing
    /// behaviour. Aggregates (String / Vec / user struct / tuple)
    /// destructure via `extract_value` over their LLVM-field layout and
    /// recurse on each field.
    ///
    /// If the supplied value's natural word count differs from the
    /// requested `num_words`, the result is padded with zeros (over-shoot)
    /// or truncated (under-shoot). Both branches log nothing — they're
    /// the safety nets for the fallback paths in
    /// `payload_word_count_for_type_expr` (which conservatively
    /// returns 1 for unknown types).
    fn coerce_to_payload_words(
        &self,
        val: BasicValueEnum<'ctx>,
        num_words: usize,
    ) -> Result<Vec<inkwell::values::IntValue<'ctx>>, String> {
        // Primitive fast path.
        if num_words <= 1 {
            return Ok(vec![self.coerce_to_i64(val)?]);
        }
        let mut out: Vec<inkwell::values::IntValue<'ctx>> = Vec::with_capacity(num_words);
        match val {
            BasicValueEnum::StructValue(sv) => {
                let n_fields = sv.get_type().count_fields();
                for i in 0..n_fields {
                    let f = self
                        .builder
                        .build_extract_value(sv, i, "pl.f")
                        .map_err(|e| {
                            format!(
                                "coerce_to_payload_words: extract_value failed at field {}: {:?}",
                                i, e
                            )
                        })?;
                    // Recurse: a struct field can itself be an aggregate
                    // (e.g. a user struct whose field is a String). Each
                    // top-level LLVM field of `sv` contributes its own
                    // word count to the running total.
                    let sub_count = match f {
                        BasicValueEnum::StructValue(ssv) => ssv.get_type().count_fields() as usize,
                        BasicValueEnum::ArrayValue(av) => av.get_type().len() as usize,
                        _ => 1,
                    };
                    let sub_words = if sub_count <= 1 {
                        vec![self.coerce_to_i64(f)?]
                    } else {
                        self.coerce_to_payload_words(f, sub_count)?
                    };
                    for w in sub_words {
                        if out.len() < num_words {
                            out.push(w);
                        }
                    }
                }
            }
            BasicValueEnum::ArrayValue(av) => {
                let len = av.get_type().len();
                for i in 0..len {
                    let f = self
                        .builder
                        .build_extract_value(av, i, "pl.a")
                        .map_err(|e| {
                            format!(
                                "coerce_to_payload_words: extract_value (array) failed at {}: {:?}",
                                i, e
                            )
                        })?;
                    if out.len() >= num_words {
                        break;
                    }
                    out.push(self.coerce_to_i64(f)?);
                }
            }
            _ => {
                out.push(self.coerce_to_i64(val)?);
            }
        }
        // Pad / truncate to exact width.
        let i64_t = self.context.i64_type();
        while out.len() < num_words {
            out.push(i64_t.const_int(0, false));
        }
        out.truncate(num_words);
        Ok(out)
    }

    /// Build an `Option[V]` aggregate at the merge BB via per-payload-word phis.
    /// Mirrors the `Vec.pop` precedent at line 8588: 1 tag phi + 3 word phis,
    /// then `build_insert_value` at fields 0..=3. Caller is responsible for
    /// having computed `some_payload_words` (length 3, via
    /// `coerce_to_payload_words(elem_val, 3)`) inside the some-end BB and
    /// having positioned the builder at the merge BB. None-side fills all
    /// payload words with 0; tag is 1 on the some side and 0 on the none side.
    fn build_option_some_via_phis(
        &self,
        some_payload_words: &[inkwell::values::IntValue<'ctx>],
        some_end_bb: inkwell::basic_block::BasicBlock<'ctx>,
        none_bb: inkwell::basic_block::BasicBlock<'ctx>,
        name_prefix: &str,
    ) -> BasicValueEnum<'ctx> {
        let i64_t = self.context.i64_type();
        let zero = i64_t.const_int(0, false);
        let one = i64_t.const_int(1, false);
        let option_ty = self.enum_layouts["Option"].llvm_type;

        let tag_phi = self
            .builder
            .build_phi(i64_t, &format!("{name_prefix}.tag"))
            .unwrap();
        tag_phi.add_incoming(&[(&zero, none_bb), (&one, some_end_bb)]);

        let mut word_phis: Vec<inkwell::values::PhiValue<'ctx>> =
            Vec::with_capacity(some_payload_words.len());
        for (i, w) in some_payload_words.iter().enumerate() {
            let phi = self
                .builder
                .build_phi(i64_t, &format!("{name_prefix}.w{i}"))
                .unwrap();
            phi.add_incoming(&[(&zero, none_bb), (w, some_end_bb)]);
            word_phis.push(phi);
        }

        let mut agg: BasicValueEnum<'ctx> = option_ty.get_undef().into();
        agg = self
            .builder
            .build_insert_value(
                agg.into_struct_value(),
                tag_phi.as_basic_value(),
                0,
                &format!("{name_prefix}.tag.f"),
            )
            .unwrap()
            .into_struct_value()
            .into();
        for (i, phi) in word_phis.iter().enumerate() {
            agg = self
                .builder
                .build_insert_value(
                    agg.into_struct_value(),
                    phi.as_basic_value(),
                    (i + 1) as u32,
                    &format!("{name_prefix}.w{i}.f"),
                )
                .unwrap()
                .into_struct_value()
                .into();
        }
        agg
    }

    /// Coerce an arbitrary value to i64 for storage in an enum payload word.
    fn coerce_to_i64(
        &self,
        val: BasicValueEnum<'ctx>,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        let i64_t = self.context.i64_type();
        match val {
            BasicValueEnum::IntValue(iv) => {
                let w = iv.get_type().get_bit_width();
                if w == 64 {
                    Ok(iv)
                } else if w < 64 {
                    Ok(self.builder.build_int_z_extend(iv, i64_t, "zext").unwrap())
                } else {
                    Ok(self.builder.build_int_truncate(iv, i64_t, "trunc").unwrap())
                }
            }
            BasicValueEnum::FloatValue(fv) => Ok(self
                .builder
                .build_bit_cast(fv, i64_t, "fcast")
                .unwrap()
                .into_int_value()),
            BasicValueEnum::PointerValue(pv) => {
                Ok(self.builder.build_ptr_to_int(pv, i64_t, "ptoi").unwrap())
            }
            // Single-field structs (e.g. `MyError { code: i64 }`) collapse to
            // their field-0 value so the result fits a uniform i64 payload
            // word. Multi-field structs intentionally fall through to the
            // zero default — there's no faithful single-i64 encoding for
            // them, and any such case here is a codegen-shape bug elsewhere
            // that we'd rather see surface than paper over.
            BasicValueEnum::StructValue(sv) if sv.get_type().count_fields() == 1 => {
                let field = self
                    .builder
                    .build_extract_value(sv, 0, "struct.f0")
                    .unwrap();
                self.coerce_to_i64(field)
            }
            _ => Ok(i64_t.const_int(0, false)),
        }
    }

    /// Look up a unit enum variant by identifier name and construct its value.
    fn try_unit_enum_variant(&self, name: &str) -> Option<BasicValueEnum<'ctx>> {
        // Symmetric to `try_compile_enum_variant`'s user-declared-vs-
        // seeded preference: when a variant name (`None` / `Some` /
        // `Ok` / `Err`) collides between a user-defined enum and the
        // seeded built-ins, pick the user-declared one. HashMap
        // iteration order is non-deterministic otherwise, and the
        // wider seeded `Option` layout would mis-construct a value
        // for a user-defined `MyOption.None`.
        let (mut user_pick, mut seed_pick) = (None, None);
        for (enum_name, layout) in &self.enum_layouts {
            if let Some(&tag) = layout.tags.get(name) {
                if layout.field_counts.get(name).copied().unwrap_or(0) == 0 {
                    if enum_name == "Option" || enum_name == "Result" {
                        seed_pick.get_or_insert((enum_name.clone(), tag, layout));
                    } else {
                        user_pick.get_or_insert((enum_name.clone(), tag, layout));
                    }
                }
            }
        }
        let (enum_name, tag, layout) = user_pick.or(seed_pick)?;
        let i64_t = self.context.i64_type();

        // Shared enum: heap-allocate.
        if let Some(info) = self.shared_types.get(&enum_name) {
            let ptr = self.emit_rc_alloc(info.heap_type);
            let tag_ptr = self
                .builder
                .build_struct_gep(info.heap_type, ptr, 1, "sh_tag")
                .unwrap();
            self.builder
                .build_store(tag_ptr, i64_t.const_int(tag, false))
                .unwrap();
            return Some(ptr.into());
        }

        let mut agg = layout.llvm_type.get_undef();
        agg = self
            .builder
            .build_insert_value(agg, i64_t.const_int(tag, false), 0, "tag")
            .unwrap()
            .into_struct_value();
        Some(agg.into())
    }

    // ── Closure compilation ────────────────────────────────────────

    /// The LLVM struct type used to represent a closure fat-pointer: `{ ptr fn_ptr, ptr env_ptr }`.
    fn closure_value_type(&self) -> StructType<'ctx> {
        let ptr = self.context.ptr_type(AddressSpace::default());
        self.context.struct_type(&[ptr.into(), ptr.into()], false)
    }

    /// Compile `|params| body` into a fat-pointer value `{ fn_ptr, env_ptr }`.
    ///
    /// Sets `pending_closure_fn_type` so the surrounding `let` binding can register the
    /// function type for later indirect calls.
    fn compile_closure(
        &mut self,
        params: &[ClosureParam],
        body: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let id = self.closure_counter;
        self.closure_counter += 1;
        let fn_name = format!("__closure_{}", id);

        // 1. Collect free variables (names referenced in body, not in params, present in scope).
        let free_vars = self.collect_closure_free_vars(params, body);

        // 2. Build the env struct type: { T0_cap, T1_cap, ... }.
        //    Use a dummy i8 when there are no captures so we always have a valid struct type.
        let env_field_types: Vec<BasicTypeEnum<'ctx>> = if free_vars.is_empty() {
            vec![self.context.i8_type().into()]
        } else {
            free_vars.iter().map(|n| self.variables[n].ty).collect()
        };
        let env_struct_ty = self.context.struct_type(&env_field_types, false);

        // 3. Determine param types. Source annotation wins, otherwise consult
        //    `pending_closure_param_hints` (caller pushdown — e.g. `Vec.sort_by`
        //    handing the element type to a `|a, b|` comparator), otherwise
        //    fall back to i64.
        let param_hints = self.pending_closure_param_hints.take();
        let param_llvm_types: Vec<BasicTypeEnum<'ctx>> = params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if let Some(te) = p.ty.as_ref() {
                    return self.llvm_type_for_type_expr(te);
                }
                if let Some(hints) = param_hints.as_ref() {
                    if let Some(&hinted) = hints.get(i) {
                        return hinted;
                    }
                }
                self.context.i64_type().into()
            })
            .collect();

        // 4. Infer return type from the body expression.
        let closure_param_types: HashMap<String, BasicTypeEnum<'ctx>> = params
            .iter()
            .zip(param_llvm_types.iter())
            .filter_map(|(cp, ty)| {
                if let PatternKind::Binding(n) = &cp.pattern.kind {
                    Some((n.clone(), *ty))
                } else {
                    None
                }
            })
            .collect();
        let return_ty = self.infer_closure_return_type(body, &closure_param_types);

        // 5. Declare the closure function: fn(ptr env_ptr, T0, T1, ...) -> R.
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let mut fn_param_types: Vec<BasicMetadataTypeEnum<'ctx>> =
            vec![BasicMetadataTypeEnum::from(ptr_ty)];
        for &ty in &param_llvm_types {
            fn_param_types.push(BasicMetadataTypeEnum::from(ty));
        }
        let fn_type = match return_ty {
            BasicTypeEnum::IntType(t) => t.fn_type(&fn_param_types, false),
            BasicTypeEnum::FloatType(t) => t.fn_type(&fn_param_types, false),
            BasicTypeEnum::PointerType(t) => t.fn_type(&fn_param_types, false),
            BasicTypeEnum::StructType(t) => t.fn_type(&fn_param_types, false),
            BasicTypeEnum::ArrayType(t) => t.fn_type(&fn_param_types, false),
            BasicTypeEnum::VectorType(t) => t.fn_type(&fn_param_types, false),
            BasicTypeEnum::ScalableVectorType(_) => {
                self.context.void_type().fn_type(&fn_param_types, false)
            }
        };
        let closure_fn = self.module.add_function(&fn_name, fn_type, None);

        // 6. Save outer codegen state — we're about to compile a new function inline.
        let saved_bb = self.builder.get_insert_block();
        let saved_fn = self.current_fn;
        let saved_vars = std::mem::take(&mut self.variables);
        let saved_var_types = std::mem::take(&mut self.var_type_names);
        let saved_loop_stack = std::mem::take(&mut self.loop_stack);
        let saved_subst = std::mem::take(&mut self.type_subst);
        let saved_cfn = std::mem::take(&mut self.closure_fn_types);
        let saved_pct = self.pending_closure_fn_type.take();

        // 7. Build the closure body.
        self.current_fn = Some(closure_fn);
        let entry = self.context.append_basic_block(closure_fn, "entry");
        self.builder.position_at_end(entry);

        // 7a. Load captured vars from the env struct (param 0 = env ptr).
        let env_ptr = closure_fn.get_nth_param(0).unwrap().into_pointer_value();
        // Load the env struct value through the env pointer.
        let env_val = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(env_struct_ty.into(), env_ptr, "__env")
            .unwrap();

        if !free_vars.is_empty() {
            for (i, var_name) in free_vars.iter().enumerate() {
                let cap_ty = env_field_types[i];
                let field_val = self
                    .builder
                    .build_extract_value(env_val.into_struct_value(), i as u32, var_name)
                    .unwrap();
                let alloca = self.create_entry_alloca(closure_fn, var_name, cap_ty);
                self.builder.build_store(alloca, field_val).unwrap();
                self.variables.insert(
                    var_name.clone(),
                    VarSlot {
                        ptr: alloca,
                        ty: cap_ty,
                    },
                );
                // Propagate the outer scope's struct/enum type binding so
                // method dispatch inside the closure can route through the
                // user impl-block path.
                if let Some(type_name) = saved_var_types.get(var_name) {
                    self.var_type_names
                        .insert(var_name.clone(), type_name.clone());
                }
            }
        }

        // 7b. Bind closure params (fn params 1..n).
        for (i, (cp, ty)) in params.iter().zip(param_llvm_types.iter()).enumerate() {
            let param_val = closure_fn.get_nth_param((i + 1) as u32).unwrap();
            let param_name = match &cp.pattern.kind {
                PatternKind::Binding(n) => n.clone(),
                _ => format!("_cp{}", i),
            };
            let alloca = self.create_entry_alloca(closure_fn, &param_name, *ty);
            self.builder.build_store(alloca, param_val).unwrap();
            self.variables.insert(
                param_name,
                VarSlot {
                    ptr: alloca,
                    ty: *ty,
                },
            );
        }

        // 7c. Compile body and build return.
        let result = self.compile_expr(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_return(Some(&result)).unwrap();
        }

        // 8. Restore outer state.
        self.type_subst = saved_subst;
        self.loop_stack = saved_loop_stack;
        self.var_type_names = saved_var_types;
        self.variables = saved_vars;
        self.current_fn = saved_fn;
        self.closure_fn_types = saved_cfn;
        self.pending_closure_fn_type = saved_pct;
        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        // 9. In the outer context, allocate and populate the env struct.
        let outer_fn = self.current_fn.unwrap();
        let env_alloca = self.create_entry_alloca(outer_fn, "__closure_env", env_struct_ty.into());
        if !free_vars.is_empty() {
            // Build the env struct by inserting each captured value.
            let mut env_agg = env_struct_ty.get_undef();
            for (i, var_name) in free_vars.iter().enumerate() {
                let slot = self.variables[var_name];
                let val = self
                    .builder
                    .build_load(slot.ty, slot.ptr, var_name)
                    .unwrap();
                env_agg = self
                    .builder
                    .build_insert_value(env_agg, val, i as u32, "__env_field")
                    .unwrap()
                    .into_struct_value();
            }
            self.builder.build_store(env_alloca, env_agg).unwrap();
        }

        // 10. Build the fat-pointer closure struct: { fn_ptr, env_alloca }.
        let fn_ptr = closure_fn.as_global_value().as_pointer_value();
        let fat_ptr_ty = self.closure_value_type();
        let mut fat = fat_ptr_ty.get_undef();
        fat = self
            .builder
            .build_insert_value(fat, fn_ptr, 0, "closure_fn")
            .unwrap()
            .into_struct_value();
        fat = self
            .builder
            .build_insert_value(fat, env_alloca, 1, "closure_env")
            .unwrap()
            .into_struct_value();

        // 11. Stage the LLVM function type for the surrounding let binding.
        self.pending_closure_fn_type = Some(fn_type);

        Ok(fat.into())
    }

    /// Execute an indirect call through a closure fat-pointer variable.
    fn compile_closure_call(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_type = match self.closure_fn_types.get(name).copied() {
            Some(t) => t,
            None => return Ok(self.context.i64_type().const_int(0, false).into()),
        };

        // Load the closure fat pointer value { fn_ptr, env_ptr }.
        let fat_val = self.load_variable(name)?;
        let fat_sv = fat_val.into_struct_value();
        let fn_ptr = self
            .builder
            .build_extract_value(fat_sv, 0, "closure_fn")
            .unwrap()
            .into_pointer_value();
        let env_ptr = self
            .builder
            .build_extract_value(fat_sv, 1, "closure_env")
            .unwrap()
            .into_pointer_value();

        // Build call args: env_ptr first, then user-supplied args.
        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
            vec![BasicMetadataValueEnum::from(env_ptr)];
        for arg in args {
            let val = self.compile_expr(&arg.value)?;
            call_args.push(BasicMetadataValueEnum::from(val));
        }

        let call = self
            .builder
            .build_indirect_call(fn_type, fn_ptr, &call_args, "closure_call")
            .unwrap();

        let basic_val = call.try_as_basic_value();
        if basic_val.is_instruction() {
            Ok(self.context.i64_type().const_int(0, false).into())
        } else {
            Ok(basic_val.unwrap_basic())
        }
    }

    /// Lightweight return-type inference for closure bodies.
    /// Walks the expression shallowly to determine the LLVM type without building IR.
    fn infer_closure_return_type(
        &self,
        expr: &Expr,
        param_types: &HashMap<String, BasicTypeEnum<'ctx>>,
    ) -> BasicTypeEnum<'ctx> {
        match &expr.kind {
            ExprKind::Integer(_, sfx) => self.llvm_int_type_for_suffix(*sfx).into(),
            ExprKind::Float(_, sfx) => self.llvm_float_type_for_suffix(*sfx).into(),
            ExprKind::Bool(_) => self.context.bool_type().into(),
            ExprKind::CharLit(_) => self.context.i32_type().into(),
            ExprKind::StringLit(_) => self.context.ptr_type(AddressSpace::default()).into(),
            ExprKind::Identifier(name) => {
                if let Some(&ty) = param_types.get(name) {
                    return ty;
                }
                if let Some(slot) = self.variables.get(name.as_str()) {
                    return slot.ty;
                }
                self.context.i64_type().into()
            }
            ExprKind::Binary { op, left, right } => match op {
                BinOp::Eq
                | BinOp::NotEq
                | BinOp::Lt
                | BinOp::LtEq
                | BinOp::Gt
                | BinOp::GtEq
                | BinOp::And
                | BinOp::Or => self.context.bool_type().into(),
                _ => {
                    let lt = self.infer_closure_return_type(left, param_types);
                    let rt = self.infer_closure_return_type(right, param_types);
                    if lt.is_float_type() || rt.is_float_type() {
                        self.context.f64_type().into()
                    } else {
                        lt
                    }
                }
            },
            ExprKind::Unary { operand, .. } => self.infer_closure_return_type(operand, param_types),
            ExprKind::MethodCall { method, .. } if method == "cmp" => self
                .enum_layouts
                .get("Ordering")
                .map(|l| BasicTypeEnum::StructType(l.llvm_type))
                .unwrap_or_else(|| {
                    self.context
                        .struct_type(&[self.context.i64_type().into()], false)
                        .into()
                }),
            ExprKind::Cast { ty, .. } => self.llvm_type_for_type_expr(ty),
            ExprKind::Block(block) | ExprKind::Seq(block) => {
                if let Some(final_expr) = &block.final_expr {
                    self.infer_closure_return_type(final_expr, param_types)
                } else {
                    self.context.i64_type().into()
                }
            }
            ExprKind::If {
                then_block,
                else_branch,
                ..
            } => {
                if let Some(else_expr) = else_branch {
                    self.infer_closure_return_type(else_expr, param_types)
                } else if let Some(final_expr) = &then_block.final_expr {
                    self.infer_closure_return_type(final_expr, param_types)
                } else {
                    self.context.i64_type().into()
                }
            }
            ExprKind::Tuple(elems) => {
                let field_types: Vec<BasicTypeEnum<'ctx>> = elems
                    .iter()
                    .map(|e| self.infer_closure_return_type(e, param_types))
                    .collect();
                self.context.struct_type(&field_types, false).into()
            }
            // Calls: look up in module or use i64 fallback.
            ExprKind::Call { callee, args } => {
                if let ExprKind::Identifier(fname) = &callee.kind {
                    if let Some(f) = self.module.get_function(fname) {
                        return f
                            .get_type()
                            .get_return_type()
                            .unwrap_or_else(|| self.context.i64_type().into());
                    }
                }
                // Lowered operator dispatch: `<Primitive>.<op>(args)` —
                // the lowering pass produces these from BinOp/UnaryOp.
                if let ExprKind::Path { segments, .. } = &callee.kind {
                    if segments.len() == 2 {
                        let target = segments[0].as_str();
                        let method = segments[1].as_str();
                        // Eq/Ord methods return bool regardless of operand type.
                        if matches!(method, "eq" | "ne" | "lt" | "le" | "gt" | "ge") {
                            return self.context.bool_type().into();
                        }
                        // Arithmetic, bitwise, shifts, not — return Self.
                        let is_self_returning = matches!(
                            method,
                            "add"
                                | "sub"
                                | "mul"
                                | "div"
                                | "rem"
                                | "neg"
                                | "bitand"
                                | "bitor"
                                | "bitxor"
                                | "shl"
                                | "shr"
                                | "not"
                        );
                        if is_self_returning {
                            return match target {
                                "f32" => self.context.f32_type().into(),
                                "f64" => self.context.f64_type().into(),
                                "bool" => self.context.bool_type().into(),
                                _ => {
                                    // Fall back to inferring from operand if available.
                                    if let Some(arg) = args.first() {
                                        return self
                                            .infer_closure_return_type(&arg.value, param_types);
                                    }
                                    self.context.i64_type().into()
                                }
                            };
                        }
                    }
                }
                self.context.i64_type().into()
            }
            _ => self.context.i64_type().into(),
        }
    }

    /// Collect the names of variables captured by a closure (free variables from outer scope).
    ///
    /// A variable is captured if:
    /// 1. It is referenced in `body`.
    /// 2. It is NOT one of the closure's own parameters.
    /// 3. It is NOT defined by a `let` inside the closure body.
    /// 4. It IS present in the current outer scope (`self.variables`).
    fn collect_closure_free_vars(&self, params: &[ClosureParam], body: &Expr) -> Vec<String> {
        let param_names: HashSet<String> = params
            .iter()
            .flat_map(|p| p.pattern.binding_names())
            .collect();

        let mut refs = HashSet::new();
        let mut inner_defs = HashSet::new();
        self.refs_in_expr(body, &mut refs, &mut inner_defs);

        let mut free: Vec<String> = refs
            .into_iter()
            .filter(|n| !param_names.contains(n) && !inner_defs.contains(n))
            .filter(|n| self.variables.contains_key(n.as_str()))
            .collect();
        free.sort(); // deterministic order
        free
    }

    /// Walk `expr` and collect all identifier references into `refs`,
    /// and all names bound by `let` statements into `defs`.
    fn refs_in_expr(&self, expr: &Expr, refs: &mut HashSet<String>, defs: &mut HashSet<String>) {
        match &expr.kind {
            ExprKind::Identifier(n) => {
                refs.insert(n.clone());
            }
            // `self` inside an impl-method body parses as `SelfValue`,
            // not `Identifier("self")`. Without this arm, an auto-par
            // branch fn whose stmts read `self.X` would not include
            // `self` in its capture set, the env-struct unpack would
            // not bind `self` in the branch fn's `self.variables`, and
            // `load_variable("self")` would error with "Undefined
            // variable 'self'" when the branch body's field access
            // tries to resolve the receiver.
            ExprKind::SelfValue => {
                refs.insert("self".to_string());
            }
            ExprKind::Binary { left, right, .. } => {
                self.refs_in_expr(left, refs, defs);
                self.refs_in_expr(right, refs, defs);
            }
            ExprKind::Unary { operand, .. } => self.refs_in_expr(operand, refs, defs),
            ExprKind::Call { callee, args } => {
                self.refs_in_expr(callee, refs, defs);
                for a in args {
                    self.refs_in_expr(&a.value, refs, defs);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.refs_in_expr(object, refs, defs);
                for a in args {
                    self.refs_in_expr(&a.value, refs, defs);
                }
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.refs_in_expr(condition, refs, defs);
                self.refs_in_block(then_block, refs, defs);
                if let Some(e) = else_branch {
                    self.refs_in_expr(e, refs, defs);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.refs_in_expr(condition, refs, defs);
                self.refs_in_block(body, refs, defs);
            }
            ExprKind::Loop { body, .. } => self.refs_in_block(body, refs, defs),
            ExprKind::Block(block) | ExprKind::Seq(block) => {
                self.refs_in_block(block, refs, defs);
            }
            ExprKind::Return(Some(e)) => self.refs_in_expr(e, refs, defs),
            ExprKind::Return(None) => {}
            ExprKind::Break { value: Some(e), .. } => self.refs_in_expr(e, refs, defs),
            ExprKind::Break { value: None, .. } => {}
            ExprKind::FieldAccess { object, .. } => self.refs_in_expr(object, refs, defs),
            ExprKind::TupleIndex { object, .. } => self.refs_in_expr(object, refs, defs),
            ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
                for e in elems {
                    self.refs_in_expr(e, refs, defs);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.refs_in_expr(&f.value, refs, defs);
                }
            }
            ExprKind::Cast { expr: inner, .. } => self.refs_in_expr(inner, refs, defs),
            ExprKind::Match { scrutinee, arms } => {
                self.refs_in_expr(scrutinee, refs, defs);
                for arm in arms {
                    for name in arm.pattern.binding_names() {
                        defs.insert(name);
                    }
                    self.refs_in_expr(&arm.body, refs, defs);
                }
            }
            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                self.refs_in_expr(iterable, refs, defs);
                for name in pattern.binding_names() {
                    defs.insert(name);
                }
                self.refs_in_block(body, refs, defs);
            }
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.refs_in_expr(value, refs, defs);
                for name in pattern.binding_names() {
                    defs.insert(name);
                }
                self.refs_in_block(then_block, refs, defs);
                if let Some(e) = else_branch {
                    self.refs_in_expr(e, refs, defs);
                }
            }
            ExprKind::Closure { params, body, .. } => {
                // Nested closure: params shadow outer names; body refs are handled recursively
                // but we only care about what escapes into the outer scope.
                let inner_params: HashSet<String> = params
                    .iter()
                    .flat_map(|p| p.pattern.binding_names())
                    .collect();
                let mut inner_refs = HashSet::new();
                let mut inner_inner_defs = HashSet::new();
                self.refs_in_expr(body, &mut inner_refs, &mut inner_inner_defs);
                for r in inner_refs {
                    if !inner_params.contains(&r) && !inner_inner_defs.contains(&r) {
                        refs.insert(r);
                    }
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.refs_in_expr(s, refs, defs);
                }
                if let Some(e) = end {
                    self.refs_in_expr(e, refs, defs);
                }
            }
            ExprKind::InterpolatedStringLit(parts) => {
                for part in parts {
                    if let ParsedInterpolationPart::Expr(inner) = part {
                        self.refs_in_expr(inner, refs, defs);
                    }
                }
            }
            // `a[i]` indexes: walk both the indexed object and the
            // index expr. Without this, an auto-par branch fn whose
            // stmts read `nums[j]` would miss `nums` in its capture
            // set — the env-struct unpack would never bind `nums` in
            // the branch's `self.variables`, and `compile_slice_index`
            // (or `compile_vec_index` / `compile_map_index`) would
            // panic at the `get_data_ptr(name).unwrap()` site when
            // the slice/vec/map registries still report the type
            // (registered in the parent) but the variables table
            // doesn't have the alloca.
            ExprKind::Index { object, index } => {
                self.refs_in_expr(object, refs, defs);
                self.refs_in_expr(index, refs, defs);
            }
            _ => {}
        }
    }

    fn refs_in_block(&self, block: &Block, refs: &mut HashSet<String>, defs: &mut HashSet<String>) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let { pattern, value, .. } | StmtKind::LetElse { pattern, value, .. } => {
                    self.refs_in_expr(value, refs, defs);
                    for name in pattern.binding_names() {
                        defs.insert(name);
                    }
                }
                StmtKind::Expr(e) => self.refs_in_expr(e, refs, defs),
                StmtKind::Assign { target, value } => {
                    self.refs_in_expr(target, refs, defs);
                    self.refs_in_expr(value, refs, defs);
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    self.refs_in_expr(target, refs, defs);
                    self.refs_in_expr(value, refs, defs);
                }
                _ => {}
            }
        }
        if let Some(e) = &block.final_expr {
            self.refs_in_expr(e, refs, defs);
        }
    }

    // ── Generic monomorphization ─────────────────────────────────

    /// Compile a call to a generic function, generating a specialization if needed.
    fn compile_generic_call(
        &mut self,
        name: &str,
        args: &[CallArg],
        explicit_generic_args: Option<&[GenericArg]>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let generic_fn = self.generic_fns[name].clone();

        // Compile argument values so we can infer concrete types.
        let arg_vals: Vec<BasicValueEnum<'ctx>> = args
            .iter()
            .map(|a| self.compile_expr(&a.value))
            .collect::<Result<_, _>>()?;

        // Infer type arguments from the argument value types.
        let mut subst = self.infer_type_args(&generic_fn, &arg_vals);

        // Const generics slice 1b: process explicit generic args. For
        // each formal param the user supplied an explicit arg for,
        // override the inferred type subst (for type params) or
        // populate a parallel const_subst (for const params). The
        // const_subst flows to `mangle_mono_name` so each distinct
        // const-arg tuple produces a distinct mono symbol. Slice 4
        // will collapse this into a single `SubstValue<'ctx>` shape
        // (fork F2) once codegen body lowering needs const-param
        // identifier resolution.
        let mut const_subst: HashMap<String, crate::prelude::ConstValue> = HashMap::new();
        if let (Some(explicit), Some(gp)) = (explicit_generic_args, &generic_fn.generic_params) {
            for (param, arg) in gp.params.iter().zip(explicit.iter()) {
                match arg {
                    GenericArg::Type(t) => {
                        let llvm_ty = self.llvm_type_for_type_expr(t);
                        subst.insert(param.name.clone(), llvm_ty);
                    }
                    GenericArg::Const(e) => {
                        if let Some(cv) = const_value_from_literal_expr(e) {
                            const_subst.insert(param.name.clone(), cv);
                        }
                    }
                }
            }
        }

        // Slice 0.a sub-step 2 — codegen monomorphization-request bound
        // enforcement (defense-in-depth). The typechecker discharges
        // bounds at every call site (`discharge_type_bounds` /
        // `normalize_bounds_into_where_clause`); this hook fires only
        // for paths that reach codegen with a still-unsatisfied bound
        // (a future cross-module path, or a typechecker-internal call
        // that bypassed the discharge). Covers built-in trait names
        // against primitive LLVM types only — user-trait-on-user-type
        // requires an impl-table threading slice that isn't built yet.
        self.verify_bounds_at_codegen(&generic_fn, &subst)?;

        // Mangle a unique name for this specialization (e.g. `max$i64`).
        let mangled = self.mangle_mono_name(name, &generic_fn, &subst, &const_subst);

        // Generate the specialization if we haven't done so yet.
        if !self.generated_monos.contains(&mangled) {
            // Mark as in-progress before recursing to avoid infinite loops.
            self.generated_monos.insert(mangled.clone());

            // Save all per-function codegen state — we're about to compile a
            // different function inline.
            let saved_bb = self.builder.get_insert_block();
            let saved_fn = self.current_fn;
            let saved_vars = std::mem::take(&mut self.variables);
            let saved_var_types = std::mem::take(&mut self.var_type_names);
            let saved_loop_stack = std::mem::take(&mut self.loop_stack);
            let saved_subst = std::mem::replace(&mut self.type_subst, subst.clone());
            // Const generics slice 4: thread the const-arg substitution
            // into the body-lowering pass so `compile_expr Identifier`
            // can resolve const-param refs against it. Parallel to
            // `type_subst`'s save/restore.
            let saved_const_subst = std::mem::replace(&mut self.const_subst, const_subst.clone());

            // Declare then compile the specialization.
            self.declare_mono_function(&generic_fn, &mangled)?;
            self.compile_mono_function(&generic_fn, &mangled)?;

            // Restore state.
            self.const_subst = saved_const_subst;
            self.type_subst = saved_subst;
            self.loop_stack = saved_loop_stack;
            self.var_type_names = saved_var_types;
            self.variables = saved_vars;
            self.current_fn = saved_fn;
            if let Some(bb) = saved_bb {
                self.builder.position_at_end(bb);
            }
        }

        // Call the specialized function.
        let func = match self.module.get_function(&mangled) {
            Some(f) => f,
            None => return Ok(self.context.i64_type().const_int(0, false).into()),
        };

        let compiled_args: Vec<BasicMetadataValueEnum<'ctx>> = arg_vals
            .iter()
            .map(|v| BasicMetadataValueEnum::from(*v))
            .collect();

        let call = self
            .builder
            .build_call(func, &compiled_args, "call")
            .unwrap();

        let basic_val = call.try_as_basic_value();
        if basic_val.is_instruction() {
            Ok(self.context.i64_type().const_int(0, false).into())
        } else {
            Ok(basic_val.unwrap_basic())
        }
    }

    /// Declare the LLVM function for a monomorphized specialization.
    /// `type_subst` must already be populated before calling this.
    fn declare_mono_function(
        &mut self,
        func: &Function,
        mangled: &str,
    ) -> Result<FunctionValue<'ctx>, String> {
        let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = func
            .params
            .iter()
            .map(|p| self.llvm_param_type(p))
            .collect();

        let fn_type = match self.llvm_return_type(&func.return_type) {
            Some(BasicTypeEnum::IntType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::FloatType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::PointerType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::StructType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::ArrayType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::VectorType(t)) => t.fn_type(&param_types, false),
            Some(BasicTypeEnum::ScalableVectorType(_)) | None => {
                self.context.void_type().fn_type(&param_types, false)
            }
        };

        Ok(self.module.add_function(mangled, fn_type, None))
    }

    /// Compile the body of a monomorphized specialization.
    /// `type_subst` must already be populated and per-function state must be fresh.
    fn compile_mono_function(&mut self, func: &Function, mangled: &str) -> Result<(), String> {
        let fn_val = self
            .module
            .get_function(mangled)
            .ok_or_else(|| format!("Mono '{}' not declared", mangled))?;

        self.current_fn = Some(fn_val);
        self.variables.clear();
        self.var_type_names.clear();

        let entry = self.context.append_basic_block(fn_val, "entry");
        self.builder.position_at_end(entry);

        for (i, param) in func.params.iter().enumerate() {
            let param_name = self.param_name(param);
            let param_val = fn_val.get_nth_param(i as u32).unwrap();
            let alloca = self.create_entry_alloca(fn_val, &param_name, param_val.get_type());
            self.builder.build_store(alloca, param_val).unwrap();
            // Track declared type name for struct/enum field resolution.
            if let TypeKind::Path(path) = &param.ty.kind {
                if let Some(type_name) = path.segments.first() {
                    self.var_type_names
                        .insert(param_name.clone(), type_name.clone());
                }
            }
            self.variables.insert(
                param_name,
                VarSlot {
                    ptr: alloca,
                    ty: param_val.get_type(),
                },
            );
        }

        let result = self.compile_block(&func.body)?;

        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            if let Some(val) = result {
                self.builder.build_return(Some(&val)).unwrap();
            } else {
                self.builder.build_return(None).unwrap();
            }
        }

        Ok(())
    }

    /// Infer the type-parameter substitution for a generic function call by
    /// matching each parameter's declared type against the concrete argument type.
    fn infer_type_args(
        &self,
        func: &Function,
        arg_vals: &[BasicValueEnum<'ctx>],
    ) -> HashMap<String, BasicTypeEnum<'ctx>> {
        let mut subst = HashMap::new();
        for (param, val) in func.params.iter().zip(arg_vals.iter()) {
            self.unify_type_expr(&param.ty, val.get_type(), &mut subst);
        }
        subst
    }

    /// Recursively match a declared type expression against a concrete LLVM type,
    /// recording bindings for any unbound type parameters found.
    fn unify_type_expr(
        &self,
        ty: &TypeExpr,
        concrete: BasicTypeEnum<'ctx>,
        subst: &mut HashMap<String, BasicTypeEnum<'ctx>>,
    ) {
        if let TypeKind::Path(path) = &ty.kind {
            if path.segments.len() == 1 && path.generic_args.is_none() {
                let name = &path.segments[0];
                // Treat as a type parameter if it's not a known concrete type.
                if !self.is_known_concrete_type(name) {
                    subst.entry(name.clone()).or_insert(concrete);
                }
            }
            // TODO: unify generic args (e.g. `Vec[T]`) when container types are codegen'd.
        }
    }

    /// Returns true if `name` is a built-in concrete type or a declared struct/enum.
    fn is_known_concrete_type(&self, name: &str) -> bool {
        matches!(
            name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "isize"
                | "usize"
                | "f32"
                | "f64"
                | "bool"
                | "str"
                | "String"
                | "char"
        ) || self.struct_types.contains_key(name)
            || self.enum_layouts.contains_key(name)
    }

    /// Build a mangled name for a specialization, e.g. `max$i64` or `zip$i64$f64`.
    fn mangle_mono_name(
        &self,
        base: &str,
        func: &Function,
        subst: &HashMap<String, BasicTypeEnum<'ctx>>,
        const_subst: &HashMap<String, crate::prelude::ConstValue>,
    ) -> String {
        let params = match &func.generic_params {
            Some(gp) => &gp.params,
            None => return base.to_string(),
        };

        let mut mangled = base.to_string();
        for param in params {
            // Const generics slice 1b: const params take priority over
            // type subst when both maps are populated (the const_subst
            // is keyed by formal name, the type subst doesn't carry
            // const params).
            if param.is_const {
                if let Some(cv) = const_subst.get(&param.name) {
                    mangled.push('$');
                    mangled.push_str(&const_value_to_mangle_str(cv));
                }
            } else if let Some(ty) = subst.get(&param.name) {
                mangled.push('$');
                mangled.push_str(&self.llvm_type_to_mangle_str(*ty));
            }
        }
        mangled
    }

    /// Slice 0.a sub-step 2 — codegen monomorphization-request bound
    /// enforcement.
    ///
    /// Walks both inline-form (`fn f[T: Bound]`) and where-clause
    /// (`fn f[T] where T: Bound`) bounds against the concrete LLVM
    /// substitution. Returns `Err` when a primitive LLVM type
    /// demonstrably fails to satisfy a built-in trait bound (e.g.
    /// `f64` for `Hash` / `Eq` / `Ord`), matching the typechecker's
    /// `type_supports_*` shape on primitives.
    ///
    /// **Scope is intentionally narrow.** The typechecker discharges
    /// bound violations at every call site (`discharge_type_bounds`),
    /// so this hook is purely defense-in-depth for paths that reach
    /// codegen without a typechecker pass (no such path exists in the
    /// single-CU compiler today, but cross-module compilation would
    /// open one). Coverage:
    /// - Built-in traits (`Hash` / `Eq` / `PartialEq` / `Ord` /
    ///   `PartialOrd` / `Display` / `Clone` / `Copy`) checked against
    ///   primitive LLVM types via `llvm_type_satisfies_trait`.
    /// - Non-primitive LLVM types (pointers, structs) and unknown
    ///   trait names fall through permissively — verifying those
    ///   requires plumbing the typechecker's impl table into codegen
    ///   (deferred; tracked as a hard-stop trigger in
    ///   `phase-7-codegen.md § Trait-bounds-at-codegen enforcement`).
    fn verify_bounds_at_codegen(
        &self,
        generic_fn: &Function,
        subst: &HashMap<String, BasicTypeEnum<'ctx>>,
    ) -> Result<(), String> {
        if let Some(gp) = &generic_fn.generic_params {
            for param in &gp.params {
                if param.bounds.is_empty() {
                    continue;
                }
                let Some(concrete) = subst.get(&param.name) else {
                    continue;
                };
                for bound in &param.bounds {
                    let Some(trait_name) = bound.path.last() else {
                        continue;
                    };
                    if !self.llvm_type_satisfies_trait(*concrete, trait_name) {
                        return Err(format!(
                            "trait bound `{}: {}` is not satisfied at monomorphization site for `{}` \
                             (concrete type `{}` does not implement `{}`)",
                            param.name,
                            trait_name,
                            generic_fn.name,
                            self.llvm_type_to_mangle_str(*concrete),
                            trait_name,
                        ));
                    }
                }
            }
        }

        if let Some(wc) = &generic_fn.where_clause {
            for constraint in &wc.constraints {
                let WhereConstraint::TypeBound {
                    type_name, bounds, ..
                } = constraint
                else {
                    continue;
                };
                let Some(concrete) = subst.get(type_name) else {
                    continue;
                };
                for bound in bounds {
                    let Some(trait_name) = bound.path.last() else {
                        continue;
                    };
                    if !self.llvm_type_satisfies_trait(*concrete, trait_name) {
                        return Err(format!(
                            "trait bound `{}: {}` is not satisfied at monomorphization site for `{}` \
                             (concrete type `{}` does not implement `{}`)",
                            type_name,
                            trait_name,
                            generic_fn.name,
                            self.llvm_type_to_mangle_str(*concrete),
                            trait_name,
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// Conservative LLVM-type-vs-built-in-trait predicate used by
    /// `verify_bounds_at_codegen`. Mirrors the typechecker's
    /// `type_supports_*` helpers but operates on `BasicTypeEnum`
    /// instead of `Type`. Permissive on non-primitive shapes
    /// (`PointerType`, `StructType`) and unknown trait names — those
    /// cases are the typechecker's responsibility today; the codegen
    /// hook only catches the unambiguous primitive violations
    /// (f32/f64 failing `Hash` / `Eq` / `Ord`).
    fn llvm_type_satisfies_trait(&self, ty: BasicTypeEnum<'ctx>, trait_name: &str) -> bool {
        match trait_name {
            "Hash" | "Eq" | "Ord" => !matches!(ty, BasicTypeEnum::FloatType(_)),
            "PartialEq" | "PartialOrd" | "Display" | "Clone" | "Copy" => true,
            _ => true,
        }
    }

    /// Produce a stable string token for an LLVM type suitable for name mangling.
    fn llvm_type_to_mangle_str(&self, ty: BasicTypeEnum<'ctx>) -> String {
        match ty {
            BasicTypeEnum::IntType(t) => match t.get_bit_width() {
                1 => "bool".to_string(),
                8 => "i8".to_string(),
                16 => "i16".to_string(),
                32 => "i32".to_string(),
                64 => "i64".to_string(),
                w => format!("i{}", w),
            },
            BasicTypeEnum::FloatType(t) => {
                // Distinguish f32 from f64 by comparing with context-canonical types.
                if t == self.context.f32_type() {
                    "f32".to_string()
                } else {
                    "f64".to_string()
                }
            }
            BasicTypeEnum::PointerType(_) => "ptr".to_string(),
            BasicTypeEnum::StructType(_) => "struct".to_string(),
            _ => "opaque".to_string(),
        }
    }

    // ── Monomorphized Map[K, V] symbol emission (Slice 1) ───────

    /// Byte offsets into the runtime's `#[repr(C)]` `KaracMap`
    /// layout (`runtime/src/map.rs`). Codegen-emitted monomorphized
    /// `Map[K, V]` method symbols load these fields by direct GEP +
    /// load against a `*mut KaracMap` opaque pointer rather than
    /// calling through the type-erased `karac_map_*` runtime
    /// functions. Pinned by the runtime-side unit test
    /// `karac_map_field_offsets_match_codegen` — any drift trips
    /// the runtime test before the binary can diverge.
    const KARAC_MAP_STATUS_OFFSET: u64 = 0;
    const KARAC_MAP_KV_OFFSET: u64 = 8;
    const KARAC_MAP_CAPACITY_OFFSET: u64 = 16;
    const KARAC_MAP_LEN_OFFSET: u64 = 24;
    const KARAC_MAP_TOMBSTONES_OFFSET: u64 = 32;
    /// Bucket status-byte sentinels for the monomorphized probe
    /// loop. Must match the runtime's `BUCKET_EMPTY` /
    /// `BUCKET_OCCUPIED` / `BUCKET_TOMBSTONE` constants in
    /// `runtime/src/map.rs`.
    const BUCKET_EMPTY: u64 = 0;
    const BUCKET_OCCUPIED: u64 = 1;
    const BUCKET_TOMBSTONE: u64 = 2;

    /// Cache key for the monomorphized Map[K, V] symbol family —
    /// `"{key_mangle}_{val_mangle}"` (e.g. `"i64_i64"`). Mirrors the
    /// content-addressed scheme used by `mangle_mono_name` for user
    /// generic fns, expressed in terms of `llvm_type_to_mangle_str`'s
    /// stable token set so distinct K/V tuples never collide.
    fn mono_map_cache_key(
        &self,
        key_ty: BasicTypeEnum<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> String {
        format!(
            "{}_{}",
            self.llvm_type_to_mangle_str(key_ty),
            self.llvm_type_to_mangle_str(val_ty),
        )
    }

    /// Gate predicate: does this K/V tuple route through the
    /// monomorphized Map path? Every tuple that returns `false`
    /// falls through to the erased `karac_map_*` runtime per § 3.6
    /// coexist-during-migration. Slice 5 deletes the erased
    /// fallback entirely.
    ///
    /// Slice 1 shipped `Map[i64, i64]`. Slice 2 adds the `i32`
    /// key family — that covers `Map[char, i64]` (the LeetCode #3
    /// kata's K/V tuple, since `char` lowers to LLVM `i32` per
    /// Slice 2.0) and `Map[i32, i64]` if anyone instantiates it.
    /// Both mangle identically (`i32_i64`) and share a single
    /// mono symbol — the K/V slot layout and FNV-1a-over-4-bytes
    /// hash are byte-identical regardless of which surface name
    /// the user wrote.
    fn should_use_mono_map_for(
        &self,
        key_ty: BasicTypeEnum<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> bool {
        let i32_t = self.context.i32_type();
        let i64_t = self.context.i64_type();
        let key_ok = matches!(key_ty, BasicTypeEnum::IntType(t) if t == i32_t || t == i64_t);
        let val_ok = matches!(val_ty, BasicTypeEnum::IntType(t) if t == i64_t);
        key_ok && val_ok
    }

    /// Lazily emit the monomorphized `Map[K, V]` method-symbol family
    /// for a given K/V tuple and return the cached handles. Each
    /// per-method `FunctionValue` is emitted with `LinkOnceODR`
    /// linkage so cross-crate / cross-TU duplicates collapse at link
    /// time (locked design § 3.2).
    ///
    /// Slice 1a ships **wrapper bodies only**: each mono method
    /// forwards to the corresponding erased `karac_map_*` runtime
    /// function 1:1. The wrapper exists at this slice to validate
    /// emission, mangling, dispatch wiring, and `linkonce_odr`
    /// linkage — `nm | grep karac_map_i64_i64_len | wc -l == 1`
    /// after the slice lands. Slice 1b replaces hot-path bodies
    /// (`insert_old`, `get`) with fully-inlined LLVM (direct i64
    /// hash + icmp eq), unlocking the bench gain.
    fn get_or_emit_map_mono_methods(
        &mut self,
        key_ty: BasicTypeEnum<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> MapMonoMethods<'ctx> {
        let cache_key = self.mono_map_cache_key(key_ty, val_ty);
        if let Some(entry) = self.map_mono_methods.get(&cache_key) {
            return *entry;
        }

        let saved_bb = self.builder.get_insert_block();

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        // len: direct GEP + load against the runtime's `#[repr(C)]`
        // `KaracMap.len` field. Drops the function-pointer indirection
        // and the extern call overhead the erased fallback's
        // `karac_map_len` carried. Offset pinned by the runtime-side
        // `karac_map_field_offsets_match_codegen` unit test.
        let len_name = format!("karac_map_{cache_key}_len");
        let len_fn = match self.module.get_function(&len_name) {
            Some(f) => f,
            None => {
                let len_ty = i64_t.fn_type(&[ptr_ty.into()], false);
                let f = self
                    .module
                    .add_function(&len_name, len_ty, Some(Linkage::LinkOnceODR));
                let entry = self.context.append_basic_block(f, "entry");
                self.builder.position_at_end(entry);
                let map_arg = f.get_nth_param(0).unwrap().into_pointer_value();
                let i8_t = self.context.i8_type();
                let offset = i64_t.const_int(Self::KARAC_MAP_LEN_OFFSET, false);
                let len_field_ptr = unsafe {
                    self.builder
                        .build_in_bounds_gep(i8_t, map_arg, &[offset], "mono.len.field.ptr")
                        .unwrap()
                };
                let len = self
                    .builder
                    .build_load(i64_t, len_field_ptr, "mono.len")
                    .unwrap();
                self.builder.build_return(Some(&len)).unwrap();
                f
            }
        };

        // insert_old: fast path inlines load-factor check, FNV-1a
        // hash (via direct call to the existing `karac_hash_<K>`
        // helper — same hash as the erased fallback so cross-path
        // consistency holds while coexist is in effect), linear
        // probe with empty / tombstone / occupied switch, and
        // inline K-typed icmp eq. Slow path (resize-needed branch
        // and safety fallback for the impossible exhausted-probe
        // case) forwards to `karac_map_insert_old` extern.
        let insert_name = format!("karac_map_{cache_key}_insert_old");
        let insert_old_fn = match self.module.get_function(&insert_name) {
            Some(f) => f,
            None => {
                let bool_t = self.context.bool_type();
                let insert_ty = bool_t.fn_type(
                    &[ptr_ty.into(), key_ty.into(), val_ty.into(), ptr_ty.into()],
                    false,
                );
                let f =
                    self.module
                        .add_function(&insert_name, insert_ty, Some(Linkage::LinkOnceODR));
                self.emit_mono_map_insert_old_body(f, key_ty, val_ty);
                f
            }
        };

        // get: same shape as insert_old's fast path but read-only.
        // No load-factor branch (get never resizes), no tombstone
        // tracking, no fresh-slot writes. Probe loop terminates on
        // EMPTY (return false) or OCCUPIED-with-matching-key (load
        // val, store to out_val, return true). On exhausted probe
        // (would be unreachable under valid resize policy, but
        // guarded for safety) returns false.
        let get_name = format!("karac_map_{cache_key}_get");
        let get_fn = match self.module.get_function(&get_name) {
            Some(f) => f,
            None => {
                let bool_t = self.context.bool_type();
                let get_ty = bool_t.fn_type(&[ptr_ty.into(), key_ty.into(), ptr_ty.into()], false);
                let f = self
                    .module
                    .add_function(&get_name, get_ty, Some(Linkage::LinkOnceODR));
                self.emit_mono_map_get_body(f, key_ty, val_ty);
                f
            }
        };

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        let methods = MapMonoMethods {
            len_fn,
            insert_old_fn,
            get_fn,
        };
        self.map_mono_methods.insert(cache_key, methods);
        methods
    }

    /// Emit the fast-path-inlined body of the monomorphized
    /// `karac_map_<K>_<V>_insert_old` function. The shape mirrors
    /// the runtime's `KaracMap::insert` algorithm
    /// (`runtime/src/map.rs:166`) — load-factor branch first,
    /// then linear probe — but inlines the hash (via direct call
    /// to `karac_hash_<K>`, the same FNV-1a helper the erased
    /// fallback's function-pointer hash dispatches to) and the eq
    /// (direct icmp on the K LLVM type), dropping the function-
    /// pointer indirection that defines the erasure tax.
    ///
    /// Slice 1b emitted this for (i64, i64) only; Slice 2 generalizes
    /// to any (i32 / i64 key) × (i64 val) pair so `Map[char, i64]`
    /// can share the shape — char lowers to LLVM i32 (Slice 2.0).
    ///
    /// On entry the function has signature `i1 (ptr map, K key,
    /// V val, ptr out_old_val)`. On exit, every path terminates
    /// with `ret i1` (the existed bit).
    fn emit_mono_map_insert_old_body(
        &mut self,
        f: FunctionValue<'ctx>,
        key_ty: BasicTypeEnum<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) {
        let i8_t = self.context.i8_type();
        let i64_t = self.context.i64_type();
        let bool_t = self.context.bool_type();
        let key_int_ty = key_ty.into_int_type();
        let val_int_ty = val_ty.into_int_type();
        let key_size = (key_int_ty.get_bit_width() as u64).div_ceil(8);
        let val_size = (val_int_ty.get_bit_width() as u64).div_ceil(8);
        let kv_size_bytes = key_size + val_size;

        let map_arg = f.get_nth_param(0).unwrap().into_pointer_value();
        let key_arg = f.get_nth_param(1).unwrap().into_int_value();
        let val_arg = f.get_nth_param(2).unwrap().into_int_value();
        let out_old_arg = f.get_nth_param(3).unwrap().into_pointer_value();

        // Match the mangle-token used by `mono_map_cache_key` so the
        // helper name aligns with the symbol family. Both `char` (4-
        // byte) and `i32` keys hash via `karac_hash_i32` here even
        // though the erased fallback's stored function-pointer might
        // be `karac_hash_char` — both are FNV-1a over 4 bytes and
        // produce identical output for identical input, so cross-
        // path consistency holds.
        let hash_name = self.llvm_type_to_mangle_str(key_ty);
        let hash_fn = self.emit_hash_fn_for_type(&hash_name, key_ty);

        let entry_bb = self.context.append_basic_block(f, "entry");
        let slow_bb = self.context.append_basic_block(f, "slow_path");
        let fast_bb = self.context.append_basic_block(f, "fast_path");
        let probe_cond_bb = self.context.append_basic_block(f, "probe.cond");
        let probe_body_bb = self.context.append_basic_block(f, "probe.body");
        let case_empty_bb = self.context.append_basic_block(f, "case.empty");
        let case_tomb_check_bb = self.context.append_basic_block(f, "case.check_tomb");
        let case_tomb_bb = self.context.append_basic_block(f, "case.tomb");
        let case_occupied_bb = self.context.append_basic_block(f, "case.occupied");
        let match_found_bb = self.context.append_basic_block(f, "match.found");
        let exhausted_bb = self.context.append_basic_block(f, "exhausted");

        // ── entry: field loads + load-factor check ────────────────
        self.builder.position_at_end(entry_bb);
        let len_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_LEN_OFFSET, false)],
                    "len.p",
                )
                .unwrap()
        };
        let len = self
            .builder
            .build_load(i64_t, len_p, "len")
            .unwrap()
            .into_int_value();
        let tomb_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_TOMBSTONES_OFFSET, false)],
                    "tomb.p",
                )
                .unwrap()
        };
        let tombs = self
            .builder
            .build_load(i64_t, tomb_p, "tombs")
            .unwrap()
            .into_int_value();
        let cap_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_CAPACITY_OFFSET, false)],
                    "cap.p",
                )
                .unwrap()
        };
        let cap = self
            .builder
            .build_load(i64_t, cap_p, "cap")
            .unwrap()
            .into_int_value();

        // Load factor: (len + tombs + 1) * 4 > cap * 3 → resize
        let sum = self.builder.build_int_add(len, tombs, "len+tombs").unwrap();
        let sum1 = self
            .builder
            .build_int_add(sum, i64_t.const_int(1, false), "lt+1")
            .unwrap();
        let lhs = self
            .builder
            .build_int_mul(sum1, i64_t.const_int(4, false), "lhs")
            .unwrap();
        let rhs = self
            .builder
            .build_int_mul(cap, i64_t.const_int(3, false), "rhs")
            .unwrap();
        let need_resize = self
            .builder
            .build_int_compare(IntPredicate::UGT, lhs, rhs, "need_resize")
            .unwrap();
        self.builder
            .build_conditional_branch(need_resize, slow_bb, fast_bb)
            .unwrap();

        // ── slow_path: forward to erased karac_map_insert_old ─────
        self.builder.position_at_end(slow_bb);
        let slow_key_slot = self.builder.build_alloca(key_ty, "slow.key.slot").unwrap();
        let slow_val_slot = self.builder.build_alloca(val_ty, "slow.val.slot").unwrap();
        self.builder.build_store(slow_key_slot, key_arg).unwrap();
        self.builder.build_store(slow_val_slot, val_arg).unwrap();
        let slow_existed = self
            .builder
            .build_call(
                self.karac_map_insert_old_fn,
                &[
                    map_arg.into(),
                    slow_key_slot.into(),
                    slow_val_slot.into(),
                    out_old_arg.into(),
                ],
                "slow.existed",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic();
        self.builder.build_return(Some(&slow_existed)).unwrap();

        // ── fast_path: load status/kv ptrs, inline hash ───────────
        self.builder.position_at_end(fast_bb);
        let status_pp = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_STATUS_OFFSET, false)],
                    "status.pp",
                )
                .unwrap()
        };
        let status_ptr = self
            .builder
            .build_load(
                self.context.ptr_type(AddressSpace::default()),
                status_pp,
                "status",
            )
            .unwrap()
            .into_pointer_value();
        let kv_pp = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_KV_OFFSET, false)],
                    "kv.pp",
                )
                .unwrap()
        };
        let kv_ptr = self
            .builder
            .build_load(self.context.ptr_type(AddressSpace::default()), kv_pp, "kv")
            .unwrap()
            .into_pointer_value();

        // Compute hash via direct call to karac_hash_<K>. Stack-
        // alloca + store + call matches the existing erased path's
        // hash exactly (same FNV-1a basis + prime, same byte order).
        let hash_key_slot = self.builder.build_alloca(key_ty, "hash.key.slot").unwrap();
        self.builder.build_store(hash_key_slot, key_arg).unwrap();
        let hash = self
            .builder
            .build_call(hash_fn, &[hash_key_slot.into()], "hash")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let mask = self
            .builder
            .build_int_sub(cap, i64_t.const_int(1, false), "mask")
            .unwrap();
        let start = self.builder.build_and(hash, mask, "start").unwrap();
        self.builder
            .build_unconditional_branch(probe_cond_bb)
            .unwrap();

        // ── probe.cond: 3-PHI'd state, bound check on i ───────────
        self.builder.position_at_end(probe_cond_bb);
        let i_phi = self.builder.build_phi(i64_t, "i").unwrap();
        let ft_phi = self.builder.build_phi(i64_t, "ft").unwrap();
        let ft_set_phi = self.builder.build_phi(bool_t, "ft_set").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), fast_bb)]);
        ft_phi.add_incoming(&[(&i64_t.const_zero(), fast_bb)]);
        ft_set_phi.add_incoming(&[(&bool_t.const_zero(), fast_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let ft_val = ft_phi.as_basic_value().into_int_value();
        let ft_set_val = ft_set_phi.as_basic_value().into_int_value();
        let bound_done = self
            .builder
            .build_int_compare(IntPredicate::UGE, i_val, cap, "bound.done")
            .unwrap();
        self.builder
            .build_conditional_branch(bound_done, exhausted_bb, probe_body_bb)
            .unwrap();

        // ── probe.body: compute slot, load status, switch ─────────
        self.builder.position_at_end(probe_body_bb);
        let sum_si = self.builder.build_int_add(start, i_val, "sum.si").unwrap();
        let slot = self.builder.build_and(sum_si, mask, "slot").unwrap();
        let status_slot_p = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, status_ptr, &[slot], "status.slot.p")
                .unwrap()
        };
        let status_byte = self
            .builder
            .build_load(i8_t, status_slot_p, "status.byte")
            .unwrap()
            .into_int_value();
        let is_empty = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                status_byte,
                i8_t.const_int(Self::BUCKET_EMPTY, false),
                "is.empty",
            )
            .unwrap();
        self.builder
            .build_conditional_branch(is_empty, case_empty_bb, case_tomb_check_bb)
            .unwrap();

        // ── case.check_tomb: branch tomb vs occupied ──────────────
        self.builder.position_at_end(case_tomb_check_bb);
        let is_tomb = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                status_byte,
                i8_t.const_int(Self::BUCKET_TOMBSTONE, false),
                "is.tomb",
            )
            .unwrap();
        self.builder
            .build_conditional_branch(is_tomb, case_tomb_bb, case_occupied_bb)
            .unwrap();

        // ── case.empty: write fresh entry, possibly at earlier tomb
        self.builder.position_at_end(case_empty_bb);
        let target_slot = self
            .builder
            .build_select(ft_set_val, ft_val, slot, "target.slot")
            .unwrap()
            .into_int_value();
        let kv_size = i64_t.const_int(kv_size_bytes, false);
        let target_off = self
            .builder
            .build_int_mul(target_slot, kv_size, "target.off")
            .unwrap();
        let target_kv_p = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, kv_ptr, &[target_off], "target.kv.p")
                .unwrap()
        };
        self.builder.build_store(target_kv_p, key_arg).unwrap();
        let target_val_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    target_kv_p,
                    &[i64_t.const_int(key_size, false)],
                    "target.val.p",
                )
                .unwrap()
        };
        self.builder.build_store(target_val_p, val_arg).unwrap();
        let target_status_p = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, status_ptr, &[target_slot], "target.status.p")
                .unwrap()
        };
        self.builder
            .build_store(
                target_status_p,
                i8_t.const_int(Self::BUCKET_OCCUPIED, false),
            )
            .unwrap();
        // len += 1
        let new_len = self
            .builder
            .build_int_add(len, i64_t.const_int(1, false), "len.new")
            .unwrap();
        self.builder.build_store(len_p, new_len).unwrap();
        // if ft_set, tombs -= 1
        let tombs_dec = self
            .builder
            .build_int_sub(tombs, i64_t.const_int(1, false), "tombs.dec")
            .unwrap();
        let new_tombs = self
            .builder
            .build_select(ft_set_val, tombs_dec, tombs, "tombs.new")
            .unwrap()
            .into_int_value();
        self.builder.build_store(tomb_p, new_tombs).unwrap();
        self.builder
            .build_return(Some(&bool_t.const_zero()))
            .unwrap();

        // ── case.tomb: remember first tomb, continue probing ─────
        self.builder.position_at_end(case_tomb_bb);
        let new_ft = self
            .builder
            .build_select(ft_set_val, ft_val, slot, "ft.new")
            .unwrap()
            .into_int_value();
        let tomb_i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "i.next.tomb")
            .unwrap();
        i_phi.add_incoming(&[(&tomb_i_next, case_tomb_bb)]);
        ft_phi.add_incoming(&[(&new_ft, case_tomb_bb)]);
        ft_set_phi.add_incoming(&[(&bool_t.const_int(1, false), case_tomb_bb)]);
        self.builder
            .build_unconditional_branch(probe_cond_bb)
            .unwrap();

        // ── case.occupied: eq-check, found vs continue ───────────
        self.builder.position_at_end(case_occupied_bb);
        let slot_off = self
            .builder
            .build_int_mul(slot, kv_size, "slot.off")
            .unwrap();
        let slot_kv_p = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, kv_ptr, &[slot_off], "slot.kv.p")
                .unwrap()
        };
        let slot_key = self
            .builder
            .build_load(key_int_ty, slot_kv_p, "slot.key")
            .unwrap()
            .into_int_value();
        let key_match = self
            .builder
            .build_int_compare(IntPredicate::EQ, slot_key, key_arg, "key.match")
            .unwrap();
        let occ_i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "i.next.occ")
            .unwrap();
        // Pre-build the no-match phi inputs.
        i_phi.add_incoming(&[(&occ_i_next, case_occupied_bb)]);
        ft_phi.add_incoming(&[(&ft_val, case_occupied_bb)]);
        ft_set_phi.add_incoming(&[(&ft_set_val, case_occupied_bb)]);
        self.builder
            .build_conditional_branch(key_match, match_found_bb, probe_cond_bb)
            .unwrap();

        // ── match.found: copy old val out, write new val ─────────
        self.builder.position_at_end(match_found_bb);
        let slot_val_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    slot_kv_p,
                    &[i64_t.const_int(key_size, false)],
                    "slot.val.p",
                )
                .unwrap()
        };
        let old_val = self
            .builder
            .build_load(val_int_ty, slot_val_p, "old.val")
            .unwrap()
            .into_int_value();
        self.builder.build_store(out_old_arg, old_val).unwrap();
        self.builder.build_store(slot_val_p, val_arg).unwrap();
        self.builder
            .build_return(Some(&bool_t.const_int(1, false)))
            .unwrap();

        // ── exhausted: unreachable under correct resize policy,
        //               fall back to erased extern for safety ──────
        self.builder.position_at_end(exhausted_bb);
        let safe_key_slot = self.builder.build_alloca(key_ty, "safe.key.slot").unwrap();
        let safe_val_slot = self.builder.build_alloca(val_ty, "safe.val.slot").unwrap();
        self.builder.build_store(safe_key_slot, key_arg).unwrap();
        self.builder.build_store(safe_val_slot, val_arg).unwrap();
        let safe_existed = self
            .builder
            .build_call(
                self.karac_map_insert_old_fn,
                &[
                    map_arg.into(),
                    safe_key_slot.into(),
                    safe_val_slot.into(),
                    out_old_arg.into(),
                ],
                "safe.existed",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic();
        self.builder.build_return(Some(&safe_existed)).unwrap();
    }

    /// Emit the fast-path-inlined body of the monomorphized
    /// `karac_map_<K>_<V>_get` function. Mirrors `KaracMap::lookup` and
    /// `KaracMap::get` from `runtime/src/map.rs:120` — but inlines hash,
    /// probe, K-typed eq, and the val load on match. No load-factor /
    /// resize branch (get never resizes); no tombstone-tracking PHI
    /// (get doesn't write).
    ///
    /// Slice 1b emitted this for (i64, i64) only; Slice 2 generalizes
    /// to any (i32 / i64 key) × (i64 val) pair so `Map[char, i64]`
    /// shares the shape.
    ///
    /// On entry the function has signature `i1 (ptr map, K key,
    /// ptr out_val)`. Returns true and writes the value through
    /// `out_val` on match; returns false otherwise, leaving
    /// `out_val` untouched.
    fn emit_mono_map_get_body(
        &mut self,
        f: FunctionValue<'ctx>,
        key_ty: BasicTypeEnum<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) {
        let i8_t = self.context.i8_type();
        let i64_t = self.context.i64_type();
        let bool_t = self.context.bool_type();
        let key_int_ty = key_ty.into_int_type();
        let val_int_ty = val_ty.into_int_type();
        let key_size = (key_int_ty.get_bit_width() as u64).div_ceil(8);
        let val_size = (val_int_ty.get_bit_width() as u64).div_ceil(8);
        let kv_size_bytes = key_size + val_size;

        let map_arg = f.get_nth_param(0).unwrap().into_pointer_value();
        let key_arg = f.get_nth_param(1).unwrap().into_int_value();
        let out_val_arg = f.get_nth_param(2).unwrap().into_pointer_value();

        let hash_name = self.llvm_type_to_mangle_str(key_ty);
        let hash_fn = self.emit_hash_fn_for_type(&hash_name, key_ty);

        let entry_bb = self.context.append_basic_block(f, "entry");
        let probe_cond_bb = self.context.append_basic_block(f, "probe.cond");
        let probe_body_bb = self.context.append_basic_block(f, "probe.body");
        let check_occupied_bb = self.context.append_basic_block(f, "check.occupied");
        let eq_check_bb = self.context.append_basic_block(f, "eq.check");
        let match_found_bb = self.context.append_basic_block(f, "match.found");
        let not_found_bb = self.context.append_basic_block(f, "not.found");

        // ── entry: load cap / status / kv, compute hash and start ─
        self.builder.position_at_end(entry_bb);
        let cap_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_CAPACITY_OFFSET, false)],
                    "cap.p",
                )
                .unwrap()
        };
        let cap = self
            .builder
            .build_load(i64_t, cap_p, "cap")
            .unwrap()
            .into_int_value();
        let status_pp = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_STATUS_OFFSET, false)],
                    "status.pp",
                )
                .unwrap()
        };
        let status_ptr = self
            .builder
            .build_load(
                self.context.ptr_type(AddressSpace::default()),
                status_pp,
                "status",
            )
            .unwrap()
            .into_pointer_value();
        let kv_pp = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_arg,
                    &[i64_t.const_int(Self::KARAC_MAP_KV_OFFSET, false)],
                    "kv.pp",
                )
                .unwrap()
        };
        let kv_ptr = self
            .builder
            .build_load(self.context.ptr_type(AddressSpace::default()), kv_pp, "kv")
            .unwrap()
            .into_pointer_value();
        let hash_key_slot = self.builder.build_alloca(key_ty, "hash.key.slot").unwrap();
        self.builder.build_store(hash_key_slot, key_arg).unwrap();
        let hash = self
            .builder
            .build_call(hash_fn, &[hash_key_slot.into()], "hash")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let mask = self
            .builder
            .build_int_sub(cap, i64_t.const_int(1, false), "mask")
            .unwrap();
        let start = self.builder.build_and(hash, mask, "start").unwrap();
        self.builder
            .build_unconditional_branch(probe_cond_bb)
            .unwrap();

        // ── probe.cond: PHI for i; bound-check vs cap ─────────────
        self.builder.position_at_end(probe_cond_bb);
        let i_phi = self.builder.build_phi(i64_t, "i").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), entry_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let bound_done = self
            .builder
            .build_int_compare(IntPredicate::UGE, i_val, cap, "bound.done")
            .unwrap();
        self.builder
            .build_conditional_branch(bound_done, not_found_bb, probe_body_bb)
            .unwrap();

        // ── probe.body: load status, branch on empty ──────────────
        self.builder.position_at_end(probe_body_bb);
        let sum_si = self.builder.build_int_add(start, i_val, "sum.si").unwrap();
        let slot = self.builder.build_and(sum_si, mask, "slot").unwrap();
        let status_slot_p = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, status_ptr, &[slot], "status.slot.p")
                .unwrap()
        };
        let status_byte = self
            .builder
            .build_load(i8_t, status_slot_p, "status.byte")
            .unwrap()
            .into_int_value();
        let is_empty = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                status_byte,
                i8_t.const_int(Self::BUCKET_EMPTY, false),
                "is.empty",
            )
            .unwrap();
        self.builder
            .build_conditional_branch(is_empty, not_found_bb, check_occupied_bb)
            .unwrap();

        // ── check.occupied: tombstone → continue, occupied → eq ──
        self.builder.position_at_end(check_occupied_bb);
        let is_occupied = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                status_byte,
                i8_t.const_int(Self::BUCKET_OCCUPIED, false),
                "is.occupied",
            )
            .unwrap();
        let tomb_i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "i.next.tomb")
            .unwrap();
        // Tombstone path: advance i, branch to probe.cond.
        i_phi.add_incoming(&[(&tomb_i_next, check_occupied_bb)]);
        self.builder
            .build_conditional_branch(is_occupied, eq_check_bb, probe_cond_bb)
            .unwrap();

        // ── eq.check: inline icmp eq on K key ────────────────────
        self.builder.position_at_end(eq_check_bb);
        let kv_size = i64_t.const_int(kv_size_bytes, false);
        let slot_off = self
            .builder
            .build_int_mul(slot, kv_size, "slot.off")
            .unwrap();
        let slot_kv_p = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, kv_ptr, &[slot_off], "slot.kv.p")
                .unwrap()
        };
        let slot_key = self
            .builder
            .build_load(key_int_ty, slot_kv_p, "slot.key")
            .unwrap()
            .into_int_value();
        let key_match = self
            .builder
            .build_int_compare(IntPredicate::EQ, slot_key, key_arg, "key.match")
            .unwrap();
        let nomatch_i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "i.next.nomatch")
            .unwrap();
        i_phi.add_incoming(&[(&nomatch_i_next, eq_check_bb)]);
        self.builder
            .build_conditional_branch(key_match, match_found_bb, probe_cond_bb)
            .unwrap();

        // ── match.found: load val, write out, return true ────────
        self.builder.position_at_end(match_found_bb);
        let slot_val_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    slot_kv_p,
                    &[i64_t.const_int(key_size, false)],
                    "slot.val.p",
                )
                .unwrap()
        };
        let val = self
            .builder
            .build_load(val_int_ty, slot_val_p, "val")
            .unwrap()
            .into_int_value();
        self.builder.build_store(out_val_arg, val).unwrap();
        self.builder
            .build_return(Some(&bool_t.const_int(1, false)))
            .unwrap();

        // ── not.found: return false, out_val untouched ───────────
        self.builder.position_at_end(not_found_bb);
        self.builder
            .build_return(Some(&bool_t.const_zero()))
            .unwrap();
    }

    // ── Helpers ─────────────────────────────────────────────────

    /// Slice B follow-up (2026-05-09) — sub-steps (b)+(d).
    ///
    /// Resolve a `Server.serve(handler)` argument expression to the
    /// LLVM `FunctionValue` of a free fn, or emit a structured
    /// rejection diagnostic when the argument shape isn't a free-fn-
    /// name reference. Closures-with-captures, indirect-call values,
    /// and other identifier-as-value shapes that don't resolve to a
    /// `module.get_function(name)` hit get the same rejection — the
    /// `extern "C" fn(*const KaracHttpRequest, *mut KaracHttpResponse)`
    /// FFI slot only accepts a bare fn pointer (the closure-pair
    /// `{ fn_ptr, env_ptr }` ABI is incompatible at the indirect-call
    /// boundary), so the v1 surface is "free fn or rejection."
    ///
    /// **Sub-step (d) framing.** The diagnostic carries the
    /// `E_CLOSURE_AS_FN_PTR_NOT_YET` code so user-side tooling
    /// (`karac build --json`) can recognize it; the code is emitted
    /// inside the codegen error string rather than registered as a
    /// separate enum variant in `cli.rs` because all codegen errors
    /// flow through the single `error: codegen failed: {e}` path
    /// (see `src/cli.rs:2374`).
    fn resolve_free_fn_for_handler_arg(
        &self,
        arg: &Expr,
    ) -> Result<inkwell::values::FunctionValue<'ctx>, String> {
        match &arg.kind {
            ExprKind::Identifier(name) => {
                // Resolution order mirrors `compile_expr`'s Identifier
                // arm: a local binding shadows; otherwise look up as a
                // free fn registered in the LLVM module. We refuse to
                // accept a local binding even if it would resolve —
                // that path is for closure-fat-pointer values which
                // don't match the FFI slot.
                if self.variables.contains_key(name.as_str()) {
                    return Err(format!(
                        "error[E_CLOSURE_AS_FN_PTR_NOT_YET]: cannot pass local binding `{name}` \
                         as the handler argument to `Server.serve` — only free fn names are \
                         supported in v1. Closures with captures (and other indirect-call \
                         values) cannot match the `extern \"C\" fn(*const Request, *mut \
                         Response)` ABI at the FFI boundary; pass a free fn instead. The \
                         closure-as-`Fn`-arg ABI fix is a separate codegen track."
                    ));
                }
                if let Some(fv) = self.module.get_function(name) {
                    return Ok(fv);
                }
                Err(format!(
                    "error[E_CLOSURE_AS_FN_PTR_NOT_YET]: cannot resolve `{name}` to a free fn \
                     for the handler argument to `Server.serve`. Only free fn names are \
                     supported in v1; closures-with-captures and other identifier shapes \
                     are rejected. Pass a top-level `fn` declaration instead."
                ))
            }
            ExprKind::Closure { .. } => Err(
                "error[E_CLOSURE_AS_FN_PTR_NOT_YET]: closures with captures cannot be \
                 passed where a fn-pointer is expected. The handler argument to \
                 `Server.serve` must be a free fn name (e.g. `Server.serve(addr, handle)`); \
                 the closure-pair `{ fn_ptr, env_ptr }` ABI does not match the FFI \
                 extern's bare-pointer parameter slot. Closure-as-`Fn`-arg is a \
                 separate codegen track."
                    .to_string(),
            ),
            _ => Err(format!(
                "error[E_CLOSURE_AS_FN_PTR_NOT_YET]: handler argument to `Server.serve` \
                 must be a free fn name; got expression shape `{:?}` which is not \
                 supported in v1.",
                std::mem::discriminant(&arg.kind)
            )),
        }
    }

    /// HTTP handler ABI trampoline (2026-05-09).
    ///
    /// Emit (or look up from `http_shim_cache`) a per-handler-fn `extern "C"`
    /// shim that adapts between hyper's FFI signature
    /// (`*const KaracHttpRequest, *mut KaracHttpResponse`) and the user's
    /// value-typed `fn(Request) -> Response`. The shim:
    ///   1. Forwards the request pointer arg as the user fn's `Request`
    ///      param (Request lowers to `ptr` per F2 — opaque-pointer shape
    ///      mirroring `Map[K, V]`).
    ///   2. Calls the user handler.
    ///   3. Extracts `status` from the returned `Response` aggregate, truncates
    ///      to u16, and writes it to the response slot via
    ///      `karac_runtime_http_response_set_status`.
    ///   4. Extracts the `body` String's `(data_ptr, len)` and copies it
    ///      into the response slot via
    ///      `karac_runtime_http_response_set_body`.
    ///   5. Returns void.
    ///
    /// Per-handler caching keeps the IR stable and avoids redundant emission
    /// when one program calls `Server.serve(handle)` multiple times.
    /// Pinned by `tests/codegen.rs::test_server_serve_handler_shim_caches`.
    ///
    /// **Panic semantics (F1).** The shim does nothing special — Kāra's
    /// `emit_panic` is `printf + exit(1)`, so handler panics terminate the
    /// server process. Recovery requires `std.panic` (separate Phase 8 work).
    fn emit_http_handler_shim(
        &mut self,
        handler_fn: inkwell::values::FunctionValue<'ctx>,
    ) -> inkwell::values::FunctionValue<'ctx> {
        let user_name = handler_fn
            .get_name()
            .to_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "handler".to_string());
        if let Some(&cached) = self.http_shim_cache.get(&user_name) {
            return cached;
        }
        let shim_name = format!("_karac_http_shim_{user_name}");
        if let Some(existing) = self.module.get_function(&shim_name) {
            self.http_shim_cache.insert(user_name, existing);
            return existing;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let void_ty = self.context.void_type();
        let i16_ty = self.context.i16_type();
        let i64_ty = self.context.i64_type();
        let shim_ty = void_ty.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let shim = self
            .module
            .add_function(&shim_name, shim_ty, Some(Linkage::External));

        // Save the builder's current cursor; we'll restore after shim emit
        // so the caller (`compile_assoc_call` for `Server.serve`) can keep
        // building the dispatch site's basic block.
        let saved_block = self.builder.get_insert_block();
        let saved_fn = self.current_fn;
        self.current_fn = Some(shim);

        let entry = self.context.append_basic_block(shim, "entry");
        self.builder.position_at_end(entry);

        let req_ptr = shim.get_nth_param(0).unwrap().into_pointer_value();
        let resp_ptr = shim.get_nth_param(1).unwrap().into_pointer_value();

        // Call the user handler. The user fn's signature is `fn(Request) ->
        // Response`; with F2's opaque-ptr Request, the Kāra ABI takes a
        // single `ptr` arg and returns the Response aggregate by value.
        let call = self
            .builder
            .build_call(handler_fn, &[req_ptr.into()], "shim.user.call")
            .unwrap();
        let resp_val = call.try_as_basic_value().unwrap_basic();
        let resp_struct = resp_val.into_struct_value();

        // Response layout: { i64 status, { ptr data, i64 len, i64 cap } body }.
        // Extract status (i64), truncate to i16 (the runtime extern takes u16
        // — the i16/u16 distinction is sign-vs-unsigned only at the source
        // level; the LLVM bit pattern is the same).
        let status_i64 = self
            .builder
            .build_extract_value(resp_struct, 0, "shim.resp.status.i64")
            .unwrap()
            .into_int_value();
        let status_i16 = self
            .builder
            .build_int_truncate(status_i64, i16_ty, "shim.resp.status.i16")
            .unwrap();
        let set_status_fn = self
            .module
            .get_function("karac_runtime_http_response_set_status")
            .expect("karac_runtime_http_response_set_status declared in Codegen::new");
        self.builder
            .build_call(
                set_status_fn,
                &[resp_ptr.into(), status_i16.into()],
                "shim.set_status",
            )
            .unwrap();

        // Extract the body String aggregate, then its data pointer + length.
        let body_struct = self
            .builder
            .build_extract_value(resp_struct, 1, "shim.resp.body")
            .unwrap()
            .into_struct_value();
        let body_data = self
            .builder
            .build_extract_value(body_struct, 0, "shim.resp.body.data")
            .unwrap()
            .into_pointer_value();
        let body_len = self
            .builder
            .build_extract_value(body_struct, 1, "shim.resp.body.len")
            .unwrap()
            .into_int_value();
        // Sign-extend / pass-through to i64 for the runtime call (Kāra's
        // String len is already i64, so this is a no-op for the typical
        // path — the explicit extension keeps us robust if a future
        // String layout uses a narrower len field).
        let body_len_i64 = self
            .builder
            .build_int_z_extend_or_bit_cast(body_len, i64_ty, "shim.resp.body.len.i64")
            .unwrap();
        let set_body_fn = self
            .module
            .get_function("karac_runtime_http_response_set_body")
            .expect("karac_runtime_http_response_set_body declared in Codegen::new");
        self.builder
            .build_call(
                set_body_fn,
                &[resp_ptr.into(), body_data.into(), body_len_i64.into()],
                "shim.set_body",
            )
            .unwrap();

        self.builder.build_return(None).unwrap();

        // Restore cursor.
        self.current_fn = saved_fn;
        if let Some(bb) = saved_block {
            self.builder.position_at_end(bb);
        }

        self.http_shim_cache.insert(user_name, shim);
        shim
    }

    /// HTTP handler ABI trampoline (2026-05-09).
    ///
    /// Compile `req.path()` / `req.method()` for a `Request`-typed local.
    /// The receiver's slot stores the opaque `*const KaracHttpRequest` (F2);
    /// load it, call the matching runtime extern to get a borrowed
    /// `*const c_char`, then copy the bytes into a fresh Kāra `String`
    /// `{ data, len, cap }` so the resulting value owns its buffer
    /// (the runtime drops the request struct after the handler returns,
    /// invalidating the borrowed pointer).
    ///
    /// Pinned by `tests/interpreter.rs::test_server_serve_handler_request_path_returns_owned_string`
    /// (interpreter parity for the owned-String contract) and
    /// `tests/http_server.rs::test_server_serve_handler_reads_path` /
    /// `_reads_method` (end-to-end runtime exercise).
    fn compile_request_string_method(
        &mut self,
        var_name: &str,
        method: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let extern_name = match method {
            "path" => "karac_runtime_http_request_path",
            "method" => "karac_runtime_http_request_method",
            other => {
                return Err(format!(
                    "compile_request_string_method called with unsupported method '{other}'"
                ));
            }
        };
        let slot = self
            .variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("Request var '{var_name}' not bound"))?;
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_ty = self.context.i64_type();

        // Load the request pointer from the local's alloca.
        let req_ptr = self
            .builder
            .build_load(slot.ty, slot.ptr, &format!("{var_name}.req.load"))
            .unwrap()
            .into_pointer_value();

        let extern_fn = self
            .module
            .get_function(extern_name)
            .unwrap_or_else(|| panic!("{extern_name} declared in Codegen::new"));
        let cstr_ptr = self
            .builder
            .build_call(extern_fn, &[req_ptr.into()], &format!("req.{method}.cstr"))
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // strlen(cstr_ptr) → i64.
        let strlen_fn = self
            .module
            .get_function("strlen")
            .expect("strlen declared in Codegen::new");
        let len_val = self
            .builder
            .build_call(strlen_fn, &[cstr_ptr.into()], &format!("req.{method}.len"))
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        // strlen returns size_t (i64 on 64-bit); ensure i64.
        let len_i64 = self
            .builder
            .build_int_z_extend_or_bit_cast(len_val, i64_ty, &format!("req.{method}.len.i64"))
            .unwrap();

        // Allocate len bytes (handle len==0 by passing 0 — malloc(0) is
        // implementation-defined but Vec/String elsewhere uses null for
        // empty buffers; mirror that here for consistency).
        let zero = i64_ty.const_zero();
        let is_zero = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                len_i64,
                zero,
                &format!("req.{method}.is_empty"),
            )
            .unwrap();
        let fn_val = self
            .current_fn
            .ok_or_else(|| "Request method called outside fn".to_string())?;
        let alloc_bb = self
            .context
            .append_basic_block(fn_val, &format!("req.{method}.alloc"));
        let empty_bb = self
            .context
            .append_basic_block(fn_val, &format!("req.{method}.empty"));
        let cont_bb = self
            .context
            .append_basic_block(fn_val, &format!("req.{method}.cont"));

        // Pre-branch alloca for the resulting (data, len, cap) buffer ptr.
        let buf_slot = self.create_entry_alloca(fn_val, "req.str.buf", ptr_ty.into());

        self.builder
            .build_conditional_branch(is_zero, empty_bb, alloc_bb)
            .unwrap();

        // Empty path: store null buffer.
        self.builder.position_at_end(empty_bb);
        self.builder
            .build_store(buf_slot, ptr_ty.const_null())
            .unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();

        // Non-empty: malloc + memcpy.
        self.builder.position_at_end(alloc_bb);
        let buf = self
            .builder
            .build_call(
                self.malloc_fn,
                &[len_i64.into()],
                &format!("req.{method}.buf"),
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        self.builder
            .build_memcpy(buf, 1, cstr_ptr, 1, len_i64)
            .unwrap();
        self.builder.build_store(buf_slot, buf).unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();

        // Cont: assemble the String aggregate.
        self.builder.position_at_end(cont_bb);
        let data = self
            .builder
            .build_load(ptr_ty, buf_slot, "req.str.data")
            .unwrap()
            .into_pointer_value();
        let str_ty = self.vec_struct_type();
        let mut str_val: BasicValueEnum<'ctx> = str_ty.get_undef().into();
        str_val = self
            .builder
            .build_insert_value(str_val.into_struct_value(), data, 0, "req.str.data.ins")
            .unwrap()
            .into_struct_value()
            .into();
        str_val = self
            .builder
            .build_insert_value(str_val.into_struct_value(), len_i64, 1, "req.str.len.ins")
            .unwrap()
            .into_struct_value()
            .into();
        str_val = self
            .builder
            .build_insert_value(str_val.into_struct_value(), len_i64, 2, "req.str.cap.ins")
            .unwrap()
            .into_struct_value()
            .into();
        Ok(str_val)
    }

    fn load_variable(&self, name: &str) -> Result<BasicValueEnum<'ctx>, String> {
        if let Some(slot) = self.variables.get(name) {
            // RC-fallback: the alloca holds a heap ptr → {i64 rc, T value}; load T from field 1.
            if let Some(&heap_type) = self.rc_fallback_heap_types.get(name) {
                let ptr_ty = self.context.ptr_type(AddressSpace::default());
                let heap_ptr = self
                    .builder
                    .build_load(ptr_ty, slot.ptr, &format!("{}.rcptr", name))
                    .unwrap()
                    .into_pointer_value();
                let val_ty = heap_type.get_field_type_at_index(1).unwrap();
                let val_field = self
                    .builder
                    .build_struct_gep(heap_type, heap_ptr, 1, &format!("{}.rcval", name))
                    .unwrap();
                return Ok(self.builder.build_load(val_ty, val_field, name).unwrap());
            }
            let val = self.builder.build_load(slot.ty, slot.ptr, name).unwrap();
            // For ref params, the loaded value is a pointer — dereference to get the actual value.
            if let Some(&inner_ty) = self.ref_params.get(name) {
                let ptr = val.into_pointer_value();
                Ok(self
                    .builder
                    .build_load(inner_ty, ptr, &format!("{}.deref", name))
                    .unwrap())
            } else {
                Ok(val)
            }
        } else {
            Err(format!("Undefined variable '{}'", name))
        }
    }

    /// For a variable that may be a ref param, return a pointer to the underlying data.
    /// Owned: returns the alloca directly (alloca holds the struct).
    /// Ref: loads the pointer from alloca then returns it (pointer to the caller's struct).
    fn get_data_ptr(&self, name: &str) -> Option<PointerValue<'ctx>> {
        let slot = self.variables.get(name)?;
        if self.ref_params.contains_key(name) {
            // Ref param: alloca holds a ptr → load it.
            let ptr_ty = self.context.ptr_type(AddressSpace::default());
            Some(
                self.builder
                    .build_load(ptr_ty, slot.ptr, &format!("{}.ref.ptr", name))
                    .unwrap()
                    .into_pointer_value(),
            )
        } else {
            // Owned: alloca IS the struct pointer.
            Some(slot.ptr)
        }
    }

    fn create_entry_alloca(
        &self,
        fn_val: FunctionValue<'ctx>,
        name: &str,
        ty: BasicTypeEnum<'ctx>,
    ) -> PointerValue<'ctx> {
        let entry = fn_val.get_first_basic_block().unwrap();
        let b = self.context.create_builder();
        match entry.get_first_instruction() {
            Some(inst) => b.position_before(&inst),
            None => b.position_at_end(entry),
        }
        b.build_alloca(ty, name).unwrap()
    }

    fn param_name(&self, param: &Param) -> String {
        match &param.pattern.kind {
            PatternKind::Binding(name) => name.clone(),
            _ => "_".to_string(),
        }
    }
}
