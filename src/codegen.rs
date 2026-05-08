//! LLVM code generation for Kāra programs.
//!
//! Compiles the AST to LLVM IR, then to native object files.
//! Uses the `inkwell` crate for LLVM bindings.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Linkage;
use inkwell::module::Module;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType, StructType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FunctionValue, GlobalValue, IntValue, PointerValue,
};
use inkwell::{
    AddressSpace, AtomicOrdering, AtomicRMWBinOp, FloatPredicate, IntPredicate, OptimizationLevel,
};

use crate::ast::*;
use crate::concurrency::{ConcurrencyAnalysis, FunctionConcurrency, ParallelGroup};
use crate::ownership::OwnershipCheckResult;
use crate::token::{FloatSuffix, IntSuffix, Span};

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
    target_machine
        .write_to_file(&cg.module, FileType::Object, Path::new(output_path))
        .map_err(|e| format!("Failed to write object file: {}", e))
}

/// Link an object file into an executable using the system C compiler.
///
/// Also statically links the Kāra runtime library (`libkarac_runtime.a`) and
/// pulls in pthread. The runtime path is resolved from (in order):
///   1. `KARAC_RUNTIME` env var (absolute path to `libkarac_runtime.a`).
///   2. Installed distribution: `<karac-binary-dir>/../lib/libkarac_runtime.a`.
///   3. Development fallback: `<workspace>/target/release/libkarac_runtime.a`.
///
/// See design.md § Runtime Distribution.
pub fn link_executable(obj_path: &str, exe_path: &str) -> Result<(), String> {
    link_executable_impl(obj_path, exe_path, &[])
}

/// Link like [`link_executable`], but prepend extra flags to the `cc` invocation
/// (e.g. `-fsanitize=address`). Used by the memory-behavior E2E test harness to
/// run Kāra-compiled binaries under AddressSanitizer.
///
/// ASAN/LSAN interpose libc `malloc`/`free` globally, so the statically linked
/// `libkarac_runtime.a` does not need to be rebuilt with sanitizer flags for
/// leak detection from Kāra-emitted IR to work. UAF detection *inside* runtime
/// code would require an instrumented runtime build; that is out of scope for
/// this harness, which focuses on codegen-emitted heap operations.
pub fn link_executable_with_sanitizer(
    obj_path: &str,
    exe_path: &str,
    sanitizer_flags: &[&str],
) -> Result<(), String> {
    link_executable_impl(obj_path, exe_path, sanitizer_flags)
}

fn link_executable_impl(
    obj_path: &str,
    exe_path: &str,
    extra_cc_args: &[&str],
) -> Result<(), String> {
    let runtime_path = resolve_runtime_path()?;
    let mut cmd = std::process::Command::new("cc");
    for arg in extra_cc_args {
        cmd.arg(arg);
    }
    cmd.args([
        obj_path,
        &runtime_path,
        "-o",
        exe_path,
        "-lm",
        "-lpthread",
        "-ldl",
    ]);
    let output = cmd
        .output()
        .map_err(|e| format!("Failed to invoke linker: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Linker failed: {}", stderr));
    }

    // Binary-size phase 1: strip non-global symbols from the linked
    // executable. `strip -x` keeps the global symbol table intact (so the
    // `karac_*` runtime entry points the program actually calls remain
    // resolvable, and ASAN-instrumented builds keep the `__asan_*`
    // globals they need at runtime) and drops local debug symbols only.
    // Skipped when sanitizer flags are passed: ASAN's stack-trace
    // symbolication walks local symbol tables for function-name lookup,
    // and stripping them turns "leak in karac_par_run+0x42" into
    // "leak in <unknown>+0x42" — the sanitizer harness keeps the full
    // symbol table for diagnostic legibility. Unix-only at v1; Windows
    // toolchains lack a drop-in `strip` equivalent.
    //
    // `strip` failures are non-fatal — the executable already exists and
    // works; we just lose the size-reduction benefit on this specific
    // build. Print a stderr note rather than failing the codegen path so
    // hosts without `strip` (rare on macOS/Linux) keep producing
    // working binaries.
    if cfg!(unix) && !is_sanitizer_link(extra_cc_args) {
        let strip_status = std::process::Command::new("strip")
            .args(["-x", exe_path])
            .output();
        match strip_status {
            Ok(o) if !o.status.success() => {
                eprintln!(
                    "warning: `strip -x {exe_path}` failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Err(e) => {
                eprintln!("warning: failed to invoke `strip`: {e}");
            }
            _ => {}
        }
    }
    Ok(())
}

/// True if any extra cc flag enables a sanitizer instrumentation runtime.
/// Sanitizer runtimes rely on local symbol tables for stack-trace
/// symbolication, so stripped sanitizer binaries print
/// `<unknown>+0xN` frames in their reports — keep symbols on those
/// builds.
fn is_sanitizer_link(extra_cc_args: &[&str]) -> bool {
    extra_cc_args.iter().any(|a| a.starts_with("-fsanitize"))
}

fn resolve_runtime_path() -> Result<String, String> {
    if let Ok(p) = std::env::var("KARAC_RUNTIME") {
        if std::path::Path::new(&p).exists() {
            return Ok(p);
        }
        return Err(format!("KARAC_RUNTIME set to {p} but file does not exist"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            let installed = bin_dir.join("../lib/libkarac_runtime.a");
            if installed.exists() {
                return Ok(installed.to_string_lossy().into_owned());
            }
        }
    }
    let dev =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/release/libkarac_runtime.a");
    if dev.exists() {
        return Ok(dev.to_string_lossy().into_owned());
    }
    Err(
        "libkarac_runtime.a not found; set KARAC_RUNTIME or build the runtime crate (`cargo build -p karac-runtime --release`)".to_string(),
    )
}

fn create_target_machine() -> Result<TargetMachine, String> {
    Target::initialize_native(&InitializationConfig::default())
        .map_err(|e| format!("Failed to initialize native target: {}", e))?;

    let triple = TargetMachine::get_default_triple();
    let target =
        Target::from_triple(&triple).map_err(|e| format!("Failed to get target: {}", e))?;

    target
        .create_target_machine(
            &triple,
            "generic",
            "",
            OptimizationLevel::Default,
            RelocMode::Default,
            CodeModel::Default,
        )
        .ok_or_else(|| "Failed to create target machine".to_string())
}

/// Read the `KARAC_RUNTIME_DEBUG_METADATA` env var to decide whether
/// `KARAC_SPAWN_SITES` (and friends) emit populated. Slice 3 of the
/// Debugger Contract; see `Codegen::runtime_debug_metadata_enabled` for
/// the field doc and `phase-8-stdlib-floor.md` § "Auto-Concurrency
/// Codegen — Debugger Contract slice 3" for the spec.
///
/// - `Ok("0")` → `false` (gate explicitly off).
/// - `Ok(_)`   → `true` (any other value, including empty).
/// - `Err(_)`  → `true` (dev default; profile-aware defaults land in
///   Phase 8.5 Track 2).
fn read_runtime_debug_metadata_env() -> bool {
    !matches!(std::env::var("KARAC_RUNTIME_DEBUG_METADATA"), Ok(v) if v == "0")
}

/// Read the `KARAC_AUTO_PAR` env var to decide whether
/// `compile_function_body` dispatches non-trivial parallel groups to
/// `emit_par_run` (auto-par codegen) or falls back to plain sequential
/// `compile_block`. Slice 6 of the Auto-Concurrency Codegen track;
/// mirrors slice 3's `read_runtime_debug_metadata_env` shape exactly.
/// See `phase-8-stdlib-floor.md` § "Auto-Concurrency Codegen —
/// Parallax-lite Workload" for the spec.
///
/// - `Ok("0")` → `false` (gate explicitly off — sequential codegen).
/// - `Ok(_)`   → `true` (any other value, including empty).
/// - `Err(_)`  → `true` (dev default — auto-par on by default; the
///   user-facing `--sequential` CLI flag is a Phase 8.5 Track 2
///   deliverable when the profile system ships).
///
/// Returns `true` iff auto-par dispatch is enabled. The
/// `Codegen::auto_par_disabled` field is `!return_value` so the
/// compile-time check reads naturally as `if self.auto_par_disabled`.
fn read_auto_par_env() -> bool {
    !matches!(std::env::var("KARAC_AUTO_PAR"), Ok(v) if v == "0")
}

// ── Variable slot: pointer + LLVM type for typed loads ─────────

#[derive(Clone, Copy)]
struct VarSlot<'ctx> {
    ptr: PointerValue<'ctx>,
    ty: BasicTypeEnum<'ctx>,
}

// ── Shared type (RC) layout ────────────────────────────────────

/// Metadata for a `shared struct` or `shared enum` that is heap-allocated with RC.
/// Heap layout for structs: `{ i64 refcount, field0, field1, … }`
/// Heap layout for enums:   `{ i64 refcount, i64 tag, i64 word0, … }`
#[derive(Clone)]
struct SharedTypeInfo<'ctx> {
    /// The LLVM struct type for the heap object (includes refcount header).
    heap_type: StructType<'ctx>,
    /// Field names in declaration order (structs only; empty for enums).
    #[allow(dead_code)]
    field_names: Vec<String>,
    /// true if this is a shared enum (vs shared struct).
    is_enum: bool,
}

// ── Enum variant layout ─────────────────────────────────────────

/// Tracks how an enum is laid out in LLVM IR as a tagged union.
/// Representation: `{ i64 tag, i64 word_0, ..., i64 word_N }`.
/// All payload words are stored as i64 (signed-extended / reinterpreted).
#[derive(Clone)]
struct EnumLayout<'ctx> {
    /// The LLVM struct type for all instances of this enum.
    llvm_type: StructType<'ctx>,
    /// variant name → discriminant tag (0, 1, 2, …)
    tags: HashMap<String, u64>,
    /// variant name → number of payload word fields
    field_counts: HashMap<String, usize>,
}

// ── SoA layout metadata ─────────────────────────────────────────

/// Metadata for a single group in a SoA layout.
#[derive(Clone, Debug)]
struct SoaGroup {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    fields: Vec<String>,
    /// Index of each field in the original struct's field list.
    field_indices: Vec<usize>,
    #[allow(dead_code)]
    elem_type: Option<StructType<'static>>,
    /// Optional `align(N)` — N is a power-of-two byte alignment for the group's backing array.
    align: Option<u32>,
    #[allow(dead_code)]
    is_cold: bool,
}

/// Full SoA layout for a named collection.
#[derive(Clone, Debug)]
struct SoaLayout {
    #[allow(dead_code)]
    name: String,
    /// Element struct name (e.g., "Entity").
    struct_name: String,
    /// Hot groups in declaration order (excludes the cold group).
    groups: Vec<SoaGroup>,
    /// Optional cold group (separate allocation, appended after all hot group pointers).
    cold_group: Option<SoaGroup>,
    /// Number of hot groups (including implicit trailing group for unassigned fields).
    /// Does NOT include the cold group — the cold pointer is always last in the struct.
    num_groups: usize,
}

// ── Scope cleanup action ────────────────────────────────────────

/// Tagged kind for per-scope destructor actions emitted at scope exit.
/// The `scope_cleanup_actions` stack holds one `Vec` per scope frame;
/// each frame accumulates these in push order and drains in reverse.
enum CleanupAction<'ctx> {
    /// Decrement the refcount of a `shared struct` value.
    RcDec {
        /// Variable name — used to reload the current pointer value in case
        /// the binding was reassigned after the track call.
        name: String,
        /// Fallback pointer if the variable is no longer in scope.
        ptr: PointerValue<'ctx>,
        /// LLVM struct type of the heap-allocated RC object.
        heap_type: StructType<'ctx>,
    },
    /// Free the heap buffer of an owned `Vec[T]` or `String`.
    FreeVecBuffer {
        /// Alloca pointer of the Vec/String struct (`{ptr, len, cap}`).
        vec_alloca: PointerValue<'ctx>,
    },
    /// Free an owned `Map[K,V]` handle via `karac_map_free`.
    FreeMapHandle {
        /// Alloca that holds the opaque map ptr.
        map_alloca: PointerValue<'ctx>,
    },
}

/// Per-element predicate driving `emit_set_op_iter` (`Set.union` /
/// `intersection` / `difference` codegen). `Always` means insert every
/// element; the other two consult `karac_map_contains` against the named
/// other-set handle and either insert on hit or on miss.
#[derive(Clone, Copy)]
enum SetOpFilter<'ctx> {
    Always,
    ContainsIn(PointerValue<'ctx>),
    NotContainsIn(PointerValue<'ctx>),
}

// ── Loop frame: break / continue targets ───────────────────────

#[derive(Clone, Copy)]
struct LoopFrame<'ctx> {
    /// Block to branch to on `continue`
    continue_bb: BasicBlock<'ctx>,
    /// Block to branch to on `break` (loop exit)
    break_bb: BasicBlock<'ctx>,
    /// Optional alloca for `break value`
    result_slot: Option<PointerValue<'ctx>>,
}

// ── Spawn-site metadata (Debugger Contract slice 3) ────────────

/// One row of the `KARAC_SPAWN_SITES` metadata table.
///
/// A `SpawnSiteId` (the `id` field) is minted per `par {}` block during
/// codegen — both explicit `par {}` blocks (`compile_par_block`) and
/// compiler-inferred parallel groups (`compile_function_body`'s auto-par
/// dispatch) flow through `emit_par_run`, which calls
/// `Codegen::record_spawn_site` to push a record into `Codegen::spawn_sites`.
/// The collected records are emitted as a module-scope global at the
/// end of compilation by `emit_spawn_sites_metadata`.
///
/// See `design.md § AI-First Compiler Interface > Debugger Contract` for
/// the four-part contract this is the foundation of:
///
/// - slice 3 (this entry) — produces `KARAC_SPAWN_SITES` + `_LEN` + `_ENABLED`.
/// - slice 4 — references these IDs in worker-frame metadata
///   (parent-frame ref + await-chain pointer).
/// - slice 5 — exposes the table to Kāra-callable code via
///   `std.runtime::list_par_blocks()` / `has_debug_metadata()`, reading
///   `KARAC_SPAWN_SITES` + `_LEN` + `_ENABLED` directly through external
///   linkage.
/// - the still-future `std.panic` crash report
///   (`design.md § Crash Report Format`) reads them for the
///   `parallel_context` field.
struct SpawnSiteRecord {
    /// Stable per-binary `SpawnSiteId`. Equal to the `par_counter` value
    /// at the time the record was minted; the same value is used to name
    /// the par-branch functions (`__par_branch_<id>_<i>`).
    id: u32,
    /// Source filename. Empty when `Codegen::source_filename` was not
    /// threaded in (most tests, ad-hoc IR dumps).
    file: String,
    /// 1-indexed line of the par-block keyword (or first stmt of an
    /// inferred group), per `crate::byte_offset_to_line_col`.
    line: u32,
    /// 1-indexed column of the par-block keyword (or first stmt of an
    /// inferred group), per `crate::byte_offset_to_line_col`.
    col: u32,
    /// Static branch count (number of stmts in the block at codegen
    /// time). `None` would indicate "unknown"; v1's runtime spawns one
    /// OS thread per branch (`karac_par_run` in `runtime/src/lib.rs`),
    /// so the count is statically the stmt count and the field is
    /// always `Some(stmts.len() as u32)` today. Recorded as `Option`
    /// to lock the field shape now — when work-stealing or
    /// thread-pool-bounded execution lands (Phase 6.2 / 6.3), the
    /// static count loses meaning and slice 4 / 5's introspection
    /// surface will need to choose between "branches in source" (this
    /// field) and a separate dynamic "currently active workers"
    /// surface from the runtime. Defer the decision; the field name
    /// captures the static-source intent.
    worker_count: Option<u32>,
}

// ── Codegen ────────────────────────────────────────────────────

struct Codegen<'ctx> {
    context: &'ctx Context,
    module: Module<'ctx>,
    builder: Builder<'ctx>,
    /// Maps variable name → (alloca pointer, value type).
    variables: HashMap<String, VarSlot<'ctx>>,
    /// Maps variable name → Kāra type name (for struct/enum field resolution).
    var_type_names: HashMap<String, String>,
    current_fn: Option<FunctionValue<'ctx>>,
    printf_fn: FunctionValue<'ctx>,
    /// `int snprintf(char* buf, size_t n, const char* fmt, ...)` — used by f-string
    /// codegen to convert integers and floats to their decimal string forms.
    snprintf_fn: FunctionValue<'ctx>,
    /// LLVM struct types for Kāra structs (struct name → LLVM type).
    struct_types: HashMap<String, StructType<'ctx>>,
    /// Field names in declaration order (struct name → field names).
    struct_field_names: HashMap<String, Vec<String>>,
    /// Enum layouts for tagged-union codegen (enum name → layout).
    enum_layouts: HashMap<String, EnumLayout<'ctx>>,
    /// Nested loop stack — innermost frame is last.
    loop_stack: Vec<LoopFrame<'ctx>>,
    // ── Generic monomorphization ──────────────────────────────────
    /// Generic function AST nodes keyed by name. Not compiled until instantiated.
    generic_fns: HashMap<String, Function>,
    /// Already-generated monomorphizations (mangled name → done). Prevents duplicate codegen.
    generated_monos: HashSet<String>,
    /// Active type-parameter substitution during a monomorphization pass.
    /// Maps generic param name (e.g. `"T"`) → concrete LLVM type.
    type_subst: HashMap<String, BasicTypeEnum<'ctx>>,
    // ── Closure compilation ────────────────────────────────────────
    /// Monotonic counter used to generate unique closure function names.
    closure_counter: u32,
    /// Maps local variable names that hold closure fat-pointers to their LLVM function type.
    /// Required for indirect calls: `build_indirect_call` needs the callee's function type.
    closure_fn_types: HashMap<String, FunctionType<'ctx>>,
    /// Staging slot — set by `compile_closure` so the surrounding `let` binding can record
    /// the function type under the newly bound name.
    pending_closure_fn_type: Option<FunctionType<'ctx>>,
    // ── Shared types (RC) ─────────────────────────────────────────
    /// Shared type metadata (struct/enum name → heap layout info).
    shared_types: HashMap<String, SharedTypeInfo<'ctx>>,
    /// malloc function for heap allocation.
    malloc_fn: FunctionValue<'ctx>,
    /// free function for heap deallocation.
    free_fn: FunctionValue<'ctx>,
    /// exit function for runtime panics.
    exit_fn: FunctionValue<'ctx>,
    /// memcmp for string comparison.
    memcmp_fn: FunctionValue<'ctx>,
    /// Per-variable Vec element type tracking (variable name → element LLVM type).
    vec_elem_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Slice element type tracking (variable name → element LLVM type).
    /// Entries only exist for values whose LLVM representation is the
    /// 2-field slice struct `{ptr, i64}`; used to dispatch indexing and
    /// iteration lowering.
    slice_elem_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Variables that are ref parameters (name → inner LLVM type for dereferencing).
    ref_params: HashMap<String, BasicTypeEnum<'ctx>>,
    /// SoA layout metadata (layout name → SoaLayout).
    soa_layouts: HashMap<String, SoaLayout>,
    /// Function parameter ref-ness (function name → vec of is_ref per param).
    fn_param_ref: HashMap<String, Vec<bool>>,
    /// Function parameter slice element type (function name → per-param
    /// Some(elem_ty) if that param is Slice[T] / mut Slice[T], else None).
    /// Used at call sites to emit Array → Slice and Vec → Slice coercions.
    fn_param_slice_elem: HashMap<String, Vec<Option<BasicTypeEnum<'ctx>>>>,
    /// Per-scope cleanup stack.  Each inner `Vec` is one scope frame; entries
    /// are emitted in reverse-push order at scope exit (innermost first).
    scope_cleanup_actions: Vec<Vec<CleanupAction<'ctx>>>,
    /// Cross-error-type conversion targets at `?` sites — populated from
    /// `Program.question_conversions` (set by the lowering pass from the
    /// typechecker's `question_conversions` map). Key: `(span.offset,
    /// span.length)` of the `?` expression. Value: target type name (e.g.
    /// `"AppError"`). When present, `compile_question` emits `Target.from(e)`
    /// against the inner err payload before the propagation early-return.
    question_conversions: HashMap<(usize, usize), String>,
    /// Per-callee effectfulness side-table — populated from
    /// `Program.callee_effectful` (set by the cli pipeline after effectcheck).
    /// Key: callable's canonical name (free fn `name`, assoc/method
    /// `Type.method`). Value: `true` iff the callee carries any of
    /// `reads`/`writes`/`sends`/`receives`. Read by `emit_branch_cancel_check`
    /// to skip the cooperative cancel atomic load when we can prove the
    /// callee is non-observably-effectful. Absent callees are treated as
    /// potentially effectful (fall back to the conservative MVP behavior).
    callee_effectful: HashMap<String, bool>,
    /// Per-method-call → `Type.method` callee key side-table — populated
    /// from `Program.method_callee_types` (set by the lowering pass from
    /// `TypeCheckResult.expr_types`). Key: `(span.offset, span.length)` of
    /// the `MethodCall` expression. Value: canonical `Type.method` string
    /// usable as a lookup into `callee_effectful`. Lets
    /// `compile_method_call` apply the same narrowing that `compile_call`
    /// applies to free-function and `Type.assoc` calls.
    method_callee_types: HashMap<(usize, usize), String>,
    /// Per-pattern-binding surface type table — populated from
    /// `Program.pattern_binding_types` (set by the lowering pass from
    /// `TypeCheckResult.pattern_binding_types`). Key: pattern's
    /// `(span.offset, span.length)`. Value: canonical type name (e.g.
    /// `"MyError"`). Used in `bind_pattern_values` to reconstitute struct
    /// payloads from the i64 word when the surface binding type is a
    /// struct, so subsequent `.field` access dispatches through the right
    /// struct shape.
    pattern_binding_types: HashMap<(usize, usize), String>,
    /// Top-level `const NAME: T = value` declarations, populated by
    /// `compile_program` from `Item::ConstDecl` items before any function
    /// body is compiled. Key: const name. Value: the const's value
    /// expression. References to a const inside function bodies (parsed as
    /// `ExprKind::Identifier(name)` for bare uses) re-compile this stored
    /// expression at the use site, leaving constant folding to LLVM.
    /// Cycles are precluded upstream by the typechecker's const-evaluation
    /// pass (`check_const_decl`).
    consts: HashMap<String, Expr>,
    /// Source filename threaded in from the CLI (`compile_to_object_with_options`
    /// / `compile_to_ir_with_options`). When `Some`, `emit_error_trace_push`
    /// emits a deduped global string and passes its `(ptr, len)` to the runtime
    /// so error-return traces print as `<file>:<line>:<col>`. `None` preserves
    /// the original `<line>:<col>` MVP output for callers that don't supply a
    /// filename (most tests, ad-hoc IR dumps).
    source_filename: Option<String>,
    /// Memoized `(ptr, len)` for the global string materialized from
    /// `source_filename`. Populated lazily on first `?` site so we don't add
    /// an unused global to programs with no `?` propagation. Cleared on each
    /// `compile_program` entry alongside the other side-tables.
    source_filename_global: Option<(PointerValue<'ctx>, u64)>,
    /// Source text threaded in from the CLI (`compile_to_object_with_options`
    /// / `compile_to_ir_with_options` via `set_source_text`). When `Some`,
    /// `record_spawn_site` resolves each `par {}` block's byte offset to
    /// `(line, col)` via `crate::byte_offset_to_line_col`. When `None`,
    /// recorded entries fall back to `(0, 0)` — the metadata table still
    /// emits, just without source-position fidelity (most tests and ad-hoc
    /// IR dumps don't supply source text, and the `(line, col)` fields are
    /// strictly for the slice 5 / debugger surface).
    source_text: Option<String>,
    /// Symbols carrying `#[used]` collected during declaration. After the
    /// program is fully lowered, `emit_llvm_used` materializes them into the
    /// special `@llvm.used` appending-linkage global so the linker preserves
    /// each symbol even when nothing else references it. Order is preserved
    /// for stable IR output (helps snapshot tests and diffs).
    used_symbols: Vec<FunctionValue<'ctx>>,
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
    branch_cancel_ptr: Option<PointerValue<'ctx>>,
    // ── RC-fallback bindings ──────────────────────────────────────
    /// Per-function RC-fallback binding names populated from `OwnershipCheckResult`.
    /// Function name → set of binding names that need heap-boxing + refcount.
    rc_fallback_fns: HashMap<String, HashSet<String>>,
    /// Per-function Arc-promoted binding names — the subset of `rc_fallback_fns`
    /// flagged by the ownership pass as crossing a `par {}` thread boundary.
    /// Inc/dec on these bindings emits atomic LLVM operations (`atomicrmw add` /
    /// `atomicrmw sub`, `SeqCst`); the rest stay on plain non-atomic load+arith+store.
    /// Allocation site is unchanged — the heap layout `{ refcount: i64, payload: T }`
    /// is identical for both flavors.
    arc_fallback_fns: HashMap<String, HashSet<String>>,
    /// Heap struct type for each active RC-fallback binding in the current function.
    /// Cleared at each `compile_function` call. Key: binding name.
    rc_fallback_heap_types: HashMap<String, StructType<'ctx>>,
    /// Per-function parallelization decisions populated from `ConcurrencyAnalysis`.
    /// Function name → `FunctionConcurrency` (parallel groups + total stmt count).
    /// Threaded in by `load_concurrency_analysis`; consumed in slice 2 by the
    /// auto-par lowering path that emits `karac_par_run` for inferred groups
    /// outside explicit `par {}` blocks. Empty when no analysis was supplied.
    concurrency_decisions: HashMap<String, FunctionConcurrency>,
    /// Name of the function currently being compiled (for rc_fallback_fns lookup).
    current_fn_name: String,
    // ── Par block runtime ─────────────────────────────────────────
    /// Monotonic counter used to generate unique par-branch function names.
    /// Also serves as the `SpawnSiteId` for each `par {}` block — the value
    /// at the time `emit_par_run` records a spawn site is the ID written
    /// into the `KARAC_SPAWN_SITES` metadata table (slice 3 of the
    /// Debugger Contract; see `SpawnSiteRecord`).
    par_counter: u32,
    /// Runtime struct `KaracBranch { ptr func, ptr ctx }` — shared across par blocks.
    karac_branch_ty: StructType<'ctx>,
    /// Runtime entry point `void karac_par_run(const KaracBranch*, usize)`.
    karac_par_run_fn: FunctionValue<'ctx>,
    // ── Debugger contract: SpawnSiteId metadata (slice 3) ─────────
    /// One entry per `par {}` block (explicit or inferred). Populated by
    /// `record_spawn_site`; emitted as the `KARAC_SPAWN_SITES` global by
    /// `emit_spawn_sites_metadata` at the end of compilation. The order
    /// matches `SpawnSiteId` order (entry 0 → ID 0, entry 1 → ID 1, …).
    spawn_sites: Vec<SpawnSiteRecord>,
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
    runtime_debug_metadata_enabled: bool,
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
    auto_par_disabled: bool,
    // ── Theme 6: `with_provider[R]` trait-method dispatch ──────────
    /// Resource name → stable u32 ID assigned at codegen init from the
    /// declaration order of `Item::EffectResource` items. The same
    /// integer flows through to runtime calls (`karac_provider_push`,
    /// `karac_provider_lookup`); the runtime is name-agnostic.
    provider_resource_ids: HashMap<String, u32>,
    /// Resource name → trait name for resources declared as
    /// `effect resource R: T`. Used to (1) drive vtable emission for
    /// the impls of `T` and (2) resolve method indices at `R.method(...)`
    /// call sites.
    provider_resource_traits: HashMap<String, String>,
    /// Trait name → ordered method-name list (source-declaration order
    /// from the `trait T { ... }` block). Vtables for `impl T for U`
    /// store fn ptrs in this same order; method dispatch resolves the
    /// vtable index by `position()` against this list.
    provider_trait_methods: HashMap<String, Vec<String>>,
    /// (impl-target type name, trait name) → emitted vtable global.
    /// Populated after impl method declarations run in `compile_program`.
    provider_vtables: HashMap<(String, String), GlobalValue<'ctx>>,
    /// Runtime extern: `karac_provider_push(frame_ptr, resource_id, data_ptr, vtable_ptr)`.
    /// Consumed by `with_provider[R]` lowering (sub-step 3).
    karac_provider_push_fn: FunctionValue<'ctx>,
    /// Runtime extern: `karac_provider_pop()`. Consumed by `with_provider[R]`
    /// lowering (sub-step 3) for the matching pop on body exit.
    karac_provider_pop_fn: FunctionValue<'ctx>,
    /// Runtime extern: `karac_provider_lookup(resource_id) -> ProviderLookupResult`.
    /// Consumed by `R.method(...)` dispatch (sub-step 4) to find the
    /// active provider's data pointer and vtable.
    karac_provider_lookup_fn: FunctionValue<'ctx>,
    /// LLVM struct type for `ProviderFrame { prev, resource_id, data, vtable }`
    /// — `#[repr(C)]` matches `runtime/src/lib.rs::ProviderFrame`. Consumed
    /// at `with_provider[R]` lowering sites for the alloca'd frame storage
    /// (sub-step 3); declared here so the type is established alongside
    /// the runtime extern declarations.
    provider_frame_ty: StructType<'ctx>,
    /// LLVM struct type for `ProviderLookupResult { data, vtable }` —
    /// matches the runtime's `#[repr(C)]` shape. Used once at codegen
    /// init to type the `karac_provider_lookup` extern's return; after
    /// that the call's return type carries the shape implicitly so
    /// extractvalue at sub-step 4 dispatch sites doesn't need to look
    /// it up here. Field retained as ABI documentation for future
    /// readers and as the canonical anchor if `ProviderLookupResult`'s
    /// shape ever changes.
    #[allow(dead_code)]
    provider_lookup_result_ty: StructType<'ctx>,
    // ── Map runtime ───────────────────────────────────────────────
    /// Per-variable Map key LLVM type (variable name → K LLVM type).
    map_key_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Map value LLVM type (variable name → V LLVM type).
    map_val_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Map key type name string (e.g. "i64", "String") for hash/eq fn selection.
    map_key_type_names: HashMap<String, String>,
    /// Per-variable element-`TypeExpr` side-table for collection variables —
    /// the *element* of a Vec/Slice/Array, or the *value* of a Map. Used by
    /// `compile_for_*_var` so for-loop bindings inherit the right side-table
    /// registrations (`vec_elem_types`, `slice_elem_types`, `map_*_types`)
    /// when the element is itself a Vec/String/Slice/Map. Without this,
    /// LLVM-type-only tracking can't distinguish `Vec[String]` from
    /// `Vec[Vec[T]]` (both store `vec_struct_type` as the element LLVM type).
    var_elem_type_exprs: HashMap<String, TypeExpr>,
    /// Per-Map-variable key-`TypeExpr` side-table (parallels
    /// `var_elem_type_exprs` for the key slot). Used by `compile_for_map_var`
    /// to register the per-iteration `k` binding when iterating with a tuple
    /// pattern `for (k, v) in m`.
    map_key_type_exprs: HashMap<String, TypeExpr>,
    /// Per-variable Set element LLVM type (variable name → T LLVM type).
    /// Mirrors `map_key_types` — `Set[T]` lowers to `Map[T, ()]` at codegen,
    /// reusing the `karac_map_*` C runtime, but the surface type identity is
    /// kept distinct so codegen can pick the right method dispatch and the
    /// Display fn can pick the `Set{...}` brace style.
    set_elem_types: HashMap<String, BasicTypeEnum<'ctx>>,
    /// Per-variable Set element type name string (e.g. `"i64"`, `"String"`)
    /// for hash/eq fn selection. Mirrors `map_key_type_names`.
    set_elem_type_names: HashMap<String, String>,
    /// Per-variable Set element-`TypeExpr` side-table. Mirrors
    /// `map_key_type_exprs` and is consulted alongside it by Set-aware paths
    /// (`compile_for_set_var`, Set Display fn) so compound element types
    /// (`Set[(i64, String)]`, `Set[Vec[T]]`) compose through the
    /// TypeExpr-aware hash/eq/Display paths.
    set_elem_type_exprs: HashMap<String, TypeExpr>,
    karac_map_new_fn: FunctionValue<'ctx>,
    karac_map_free_fn: FunctionValue<'ctx>,
    karac_map_insert_old_fn: FunctionValue<'ctx>,
    karac_map_get_fn: FunctionValue<'ctx>,
    karac_map_remove_old_fn: FunctionValue<'ctx>,
    karac_map_contains_fn: FunctionValue<'ctx>,
    karac_map_len_fn: FunctionValue<'ctx>,
    karac_map_clear_fn: FunctionValue<'ctx>,
    karac_map_iter_new_fn: FunctionValue<'ctx>,
    karac_map_iter_next_fn: FunctionValue<'ctx>,
    karac_map_iter_free_fn: FunctionValue<'ctx>,
    /// `karac_map_entry(map: ptr, key: ptr, out_slot_ptr: ptr) -> i1` —
    /// probe-and-insert-on-vacant. Used by entry chains whose terminal is
    /// `or_insert` / `or_insert_with` — codegen will write a default through
    /// the slot when occupied=false, so the runtime claims the bucket up
    /// front.
    karac_map_entry_fn: FunctionValue<'ctx>,
    /// `karac_map_lookup_slot(map: ptr, key: ptr, out_slot_ptr: ptr) -> i1`
    /// — read-only variant used by entry chains whose terminal is
    /// `and_modify`. The closure runs only when occupied=true; nothing is
    /// inserted on the Vacant path.
    karac_map_lookup_slot_fn: FunctionValue<'ctx>,
    /// `karac_string_clone(src: ptr, dst: ptr) -> void` — runtime helper
    /// for the codegen-emitted String case in `emit_clone_fn_for_type_expr`.
    /// Allocates a fresh buffer, copies len bytes, writes the new
    /// `{data, len, cap}` to `dst`. Static-literal sources (cap = 0) get
    /// a heap-owned copy so scope-exit cleanup fires; source untouched.
    karac_string_clone_fn: FunctionValue<'ctx>,
    /// Per-type clone function cache. Keyed on the canonical mangled type
    /// name (`display_mangle_te`). Each emitted fn has signature
    /// `void karac_clone_<typename>(*const T src, *mut T dst)` — caller
    /// provides both source and destination addresses, callee writes the
    /// cloned value into the destination slot. Mirror of `display_fn_cache`.
    clone_fn_cache: HashMap<String, FunctionValue<'ctx>>,
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
    display_fn_cache: HashMap<String, FunctionValue<'ctx>>,
    // ── Error return trace runtime ────────────────────────────────
    /// `void karac_error_trace_push(ptr file, i64 file_len, i32 line, i32 col)`.
    /// Called by `compile_question` at each `?` failure block before
    /// `emit_scope_cleanup`. The runtime maintains a thread-local depth-64
    /// ring buffer; an atexit handler prints it to stderr at program exit.
    karac_error_trace_push_fn: FunctionValue<'ctx>,
    /// `void karac_error_trace_clear()`. Emitted at every `?` success site
    /// so a recovered earlier propagation doesn't leak frames into a later
    /// failure.
    karac_error_trace_clear_fn: FunctionValue<'ctx>,
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
            enum_layouts: HashMap::new(),
            loop_stack: Vec::new(),
            generic_fns: HashMap::new(),
            generated_monos: HashSet::new(),
            type_subst: HashMap::new(),
            closure_counter: 0,
            closure_fn_types: HashMap::new(),
            pending_closure_fn_type: None,
            shared_types: HashMap::new(),
            malloc_fn,
            free_fn,
            exit_fn,
            memcmp_fn,
            vec_elem_types: HashMap::new(),
            slice_elem_types: HashMap::new(),
            fn_param_slice_elem: HashMap::new(),
            ref_params: HashMap::new(),
            fn_param_ref: HashMap::new(),
            soa_layouts: HashMap::new(),
            scope_cleanup_actions: Vec::new(),
            question_conversions: HashMap::new(),
            callee_effectful: HashMap::new(),
            method_callee_types: HashMap::new(),
            pattern_binding_types: HashMap::new(),
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
            karac_map_new_fn,
            karac_map_free_fn,
            karac_map_insert_old_fn,
            karac_map_get_fn,
            karac_map_remove_old_fn,
            karac_map_contains_fn,
            karac_map_len_fn,
            karac_map_clear_fn,
            karac_map_iter_new_fn,
            karac_map_iter_next_fn,
            karac_map_iter_free_fn,
            karac_map_entry_fn,
            karac_map_lookup_slot_fn,
            karac_string_clone_fn,
            clone_fn_cache: HashMap::new(),
            display_fn_cache: HashMap::new(),
            karac_error_trace_push_fn,
            karac_error_trace_clear_fn,
        }
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

    // ── Type resolution ───────────────────────────────────────────

    fn llvm_type_for_type_expr(&self, ty: &TypeExpr) -> BasicTypeEnum<'ctx> {
        match &ty.kind {
            TypeKind::Path(path) => {
                let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if name == "Array" {
                    if let Some(arr_ty) = self.llvm_array_type(&path.generic_args) {
                        return arr_ty;
                    }
                }
                if name == "Vec" {
                    return self.vec_struct_type().into();
                }
                if name == "Slice" {
                    return self.slice_struct_type().into();
                }
                // Map[K,V] and Set[T] are opaque heap pointers managed by the
                // karac_map_* runtime functions.
                if name == "Map" || name == "Set" || name == "SortedSet" {
                    return self.context.ptr_type(AddressSpace::default()).into();
                }
                self.llvm_type_for_name(name)
            }
            TypeKind::Tuple(elems) if elems.is_empty() => {
                // unit type → i64 zero
                self.context.i64_type().into()
            }
            TypeKind::Tuple(elems) => {
                let fields: Vec<BasicTypeEnum<'ctx>> = elems
                    .iter()
                    .map(|e| self.llvm_type_for_type_expr(e))
                    .collect();
                self.context.struct_type(&fields, false).into()
            }
            TypeKind::Ref(_) | TypeKind::MutRef(_) => {
                self.context.ptr_type(AddressSpace::default()).into()
            }
            TypeKind::MutSlice(_) => self.slice_struct_type().into(),
            _ => self.context.i64_type().into(),
        }
    }

    /// Extract the inner type from a ref/mut ref type expression.
    fn inner_type_of_ref(&self, ty: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        match &ty.kind {
            TypeKind::Ref(inner) | TypeKind::MutRef(inner) => {
                Some(self.llvm_type_for_type_expr(inner))
            }
            _ => None,
        }
    }

    /// Lower `Array[T, N]` generic args to an LLVM `[N x T]` type.
    /// Mirrors typechecker::lower_array_type — accepts only positive integer-literal size.
    fn llvm_array_type(
        &self,
        generic_args: &Option<Vec<GenericArg>>,
    ) -> Option<BasicTypeEnum<'ctx>> {
        let args = generic_args.as_ref()?;
        if args.len() != 2 {
            return None;
        }
        let elem_ty_expr = match &args[0] {
            GenericArg::Type(t) => t,
            GenericArg::Const(_) => return None,
        };
        let size = match &args[1] {
            GenericArg::Const(expr) => match &expr.kind {
                ExprKind::Integer(n, _) if *n >= 0 => *n as u32,
                _ => return None,
            },
            GenericArg::Type(_) => return None,
        };
        let elem_ty = self.llvm_type_for_type_expr(elem_ty_expr);
        Some(elem_ty.array_type(size).into())
    }

    /// Vec[T] runtime layout: `{ ptr data, i64 len, i64 capacity }`.
    fn vec_struct_type(&self) -> StructType<'ctx> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default()).into();
        let i64_ty = self.context.i64_type().into();
        self.context.struct_type(&[ptr_ty, i64_ty, i64_ty], false)
    }

    /// Slice[T] and `mut Slice[T]` runtime layout: `{ ptr data, i64 len }`.
    /// Mutability is a type-system concept — the physical layout is identical
    /// for read-only and mutable slices. 16 bytes on 64-bit platforms.
    fn slice_struct_type(&self) -> StructType<'ctx> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default()).into();
        let i64_ty = self.context.i64_type().into();
        self.context.struct_type(&[ptr_ty, i64_ty], false)
    }

    /// Produce an LLVM integer type matching the source-level suffix.
    /// `None` defaults to `i64`. `I128`/`U128` are not supported and fall
    /// back to `i64` — the typechecker emits a diagnostic on the literal,
    /// so codegen never needs to reach a correct 128-bit representation.
    fn llvm_int_type_for_suffix(&self, sfx: Option<IntSuffix>) -> inkwell::types::IntType<'ctx> {
        match sfx {
            None => self.context.i64_type(),
            Some(IntSuffix::I8) | Some(IntSuffix::U8) => self.context.i8_type(),
            Some(IntSuffix::I16) | Some(IntSuffix::U16) => self.context.i16_type(),
            Some(IntSuffix::I32) | Some(IntSuffix::U32) => self.context.i32_type(),
            Some(IntSuffix::I64) | Some(IntSuffix::U64) => self.context.i64_type(),
            Some(IntSuffix::I128) | Some(IntSuffix::U128) => self.context.i64_type(),
        }
    }

    fn llvm_float_type_for_suffix(
        &self,
        sfx: Option<FloatSuffix>,
    ) -> inkwell::types::FloatType<'ctx> {
        match sfx {
            None | Some(FloatSuffix::F64) => self.context.f64_type(),
            Some(FloatSuffix::F32) => self.context.f32_type(),
        }
    }

    fn const_int_for_suffix(
        &self,
        n: i64,
        sfx: Option<IntSuffix>,
    ) -> inkwell::values::IntValue<'ctx> {
        let is_signed = matches!(
            sfx,
            None | Some(IntSuffix::I8)
                | Some(IntSuffix::I16)
                | Some(IntSuffix::I32)
                | Some(IntSuffix::I64)
                | Some(IntSuffix::I128)
        );
        self.llvm_int_type_for_suffix(sfx)
            .const_int(n as u64, is_signed)
    }

    fn const_float_for_suffix(
        &self,
        f: f64,
        sfx: Option<FloatSuffix>,
    ) -> inkwell::values::FloatValue<'ctx> {
        self.llvm_float_type_for_suffix(sfx).const_float(f)
    }

    /// Infer the slice element type from a let-binding RHS that produces
    /// a slice value. Recognizes `.as_slice()` / `.as_slice_mut()` on a
    /// known sequence variable and range-indexing `x[a..b]` on the same.
    /// Returns `None` when the RHS is not a slice-producing shape.
    fn infer_slice_elem_from_rhs(&self, expr: &Expr) -> Option<BasicTypeEnum<'ctx>> {
        match &expr.kind {
            ExprKind::MethodCall { object, method, .. }
                if method == "as_slice" || method == "as_slice_mut" =>
            {
                self.infer_elem_from_source(object)
            }
            ExprKind::Index { object, index } => {
                if matches!(&index.kind, ExprKind::Range { .. }) {
                    self.infer_elem_from_source(object)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Look up the element LLVM type of a known sequence variable (Array,
    /// Vec, or Slice).
    fn infer_elem_from_source(&self, object: &Expr) -> Option<BasicTypeEnum<'ctx>> {
        let name = if let ExprKind::Identifier(n) = &object.kind {
            n
        } else {
            return None;
        };
        if let Some(slot) = self.variables.get(name.as_str()) {
            if let BasicTypeEnum::ArrayType(at) = slot.ty {
                return Some(at.get_element_type());
            }
        }
        if let Some(&elem) = self.slice_elem_types.get(name.as_str()) {
            return Some(elem);
        }
        if let Some(&elem) = self.vec_elem_types.get(name.as_str()) {
            return Some(elem);
        }
        if let Some(&BasicTypeEnum::ArrayType(at)) = self.ref_params.get(name.as_str()) {
            return Some(at.get_element_type());
        }
        None
    }

    /// Extract the element LLVM type from a `Slice[T]` or `mut Slice[T]`
    /// type expression.
    fn extract_slice_elem_type(&self, te: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        match &te.kind {
            TypeKind::Path(path) => {
                let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if name != "Slice" {
                    return None;
                }
                let args = path.generic_args.as_ref()?;
                if args.len() != 1 {
                    return None;
                }
                match &args[0] {
                    GenericArg::Type(t) => Some(self.llvm_type_for_type_expr(t)),
                    GenericArg::Const(_) => None,
                }
            }
            TypeKind::MutSlice(element) => Some(self.llvm_type_for_type_expr(element)),
            _ => None,
        }
    }

    /// Extract the element LLVM type from a `Vec[T]` type expression.
    fn extract_vec_elem_type(&self, te: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        if let TypeKind::Path(path) = &te.kind {
            let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
            if name == "Vec" {
                if let Some(args) = &path.generic_args {
                    if let Some(GenericArg::Type(elem_te)) = args.first() {
                        return Some(self.llvm_type_for_type_expr(elem_te));
                    }
                }
            }
        }
        None
    }

    fn is_string_type_expr(&self, te: &TypeExpr) -> bool {
        if let TypeKind::Path(path) = &te.kind {
            path.segments.first().map(|s| s.as_str()) == Some("String")
        } else {
            false
        }
    }

    /// Extract the key type name string from a `Map[K, V]` type expression.
    /// Returns a canonical mangled name suitable for `karac_hash_<name>` —
    /// path segment for named types, `tuple_T1_T2_…_Tn` for tuples (recursive).
    fn extract_map_key_name(te: &TypeExpr) -> Option<String> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Map") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if let Some(GenericArg::Type(k_te)) = args.first() {
                return Some(Self::mangled_type_name(k_te));
            }
        }
        None
    }

    /// Produce a canonical, deterministic name for a `TypeExpr` suitable for
    /// use as a per-type function suffix (`karac_hash_<name>`, `karac_eq_<name>`).
    /// Path types collapse to their head segment; tuples mangle recursively as
    /// `tuple_T1_T2_…_Tn`. Unsupported shapes fall back to "unknown" — the
    /// typechecker's `K: Hash + Eq` enforcement prevents codegen from ever
    /// reaching such a key type.
    fn mangled_type_name(te: &TypeExpr) -> String {
        match &te.kind {
            TypeKind::Path(p) => p
                .segments
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".to_string()),
            TypeKind::Tuple(elems) if elems.is_empty() => "unit".to_string(),
            TypeKind::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(Self::mangled_type_name).collect();
                format!("tuple_{}", parts.join("_"))
            }
            _ => "unknown".to_string(),
        }
    }

    /// Extract (K, V) LLVM types from a `Map[K, V]` type expression.
    fn extract_map_kv_types(
        &self,
        te: &TypeExpr,
    ) -> Option<(BasicTypeEnum<'ctx>, BasicTypeEnum<'ctx>)> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Map") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if args.len() != 2 {
                return None;
            }
            let k = match &args[0] {
                GenericArg::Type(t) => self.llvm_type_for_type_expr(t),
                _ => return None,
            };
            let v = match &args[1] {
                GenericArg::Type(t) => self.llvm_type_for_type_expr(t),
                _ => return None,
            };
            Some((k, v))
        } else {
            None
        }
    }

    /// Extract the element LLVM type from a `Set[T]` type expression.
    /// Mirror of `extract_map_kv_types` for the single-type-parameter Set.
    fn extract_set_elem_type(&self, te: &TypeExpr) -> Option<BasicTypeEnum<'ctx>> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Set") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if args.is_empty() {
                return None;
            }
            if let GenericArg::Type(t) = &args[0] {
                return Some(self.llvm_type_for_type_expr(t));
            }
        }
        None
    }

    /// Extract the element shallow type-name (e.g. `"i64"`, `"String"`) from
    /// a `Set[T]` type expression. Used to drive hash/eq fn selection.
    /// Mirror of `extract_map_key_name`.
    fn extract_set_elem_name(te: &TypeExpr) -> Option<String> {
        if let TypeKind::Path(path) = &te.kind {
            if path.segments.first().map(|s| s.as_str()) != Some("Set") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if let Some(GenericArg::Type(elem_te)) = args.first() {
                return Some(Self::mangled_type_name(elem_te));
            }
        }
        None
    }

    /// Register a variable's collection-type metadata in the side-tables
    /// driven off a Kāra `TypeExpr`. Mirrors the let-statement site at
    /// `compile_stmt(StmtKind::Let)` so for-loop bindings can inherit the
    /// same registrations from the source's stored element `TypeExpr`.
    ///
    /// Populates whichever subset of `vec_elem_types` / `slice_elem_types` /
    /// `map_key_types` / `map_val_types` / `map_key_type_names` /
    /// `var_elem_type_exprs` / `map_key_type_exprs` / `set_elem_types` /
    /// `set_elem_type_names` / `set_elem_type_exprs` matches the `TypeExpr`
    /// shape; primitives (and other shapes we don't track) are no-ops.
    fn register_var_from_type_expr(&mut self, var_name: &str, te: &TypeExpr) {
        if let Some(elem_ty) = self.extract_vec_elem_type(te) {
            self.vec_elem_types.insert(var_name.to_string(), elem_ty);
            if let Some(inner) = vec_inner_type_expr(te) {
                self.var_elem_type_exprs.insert(var_name.to_string(), inner);
            }
            return;
        }
        if self.is_string_type_expr(te) {
            self.vec_elem_types
                .insert(var_name.to_string(), self.context.i8_type().into());
            return;
        }
        if let Some(elem_ty) = self.extract_slice_elem_type(te) {
            self.slice_elem_types.insert(var_name.to_string(), elem_ty);
            if let Some(inner) = slice_inner_type_expr(te) {
                self.var_elem_type_exprs.insert(var_name.to_string(), inner);
            }
            return;
        }
        if let Some((k_ty, v_ty)) = self.extract_map_kv_types(te) {
            self.map_key_types.insert(var_name.to_string(), k_ty);
            self.map_val_types.insert(var_name.to_string(), v_ty);
            if let Some(k_name) = Self::extract_map_key_name(te) {
                self.map_key_type_names.insert(var_name.to_string(), k_name);
            }
            if let Some((k_te, v_te)) = map_kv_type_exprs(te) {
                self.map_key_type_exprs.insert(var_name.to_string(), k_te);
                self.var_elem_type_exprs.insert(var_name.to_string(), v_te);
            }
            return;
        }
        if let Some(elem_ty) = self.extract_set_elem_type(te) {
            self.set_elem_types.insert(var_name.to_string(), elem_ty);
            if let Some(elem_name) = Self::extract_set_elem_name(te) {
                self.set_elem_type_names
                    .insert(var_name.to_string(), elem_name);
            }
            if let Some(elem_te) = set_inner_type_expr(te) {
                self.set_elem_type_exprs
                    .insert(var_name.to_string(), elem_te);
            }
        }
    }

    /// Register collection side-tables for the bindings produced by a
    /// for-loop's destructuring pattern, using the source variable's
    /// stored element `TypeExpr`. Without this, `for s in vec_of_strings`
    /// binds `s` only in `self.variables` — method dispatch in
    /// `compile_expr_method_call` then misses the Vec/Slice/Map side-table
    /// check and falls through to the silent-`0` default.
    fn register_for_loop_bindings(&mut self, pattern: &Pattern, source_var: &str) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                if let Some(elem_te) = self.var_elem_type_exprs.get(source_var).cloned() {
                    self.register_var_from_type_expr(name, &elem_te);
                }
            }
            // `for (k, v) in m` — only legal tuple iteration shape today
            // (Map). `for (a, b) in vec_of_tuples` would fall through; the
            // tuple-element-classification follow-up would extend this arm.
            PatternKind::Tuple(pats) if pats.len() == 2 => {
                if let PatternKind::Binding(k_name) = &pats[0].kind {
                    if let Some(k_te) = self.map_key_type_exprs.get(source_var).cloned() {
                        self.register_var_from_type_expr(k_name, &k_te);
                    }
                }
                if let PatternKind::Binding(v_name) = &pats[1].kind {
                    if let Some(v_te) = self.var_elem_type_exprs.get(source_var).cloned() {
                        self.register_var_from_type_expr(v_name, &v_te);
                    }
                }
            }
            _ => {}
        }
    }

    fn is_map_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "Map" && segments[1] == "new";
            }
        }
        false
    }

    fn is_set_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "Set" && segments[1] == "new";
            }
        }
        false
    }

    fn is_vec_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "Vec" && segments[1] == "new";
            }
        }
        false
    }

    /// Debugger Contract slice 5: `Runtime.list_par_blocks()` /
    /// `Runtime.list_tasks()` return `Vec[ParBlockInfo]` /
    /// `Vec[TaskInfo]`. Used by the let-binding registration to set up
    /// `vec_elem_types` so subsequent `.len()` / `.is_empty()` dispatches
    /// through `compile_vec_method`.
    fn is_runtime_introspection_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2
                    && segments[0] == "Runtime"
                    && (segments[1] == "list_par_blocks" || segments[1] == "list_tasks");
            }
        }
        false
    }

    fn is_string_binary_op(&self, expr: &Expr) -> bool {
        // Source-form `a + b` (pre-lowering).
        if let ExprKind::Binary {
            op: BinOp::Add,
            left,
            ..
        } = &expr.kind
        {
            return self.first_operand_is_string(left);
        }
        // Lowered form `Call(Path(["String", "add"]), [a, b])` — produced by
        // the operator lowering pass. Also recognize String + String here.
        if let ExprKind::Call { callee, args } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                if segments.len() == 2 && segments[0] == "String" && segments[1] == "add" {
                    if let Some(first) = args.first() {
                        return self.first_operand_is_string(&first.value);
                    }
                }
            }
        }
        false
    }

    /// Helper: is this expression a string literal or a known string variable?
    fn first_operand_is_string(&self, expr: &Expr) -> bool {
        if matches!(&expr.kind, ExprKind::StringLit(_)) {
            return true;
        }
        if let ExprKind::Identifier(name) = &expr.kind {
            return self
                .vec_elem_types
                .get(name.as_str())
                .map(|t| t.is_int_type() && t.into_int_type().get_bit_width() == 8)
                .unwrap_or(false);
        }
        false
    }

    fn is_string_new_call(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind {
            if let ExprKind::Path { segments, .. } = &callee.kind {
                return segments.len() == 2 && segments[0] == "String" && segments[1] == "new";
            }
        }
        false
    }

    /// Look up the Vec element type for a variable, defaulting to i64.
    fn vec_elem_type_for_var(&self, name: &str) -> BasicTypeEnum<'ctx> {
        self.vec_elem_types
            .get(name)
            .copied()
            .unwrap_or_else(|| self.context.i64_type().into())
    }

    fn llvm_type_for_name(&self, name: &str) -> BasicTypeEnum<'ctx> {
        // Active monomorphization substitution takes priority.
        if let Some(&ty) = self.type_subst.get(name) {
            return ty;
        }
        match name {
            "i8" | "u8" => self.context.i8_type().into(),
            "i16" | "u16" => self.context.i16_type().into(),
            "i32" | "u32" => self.context.i32_type().into(),
            "i64" | "u64" | "isize" | "usize" => self.context.i64_type().into(),
            "f32" => self.context.f32_type().into(),
            "f64" => self.context.f64_type().into(),
            "bool" => self.context.bool_type().into(),
            "String" | "str" => self.vec_struct_type().into(),
            name => {
                // Shared types are heap-allocated pointers.
                if self.shared_types.contains_key(name) {
                    return self.context.ptr_type(AddressSpace::default()).into();
                }
                if let Some(st) = self.struct_types.get(name) {
                    (*st).into()
                } else if let Some(layout) = self.enum_layouts.get(name) {
                    // Enum types are represented as tagged-union structs.
                    layout.llvm_type.into()
                } else {
                    self.context.i64_type().into()
                }
            }
        }
    }

    fn llvm_return_type(&self, ty: &Option<TypeExpr>) -> Option<BasicTypeEnum<'ctx>> {
        let te = ty.as_ref()?;
        match &te.kind {
            TypeKind::Path(path) => {
                let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if name.is_empty() {
                    return None;
                }
                // Delegate to llvm_type_for_type_expr so generic types
                // (Array[T, N], Vec[T], Slice[T], Map[K,V], …) are honored
                // — bare llvm_type_for_name drops generic args.
                Some(self.llvm_type_for_type_expr(te))
            }
            TypeKind::Tuple(elems) if elems.is_empty() => None,
            _ => Some(self.llvm_type_for_type_expr(te)),
        }
    }

    fn llvm_param_type(&self, param: &Param) -> BasicMetadataTypeEnum<'ctx> {
        BasicMetadataTypeEnum::from(self.llvm_type_for_type_expr(&param.ty))
    }

    // ── Shared type helpers ─────────────────────────────────────────

    /// Check if a type name refers to a shared (RC) type.
    #[allow(dead_code)]
    fn is_shared_type(&self, name: &str) -> bool {
        self.shared_types.contains_key(name)
    }

    /// Get the heap struct type for a shared type, if it exists.
    #[allow(dead_code)]
    fn shared_heap_type(&self, name: &str) -> Option<StructType<'ctx>> {
        self.shared_types.get(name).map(|info| info.heap_type)
    }

    /// If the expression refers to a variable of shared type, return the type name and info.
    fn shared_type_for_expr(&self, expr: &Expr) -> Option<(String, SharedTypeInfo<'ctx>)> {
        if let ExprKind::Identifier(var_name) = &expr.kind {
            if let Some(type_name) = self.var_type_names.get(var_name.as_str()) {
                if let Some(info) = self.shared_types.get(type_name.as_str()) {
                    return Some((type_name.clone(), info.clone()));
                }
            }
        }
        None
    }

    /// Allocate a new RC heap object: `malloc(sizeof(heap_type))`, store refcount = 1.
    /// Returns a pointer to the heap object.
    fn emit_panic(&self, message: &str) {
        let msg = self
            .builder
            .build_global_string_ptr(&format!("panic: {}\n\0", message), "panic_msg")
            .unwrap();
        self.builder
            .build_call(
                self.printf_fn,
                &[msg.as_pointer_value().into()],
                "panic_print",
            )
            .unwrap();
        let exit_code = self.context.i32_type().const_int(1, false);
        self.builder
            .build_call(self.exit_fn, &[exit_code.into()], "")
            .unwrap();
    }

    fn emit_rc_alloc(&self, heap_type: StructType<'ctx>) -> PointerValue<'ctx> {
        let size = heap_type.size_of().expect("heap type must be sized");
        let call = self
            .builder
            .build_call(self.malloc_fn, &[size.into()], "rc_alloc")
            .unwrap();
        let ptr = call
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        // Store refcount = 1 at field 0.
        let rc_ptr = self
            .builder
            .build_struct_gep(heap_type, ptr, 0, "rc_ptr")
            .unwrap();
        self.builder
            .build_store(rc_ptr, self.context.i64_type().const_int(1, false))
            .unwrap();
        ptr
    }

    /// Increment the reference count of a shared object.
    fn emit_rc_inc(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
        let rc_ptr = self
            .builder
            .build_struct_gep(heap_type, ptr, 0, "rc_ptr")
            .unwrap();
        let rc = self
            .builder
            .build_load(self.context.i64_type(), rc_ptr, "rc")
            .unwrap()
            .into_int_value();
        let rc_inc = self
            .builder
            .build_int_add(rc, self.context.i64_type().const_int(1, false), "rc_inc")
            .unwrap();
        self.builder.build_store(rc_ptr, rc_inc).unwrap();
    }

    /// Decrement the reference count. If it reaches zero, call free().
    fn emit_rc_dec(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
        let rc_ptr = self
            .builder
            .build_struct_gep(heap_type, ptr, 0, "rc_ptr")
            .unwrap();
        let rc = self
            .builder
            .build_load(self.context.i64_type(), rc_ptr, "rc")
            .unwrap()
            .into_int_value();
        let rc_dec = self
            .builder
            .build_int_sub(rc, self.context.i64_type().const_int(1, false), "rc_dec")
            .unwrap();
        self.builder.build_store(rc_ptr, rc_dec).unwrap();

        let is_zero = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                rc_dec,
                self.context.i64_type().const_zero(),
                "rc_is_zero",
            )
            .unwrap();

        let current_fn = self.current_fn.unwrap();
        let free_bb = self.context.append_basic_block(current_fn, "rc_free");
        let done_bb = self.context.append_basic_block(current_fn, "rc_done");

        self.builder
            .build_conditional_branch(is_zero, free_bb, done_bb)
            .unwrap();

        self.builder.position_at_end(free_bb);
        self.builder
            .build_call(self.free_fn, &[ptr.into()], "")
            .unwrap();
        self.builder.build_unconditional_branch(done_bb).unwrap();

        self.builder.position_at_end(done_bb);
    }

    /// Atomic counterpart to `emit_rc_inc` for `arc_values`-promoted bindings.
    /// `atomicrmw add refcount, 1, seq_cst`. Mirrors the non-atomic helper's
    /// shape exactly — same `struct_gep` to land on the refcount field, same
    /// `+1`-by-i64 — only the load+arith+store sequence changes to a single
    /// `atomicrmw` op. Memory ordering is `SequentiallyConsistent` for v1
    /// (correct, conservative); relaxation to `Monotonic`+`Acquire`/`Release`
    /// per Rust's `Arc` is a future optimization tracked under "out of scope"
    /// in the slice plan. The returned old value is discarded — increments do
    /// not need to observe it (only decrements do, to detect transition to 0).
    fn emit_arc_inc(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
        let rc_ptr = self
            .builder
            .build_struct_gep(heap_type, ptr, 0, "arc_ptr")
            .unwrap();
        let one = self.context.i64_type().const_int(1, false);
        self.builder
            .build_atomicrmw(
                AtomicRMWBinOp::Add,
                rc_ptr,
                one,
                AtomicOrdering::SequentiallyConsistent,
            )
            .unwrap();
    }

    /// Atomic counterpart to `emit_rc_dec`. Uses `atomicrmw sub refcount, 1,
    /// seq_cst`; the returned value is the *previous* refcount, so the
    /// "drop-to-zero" check is `old == 1` (post-decrement value is 0). Same
    /// branch shape as `emit_rc_dec`: a `free_bb` that calls `free(ptr)` and
    /// a `done_bb` join.
    fn emit_arc_dec(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
        let rc_ptr = self
            .builder
            .build_struct_gep(heap_type, ptr, 0, "arc_ptr")
            .unwrap();
        let one = self.context.i64_type().const_int(1, false);
        let old = self
            .builder
            .build_atomicrmw(
                AtomicRMWBinOp::Sub,
                rc_ptr,
                one,
                AtomicOrdering::SequentiallyConsistent,
            )
            .unwrap();

        let is_last = self
            .builder
            .build_int_compare(IntPredicate::EQ, old, one, "arc_is_last")
            .unwrap();

        let current_fn = self.current_fn.unwrap();
        let free_bb = self.context.append_basic_block(current_fn, "arc_free");
        let done_bb = self.context.append_basic_block(current_fn, "arc_done");

        self.builder
            .build_conditional_branch(is_last, free_bb, done_bb)
            .unwrap();

        self.builder.position_at_end(free_bb);
        self.builder
            .build_call(self.free_fn, &[ptr.into()], "")
            .unwrap();
        self.builder.build_unconditional_branch(done_bb).unwrap();

        self.builder.position_at_end(done_bb);
    }

    /// Dispatch an inc on `name`'s refcount: atomic path when `name` is in
    /// `arc_fallback_fns` for the current function, plain non-atomic otherwise.
    fn emit_refcount_inc(&self, name: &str, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
        if self.is_arc_binding(name) {
            self.emit_arc_inc(heap_type, ptr);
        } else {
            self.emit_rc_inc(heap_type, ptr);
        }
    }

    /// Dispatch a dec on `name`'s refcount: atomic path when `name` is in
    /// `arc_fallback_fns` for the current function, plain non-atomic otherwise.
    fn emit_refcount_dec(&self, name: &str, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
        if self.is_arc_binding(name) {
            self.emit_arc_dec(heap_type, ptr);
        } else {
            self.emit_rc_dec(heap_type, ptr);
        }
    }

    /// Track a shared-type variable for scope-exit rc_dec.
    fn track_rc_var(&mut self, name: &str, ptr: PointerValue<'ctx>, heap_type: StructType<'ctx>) {
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::RcDec {
                name: name.to_string(),
                ptr,
                heap_type,
            });
        }
    }

    /// Track a Vec/String alloca for scope-exit buffer free.
    fn track_vec_var(&mut self, vec_alloca: PointerValue<'ctx>) {
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::FreeVecBuffer { vec_alloca });
        }
    }

    /// Track a Map alloca for scope-exit `karac_map_free` call.
    fn track_map_var(&mut self, map_alloca: PointerValue<'ctx>) {
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::FreeMapHandle { map_alloca });
        }
    }

    /// Emit all cleanup actions registered across all scope frames (for function exit).
    /// Iterates frames in reverse (innermost first) and within each frame in push order
    /// (consistent with how RAII destruction works in block-structured languages).
    fn emit_scope_cleanup(&self) {
        let vec_ty = self.vec_struct_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();

        for frame in self.scope_cleanup_actions.iter().rev() {
            for action in frame {
                match action {
                    CleanupAction::RcDec {
                        name,
                        ptr,
                        heap_type,
                    } => {
                        let current_ptr = if let Some(slot) = self.variables.get(name) {
                            self.builder
                                .build_load(ptr_ty, slot.ptr, &format!("{}_rc_cleanup", name))
                                .unwrap()
                                .into_pointer_value()
                        } else {
                            *ptr
                        };
                        self.emit_refcount_dec(name, *heap_type, current_ptr);
                    }
                    CleanupAction::FreeVecBuffer { vec_alloca } => {
                        let cap_ptr = self
                            .builder
                            .build_struct_gep(vec_ty, *vec_alloca, 2, "cleanup.cap.ptr")
                            .unwrap();
                        let cap = self
                            .builder
                            .build_load(i64_t, cap_ptr, "cleanup.cap")
                            .unwrap()
                            .into_int_value();
                        let zero = i64_t.const_int(0, false);
                        let is_heap = self
                            .builder
                            .build_int_compare(IntPredicate::UGT, cap, zero, "is_heap")
                            .unwrap();
                        let free_bb = self.context.append_basic_block(fn_val, "cleanup.free");
                        let skip_bb = self.context.append_basic_block(fn_val, "cleanup.skip");
                        self.builder
                            .build_conditional_branch(is_heap, free_bb, skip_bb)
                            .unwrap();
                        self.builder.position_at_end(free_bb);
                        let data_ptr_ptr = self
                            .builder
                            .build_struct_gep(vec_ty, *vec_alloca, 0, "cleanup.data.ptr")
                            .unwrap();
                        let data = self
                            .builder
                            .build_load(ptr_ty, data_ptr_ptr, "cleanup.data")
                            .unwrap()
                            .into_pointer_value();
                        self.builder
                            .build_call(self.free_fn, &[data.into()], "")
                            .unwrap();
                        self.builder.build_unconditional_branch(skip_bb).unwrap();
                        self.builder.position_at_end(skip_bb);
                    }
                    CleanupAction::FreeMapHandle { map_alloca } => {
                        let handle = self
                            .builder
                            .build_load(ptr_ty, *map_alloca, "cleanup.map.handle")
                            .unwrap()
                            .into_pointer_value();
                        self.builder
                            .build_call(self.karac_map_free_fn, &[handle.into()], "")
                            .unwrap();
                    }
                }
            }
        }
    }

    // ── F-string helpers ──────────────────────────────────────────

    /// Append `src_len` bytes from `src_ptr` to the String (Vec<u8>) alloca at
    /// `dest_alloca`, growing the buffer if necessary.  Mirrors the inline
    /// `push_str` logic in `compile_vec_method`.
    fn emit_string_append_raw(
        &mut self,
        dest_alloca: PointerValue<'ctx>,
        src_ptr: PointerValue<'ctx>,
        src_len: inkwell::values::IntValue<'ctx>,
    ) {
        let vec_ty = self.vec_struct_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();

        let data_ptr_ptr = self
            .builder
            .build_struct_gep(vec_ty, dest_alloca, 0, "fsa.data.pp")
            .unwrap();
        let len_ptr = self
            .builder
            .build_struct_gep(vec_ty, dest_alloca, 1, "fsa.len.ptr")
            .unwrap();
        let cap_ptr = self
            .builder
            .build_struct_gep(vec_ty, dest_alloca, 2, "fsa.cap.ptr")
            .unwrap();

        let data = self
            .builder
            .build_load(ptr_ty, data_ptr_ptr, "fsa.data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_ptr, "fsa.len")
            .unwrap()
            .into_int_value();
        let cap = self
            .builder
            .build_load(i64_t, cap_ptr, "fsa.cap")
            .unwrap()
            .into_int_value();

        let new_len = self
            .builder
            .build_int_add(len, src_len, "fsa.new_len")
            .unwrap();

        // Grow if new_len > cap.
        let grow_bb = self.context.append_basic_block(fn_val, "fsa.grow");
        let copy_bb = self.context.append_basic_block(fn_val, "fsa.copy");
        let needs_grow = self
            .builder
            .build_int_compare(IntPredicate::UGT, new_len, cap, "fsa.needs_grow")
            .unwrap();
        self.builder
            .build_conditional_branch(needs_grow, grow_bb, copy_bb)
            .unwrap();

        // Grow path: compute new_cap, malloc, memcpy old data, free old, update alloca.
        self.builder.position_at_end(grow_bb);
        let two = i64_t.const_int(2, false);
        let four = i64_t.const_int(4, false);
        let doubled = self.builder.build_int_mul(cap, two, "fsa.doubled").unwrap();
        let cmp1 = self
            .builder
            .build_int_compare(IntPredicate::UGT, doubled, four, "fsa.cmp1")
            .unwrap();
        let growth_min = self
            .builder
            .build_select(cmp1, doubled, four, "fsa.gmin")
            .unwrap()
            .into_int_value();
        let cmp2 = self
            .builder
            .build_int_compare(IntPredicate::UGT, new_len, growth_min, "fsa.cmp2")
            .unwrap();
        let new_cap = self
            .builder
            .build_select(cmp2, new_len, growth_min, "fsa.new_cap")
            .unwrap()
            .into_int_value();
        let new_buf = self
            .builder
            .build_call(self.malloc_fn, &[new_cap.into()], "fsa.new_buf")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        // Copy existing data into new buffer (memcpy with len=0 is safe per C spec).
        self.builder.build_memcpy(new_buf, 1, data, 1, len).unwrap();
        // Free old heap buffer (free(null) is a no-op per C spec).
        self.builder
            .build_call(self.free_fn, &[data.into()], "")
            .unwrap();
        // Update data pointer and cap in the alloca.
        self.builder.build_store(data_ptr_ptr, new_buf).unwrap();
        self.builder.build_store(cap_ptr, new_cap).unwrap();
        self.builder.build_unconditional_branch(copy_bb).unwrap();

        // Copy path: reload cur data (updated by grow, or unchanged), memcpy src.
        self.builder.position_at_end(copy_bb);
        let cur_data = self
            .builder
            .build_load(ptr_ty, data_ptr_ptr, "fsa.cur_data")
            .unwrap()
            .into_pointer_value();
        let i8_ty = self.context.i8_type();
        let dest = unsafe {
            self.builder
                .build_gep(i8_ty, cur_data, &[len], "fsa.dest")
                .unwrap()
        };
        self.builder
            .build_memcpy(dest, 1, src_ptr, 1, src_len)
            .unwrap();
        self.builder.build_store(len_ptr, new_len).unwrap();
    }

    /// Convert a compiled value to `(raw_ptr, byte_len)` for f-string interpolation.
    /// Dispatches on the LLVM type so callers don't need to track the Kāra type name.
    ///
    /// - `String` (3-field struct) → extract (data_ptr, len)
    /// - `bool` (i1) → global "true"/"false" literal
    /// - float (f32/f64) → snprintf "%g" into a 64-byte stack buffer
    /// - integer → snprintf "%lld" into a 64-byte stack buffer
    fn compile_fstr_part_to_cstr(
        &mut self,
        val: BasicValueEnum<'ctx>,
    ) -> (PointerValue<'ctx>, inkwell::values::IntValue<'ctx>) {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let fn_val = self.current_fn.unwrap();

        match val {
            BasicValueEnum::StructValue(sv) => {
                // Treat as String: field 0 = ptr, field 1 = len.
                let ptr = self
                    .builder
                    .build_extract_value(sv, 0, "fst.ptr")
                    .unwrap()
                    .into_pointer_value();
                let len = self
                    .builder
                    .build_extract_value(sv, 1, "fst.len")
                    .unwrap()
                    .into_int_value();
                (ptr, len)
            }
            BasicValueEnum::IntValue(iv) if iv.get_type().get_bit_width() == 1 => {
                // bool
                let true_str = self
                    .builder
                    .build_global_string_ptr("true", "fst.true")
                    .unwrap();
                let false_str = self
                    .builder
                    .build_global_string_ptr("false", "fst.false")
                    .unwrap();
                let four = i64_t.const_int(4, false);
                let five = i64_t.const_int(5, false);
                let ptr = self
                    .builder
                    .build_select(
                        iv,
                        true_str.as_pointer_value(),
                        false_str.as_pointer_value(),
                        "fst.bptr",
                    )
                    .unwrap()
                    .into_pointer_value();
                let len = self
                    .builder
                    .build_select(iv, four, five, "fst.blen")
                    .unwrap()
                    .into_int_value();
                (ptr, len)
            }
            _ => {
                // Integer or float: use snprintf into a 64-byte stack buffer.
                let buf_size = i64_t.const_int(64, false);
                let buf = self.create_entry_alloca(
                    fn_val,
                    "fst.buf",
                    self.context.i8_type().array_type(64).into(),
                );
                let buf_ptr = self
                    .builder
                    .build_pointer_cast(buf, ptr_ty, "fst.buf_ptr")
                    .unwrap();
                let fmt_str = if matches!(val, BasicValueEnum::FloatValue(_)) {
                    self.builder
                        .build_global_string_ptr("%g", "fst.fmt_f")
                        .unwrap()
                        .as_pointer_value()
                } else {
                    // Integer
                    self.builder
                        .build_global_string_ptr("%lld", "fst.fmt_i")
                        .unwrap()
                        .as_pointer_value()
                };
                let written = self
                    .builder
                    .build_call(
                        self.snprintf_fn,
                        &[buf_ptr.into(), buf_size.into(), fmt_str.into(), val.into()],
                        "fst.written",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let len = self
                    .builder
                    .build_int_z_extend(written, i64_t, "fst.len")
                    .unwrap();
                (buf_ptr, len)
            }
        }
    }

    // ── Struct declaration pass ───────────────────────────────────

    fn declare_structs(&mut self, program: &Program) {
        for item in &program.items {
            if let Item::StructDef(s) = item {
                let field_types: Vec<BasicTypeEnum<'ctx>> = s
                    .fields
                    .iter()
                    .map(|f| self.llvm_type_for_type_expr(&f.ty))
                    .collect();
                let names: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();

                if s.is_shared {
                    // Shared struct: heap layout is { i64 refcount, field0, field1, … }
                    let mut heap_fields: Vec<BasicTypeEnum<'ctx>> =
                        vec![self.context.i64_type().into()]; // refcount
                    heap_fields.extend_from_slice(&field_types);
                    let heap_type = self.context.struct_type(&heap_fields, false);

                    self.shared_types.insert(
                        s.name.clone(),
                        SharedTypeInfo {
                            heap_type,
                            field_names: names.clone(),
                            is_enum: false,
                        },
                    );
                    // Also register field names for field-index lookups.
                    self.struct_field_names.insert(s.name.clone(), names);
                } else {
                    let st = self.context.struct_type(&field_types, false);
                    self.struct_types.insert(s.name.clone(), st);
                    self.struct_field_names.insert(s.name.clone(), names);
                }
            }
        }
    }

    fn collect_soa_layouts(&mut self, program: &Program) {
        for item in &program.items {
            if let Item::LayoutDef(layout) = item {
                // Extract element struct name from collection type.
                let struct_name = if let TypeKind::Path(path) = &layout.collection_type.kind {
                    if let Some(args) = &path.generic_args {
                        if let Some(GenericArg::Type(te)) = args.first() {
                            if let TypeKind::Path(inner) = &te.kind {
                                inner.segments.first().cloned()
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let struct_name = match struct_name {
                    Some(n) => n,
                    None => continue,
                };

                // Look up struct field names.
                let all_fields = match self.struct_field_names.get(&struct_name) {
                    Some(f) => f.clone(),
                    None => continue,
                };

                // Build groups from layout items.
                let mut groups = Vec::new();
                let mut cold_group: Option<SoaGroup> = None;
                let mut assigned: HashSet<String> = HashSet::new();

                for li in &layout.items {
                    match li {
                        LayoutItem::Group {
                            name,
                            fields,
                            align,
                            ..
                        } => {
                            let field_indices: Vec<usize> = fields
                                .iter()
                                .filter_map(|f| all_fields.iter().position(|af| af == f))
                                .collect();
                            for f in fields {
                                assigned.insert(f.clone());
                            }
                            groups.push(SoaGroup {
                                name: name.clone(),
                                fields: fields.clone(),
                                field_indices,
                                elem_type: None,
                                align: *align,
                                is_cold: false,
                            });
                        }
                        LayoutItem::Cold { fields, .. } => {
                            let field_indices: Vec<usize> = fields
                                .iter()
                                .filter_map(|f| all_fields.iter().position(|af| af == f))
                                .collect();
                            for f in fields {
                                assigned.insert(f.clone());
                            }
                            cold_group = Some(SoaGroup {
                                name: "__cold".to_string(),
                                fields: fields.clone(),
                                field_indices,
                                elem_type: None,
                                align: None,
                                is_cold: true,
                            });
                        }
                        LayoutItem::SplitByVariant(_) => {}
                    }
                }

                // Implicit trailing hot group for unassigned fields (excludes cold fields).
                let unassigned: Vec<String> = all_fields
                    .iter()
                    .filter(|f| !assigned.contains(*f))
                    .cloned()
                    .collect();
                if !unassigned.is_empty() {
                    let field_indices: Vec<usize> = unassigned
                        .iter()
                        .filter_map(|f| all_fields.iter().position(|af| af == f))
                        .collect();
                    groups.push(SoaGroup {
                        name: "__unassigned".to_string(),
                        fields: unassigned,
                        field_indices,
                        elem_type: None,
                        align: None,
                        is_cold: false,
                    });
                }

                let num_groups = groups.len();
                self.soa_layouts.insert(
                    layout.name.clone(),
                    SoaLayout {
                        name: layout.name.clone(),
                        struct_name,
                        groups,
                        cold_group,
                        num_groups,
                    },
                );
            }
        }
    }

    /// Returns (or lazily declares) `aligned_alloc(i64 alignment, i64 size) -> ptr`.
    /// Used for SoA group allocations with an `align(N)` modifier.
    fn aligned_alloc_fn(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("aligned_alloc") {
            return f;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_ty = self.context.i64_type();
        let fn_ty = ptr_ty.fn_type(&[i64_ty.into(), i64_ty.into()], false);
        self.module
            .add_function("aligned_alloc", fn_ty, Some(Linkage::External))
    }

    /// Build the LLVM struct type for a SoA-laid-out Vec.
    /// Layout: `{ ptr_g0, ..., ptr_gN, [ptr_cold,] i64 len, i64 cap }`.
    /// The cold pointer (if `has_cold` is true) comes after all hot group pointers and before len/cap.
    fn soa_vec_type(&self, num_groups: usize, has_cold: bool) -> StructType<'ctx> {
        let ptr_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(AddressSpace::default()).into();
        let i64_ty: BasicTypeEnum<'ctx> = self.context.i64_type().into();
        let num_ptrs = num_groups + if has_cold { 1 } else { 0 };
        let mut fields: Vec<BasicTypeEnum<'ctx>> = vec![ptr_ty; num_ptrs];
        fields.push(i64_ty); // len
        fields.push(i64_ty); // cap
        self.context.struct_type(&fields, false)
    }

    /// Returns the struct field index of the cold pointer within a SoA vec struct.
    /// `num_hot_groups` is the count of hot groups (excluding cold).
    fn soa_cold_ptr_index(num_hot_groups: usize) -> u32 {
        num_hot_groups as u32
    }

    /// Returns the struct field index of `len` within a SoA vec struct.
    fn soa_len_index(num_hot_groups: usize, has_cold: bool) -> u32 {
        num_hot_groups as u32 + if has_cold { 1 } else { 0 }
    }

    /// Returns the struct field index of `cap` within a SoA vec struct.
    fn soa_cap_index(num_hot_groups: usize, has_cold: bool) -> u32 {
        Self::soa_len_index(num_hot_groups, has_cold) + 1
    }

    /// Build the LLVM struct type for one element of a SoA group.
    /// E.g., if group "physics" has fields { position: f64, velocity: f64 },
    /// the group element type is `{ f64, f64 }`.
    fn soa_group_elem_type(&self, struct_name: &str, group: &SoaGroup) -> StructType<'ctx> {
        let struct_field_types: Vec<BasicTypeEnum<'ctx>> =
            if let Some(&st) = self.struct_types.get(struct_name) {
                (0..st.count_fields())
                    .map(|i| st.get_field_type_at_index(i).unwrap())
                    .collect()
            } else {
                Vec::new()
            };

        let group_field_types: Vec<BasicTypeEnum<'ctx>> = group
            .field_indices
            .iter()
            .filter_map(|&idx| struct_field_types.get(idx).copied())
            .collect();

        self.context.struct_type(&group_field_types, false)
    }

    fn declare_enums(&mut self, program: &Program) {
        for item in &program.items {
            if let Item::EnumDef(e) = item {
                // Compute max payload words across all variants.
                let max_words = e
                    .variants
                    .iter()
                    .map(|v| match &v.kind {
                        VariantKind::Unit => 0,
                        VariantKind::Tuple(tys) => tys.len(),
                        VariantKind::Struct(fields) => fields.len(),
                    })
                    .max()
                    .unwrap_or(0);

                // Build the unified LLVM type: { i64 tag, i64 w0, ..., i64 wN }
                let i64_t: BasicTypeEnum<'ctx> = self.context.i64_type().into();
                let mut field_types: Vec<BasicTypeEnum<'ctx>> = vec![i64_t]; // tag
                for _ in 0..max_words {
                    field_types.push(i64_t);
                }
                let llvm_type = self.context.struct_type(&field_types, false);

                let mut tags = HashMap::new();
                let mut field_counts = HashMap::new();
                for (idx, v) in e.variants.iter().enumerate() {
                    tags.insert(v.name.clone(), idx as u64);
                    let fc = match &v.kind {
                        VariantKind::Unit => 0,
                        VariantKind::Tuple(tys) => tys.len(),
                        VariantKind::Struct(fields) => fields.len(),
                    };
                    field_counts.insert(v.name.clone(), fc);
                }

                if e.is_shared {
                    // Shared enum: heap layout is { i64 refcount, i64 tag, i64 w0, … }
                    let mut heap_fields: Vec<BasicTypeEnum<'ctx>> = vec![i64_t]; // refcount
                    heap_fields.extend_from_slice(&field_types); // tag + payload words
                    let heap_type = self.context.struct_type(&heap_fields, false);

                    self.shared_types.insert(
                        e.name.clone(),
                        SharedTypeInfo {
                            heap_type,
                            field_names: vec![],
                            is_enum: true,
                        },
                    );
                }

                // Always register in enum_layouts for tag/variant resolution.
                self.enum_layouts.insert(
                    e.name.clone(),
                    EnumLayout {
                        llvm_type,
                        tags,
                        field_counts,
                    },
                );
            }
        }
    }

    /// Seed enum layouts for stdlib types that are not declared as `enum` in
    /// the prelude AST (e.g. Option[T]) so that variant construction/matching
    /// and methods like `first`/`last`/`get` can produce properly typed LLVM.
    fn seed_builtin_enum_layouts(&mut self) {
        let i64_t: BasicTypeEnum<'ctx> = self.context.i64_type().into();
        let enum_type = self.context.struct_type(&[i64_t, i64_t], false);

        // Option[T]: { i64 tag, i64 w0 }  — None(tag=0) | Some(tag=1, w0=value)
        if !self.enum_layouts.contains_key("Option") {
            let mut tags = HashMap::new();
            tags.insert("None".to_string(), 0u64);
            tags.insert("Some".to_string(), 1u64);
            let mut field_counts = HashMap::new();
            field_counts.insert("None".to_string(), 0usize);
            field_counts.insert("Some".to_string(), 1usize);
            self.enum_layouts.insert(
                "Option".to_string(),
                EnumLayout {
                    llvm_type: enum_type,
                    tags,
                    field_counts,
                },
            );
        }

        // Result[T, E]: { i64 tag, i64 w0 }  — Err(tag=0, w0=err) | Ok(tag=1, w0=val)
        if !self.enum_layouts.contains_key("Result") {
            let mut tags = HashMap::new();
            tags.insert("Err".to_string(), 0u64);
            tags.insert("Ok".to_string(), 1u64);
            let mut field_counts = HashMap::new();
            field_counts.insert("Err".to_string(), 1usize);
            field_counts.insert("Ok".to_string(), 1usize);
            self.enum_layouts.insert(
                "Result".to_string(),
                EnumLayout {
                    llvm_type: enum_type,
                    tags,
                    field_counts,
                },
            );
        }
    }

    // ── FFI: extern function declarations ──────────────────────────

    fn declare_extern_functions(&mut self, program: &Program) -> Result<(), String> {
        for item in &program.items {
            if let Item::ExternFunction(ext) = item {
                let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = ext
                    .params
                    .iter()
                    .map(|p| BasicMetadataTypeEnum::from(self.llvm_type_for_type_expr(&p.ty)))
                    .collect();

                let fn_type = match ext.return_type.as_ref().and_then(|ty| match &ty.kind {
                    TypeKind::Path(path) => {
                        let name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                        if name.is_empty() {
                            None
                        } else {
                            Some(self.llvm_type_for_name(name))
                        }
                    }
                    TypeKind::Tuple(elems) if elems.is_empty() => None,
                    _ => Some(self.llvm_type_for_type_expr(ty)),
                }) {
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

                let fn_val = self
                    .module
                    .add_function(&ext.name, fn_type, Some(Linkage::External));
                // `#[link_section]`, `#[no_mangle]`, `#[used]` attached to
                // an `extern` declaration apply to the symbol as imported.
                self.apply_linker_attrs(fn_val, &ext.attributes);
            }
        }
        Ok(())
    }

    // ── Program / function compilation ───────────────────────────

    fn compile_program(&mut self, program: &Program) -> Result<(), String> {
        self.declare_structs(program);
        self.declare_enums(program);
        self.seed_builtin_enum_layouts();
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

    /// Emit a static vtable global per `impl T for U` where `T` was
    /// declared as a provider trait via some `effect resource R: T`.
    /// The vtable is an array of fn pointers in trait-method-declaration
    /// order; method dispatch at `R.method(...)` indexes into this array
    /// using the method's position in `provider_trait_methods[T]`.
    /// Symbol name: `@VT_<U>_<T>`. Stored in `provider_vtables` keyed by
    /// `(U, T)` for `with_provider[R]` lookup.
    fn emit_provider_vtables(&mut self, program: &Program) {
        // Gather the set of provider trait names from the resource decls
        // walked earlier. Inherent impls (no trait) don't need vtables —
        // they're called directly by name.
        let provider_traits: HashSet<String> =
            self.provider_resource_traits.values().cloned().collect();
        if provider_traits.is_empty() {
            return;
        }

        let ptr_type = self.context.ptr_type(AddressSpace::default());
        for item in &program.items {
            let Item::ImplBlock(imp) = item else { continue };
            let Some(trait_path) = &imp.trait_name else {
                continue;
            };
            let Some(trait_name) = trait_path.segments.last().cloned() else {
                continue;
            };
            if !provider_traits.contains(&trait_name) {
                continue;
            }
            let Some(target_name) = impl_target_name(&imp.target_type) else {
                continue;
            };
            let Some(method_order) = self.provider_trait_methods.get(&trait_name).cloned() else {
                continue;
            };

            // Look up each method's compiled fn-ptr. Methods declared on
            // the impl but absent from the trait (extras) are ignored —
            // the vtable matches the trait's view. Trait methods missing
            // from the impl emit a null fn-ptr; calling such a vtable
            // slot would null-deref at runtime, but the typechecker
            // rejects partial impls so this case shouldn't reach codegen.
            let mut entries: Vec<inkwell::values::PointerValue<'ctx>> = Vec::new();
            for method_name in &method_order {
                let symbol = format!("{}.{}", target_name, method_name);
                let entry = match self.module.get_function(&symbol) {
                    Some(f) => f.as_global_value().as_pointer_value(),
                    None => ptr_type.const_null(),
                };
                entries.push(entry);
            }

            let vtable_array_ty = ptr_type.array_type(entries.len() as u32);
            let vtable_init = ptr_type.const_array(&entries);
            let vt_name = format!("VT_{}_{}", target_name, trait_name);
            let vt_global = self.module.add_global(vtable_array_ty, None, &vt_name);
            vt_global.set_initializer(&vtable_init);
            vt_global.set_linkage(Linkage::Internal);
            vt_global.set_constant(true);
            self.provider_vtables
                .insert((target_name, trait_name), vt_global);
        }
    }

    /// Theme 6 sub-step 3: lower `with_provider[R](provider, ||body)`.
    ///
    /// Generates:
    /// ```text
    ///   %frame = alloca ProviderFrame
    ///   %data = <pointer to provider value>
    ///   call void @karac_provider_push(%frame, <resource_id>, %data, @VT_<U>_<T>)
    ///   <body>                                    ; inlined closure body
    ///   call void @karac_provider_pop()
    ///   ; result = body's value
    /// ```
    ///
    /// The `ProviderFrame` is alloca'd on the entry block so each
    /// `with_provider` call site has its own per-invocation slot — the
    /// runtime only mutates head pointers, the storage is caller-owned.
    /// Restrictions for v1: the closure argument must be an inline
    /// `||body` literal (the canonical Parallax-lite shape); a named
    /// closure-binding form would require routing through the indirect
    /// closure-call path. The provider's impl-target type is inferred
    /// from a small set of receiver-shape patterns (identifier whose
    /// `var_type_names` is set, struct literal, shared-struct value);
    /// other shapes return a codegen error.
    fn compile_with_provider(
        &mut self,
        resource: &str,
        provider_expr: &Expr,
        closure_expr: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // 1. Resolve the resource ID and provider trait. Both must have
        //    been populated by the early walk over `Item::EffectResource`
        //    in `compile_program`; absence here means the resource
        //    name is bogus or the resource has no provider trait
        //    (`effect resource R;` without `: T`), which the typechecker
        //    should already reject before codegen runs.
        let resource_id = self
            .provider_resource_ids
            .get(resource)
            .copied()
            .ok_or_else(|| {
                format!(
                    "with_provider: unknown effect resource '{}' (no resource ID assigned)",
                    resource
                )
            })?;
        let trait_name = self
            .provider_resource_traits
            .get(resource)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "with_provider: resource '{}' has no provider trait — `with_provider` \
                     requires `effect resource {}: T` for some trait T",
                    resource, resource
                )
            })?;

        // 2. Infer the provider's impl-target type and look up its vtable.
        let provider_type_name = self
            .infer_provider_type_name(provider_expr)
            .ok_or_else(|| {
                format!(
                    "with_provider[{}]: cannot infer concrete provider type at codegen — \
                 supported shapes are an identifier with a known struct type or a \
                 struct literal",
                    resource
                )
            })?;
        let vt_global = self
            .provider_vtables
            .get(&(provider_type_name.clone(), trait_name.clone()))
            .copied()
            .ok_or_else(|| {
                format!(
                    "with_provider[{}]: no vtable found for `impl {} for {}` — check that \
                     the impl exists and `effect resource {}: {}` is declared at the top level",
                    resource, trait_name, provider_type_name, resource, trait_name
                )
            })?;

        // 3. Materialize a pointer to the provider's data. For shared
        //    structs, the loaded variable value IS the heap pointer
        //    (`{refcount, fields...}`); for value-type structs, take the
        //    storage alloca, or alloca-and-store a fresh value when the
        //    provider expression isn't a known identifier.
        let data_ptr = self.compile_provider_data_ptr(provider_expr, &provider_type_name)?;

        // 4. Alloca a `ProviderFrame` on the function entry block so the
        //    storage outlives the push/pop pair without re-alloca'ing
        //    on each loop iteration if a `with_provider` is in a loop.
        let fn_val = self.current_fn.ok_or_else(|| {
            "with_provider: no current function (called from top-level?)".to_string()
        })?;
        let frame_ptr = self.create_entry_alloca(fn_val, "wp.frame", self.provider_frame_ty.into());

        // 5. Push: karac_provider_push(frame, resource_id, data, vtable_ptr).
        let i32_t = self.context.i32_type();
        let id_v = i32_t.const_int(resource_id as u64, false);
        let vtable_ptr = vt_global.as_pointer_value();
        self.builder
            .build_call(
                self.karac_provider_push_fn,
                &[
                    frame_ptr.into(),
                    id_v.into(),
                    data_ptr.into(),
                    vtable_ptr.into(),
                ],
                "",
            )
            .unwrap();

        // 6. Inline the closure body. Only inline `||body` is supported in
        //    v1 — the body's free variables resolve against the outer
        //    scope, exactly as the interpreter handles a `with_provider`
        //    closure (see `Interpreter::eval_with_provider`).
        let body_result = self.compile_with_provider_body(closure_expr, resource)?;

        // 7. Pop: karac_provider_pop(). Matches the push; the runtime
        //    asserts head==frame and walks back to `frame.prev`.
        self.builder
            .build_call(self.karac_provider_pop_fn, &[], "")
            .unwrap();

        Ok(body_result)
    }

    /// Determine the concrete impl-target type name of a provider
    /// expression at codegen, used to look up the right `@VT_<U>_<T>`
    /// vtable. Supports:
    ///   - `ExprKind::Identifier(n)` whose `var_type_names[n]` is set
    ///     (covers `let p = MyProvider { ... }; with_provider[R](p, ...)`);
    ///   - `ExprKind::StructLit { name, ... }` for inline construction.
    /// Other shapes (function-return values, field projections, etc.)
    /// fall through and the caller emits a codegen error.
    fn infer_provider_type_name(&self, expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(n) => self.var_type_names.get(n.as_str()).cloned(),
            ExprKind::StructLiteral { path, .. } => path.last().cloned(),
            _ => None,
        }
    }

    /// Materialize a pointer to the provider value's data, suitable for
    /// passing to `karac_provider_push` and reading back as `*const Self`
    /// inside vtable methods.
    ///
    /// - **Shared struct provider:** the loaded value IS the heap pointer
    ///   (`{refcount, fields...}`). Vtable methods for shared structs
    ///   already know how to skip the refcount slot, so we pass the heap
    ///   pointer directly.
    /// - **Value-type struct provider, identifier receiver:** use the
    ///   variable's alloca pointer via `get_data_ptr`. This is in-place
    ///   (no copy), so mutations through `mut ref self` persist back to
    ///   the binding — same semantics as a direct method call.
    /// - **Value-type struct provider, struct-literal receiver (or
    ///   anything else):** alloca a fresh slot, store the compiled value,
    ///   and pass that. The lifetime of the alloca is the enclosing
    ///   function frame, so the runtime stack can hold the pointer for
    ///   the entire `with_provider` body without aliasing concerns.
    fn compile_provider_data_ptr(
        &mut self,
        expr: &Expr,
        type_name: &str,
    ) -> Result<inkwell::values::PointerValue<'ctx>, String> {
        if self.shared_types.contains_key(type_name) {
            let v = self.compile_expr(expr)?;
            let pv = v.into_pointer_value();
            return Ok(pv);
        }
        if let ExprKind::Identifier(name) = &expr.kind {
            if let Some(ptr) = self.get_data_ptr(name) {
                return Ok(ptr);
            }
        }
        let fn_val = self
            .current_fn
            .ok_or_else(|| "with_provider: no current function for provider alloca".to_string())?;
        let v = self.compile_expr(expr)?;
        let alloca = self.create_entry_alloca(fn_val, "wp.data", v.get_type());
        self.builder.build_store(alloca, v).unwrap();
        Ok(alloca)
    }

    /// Inline-compile the `with_provider` body closure. Only the
    /// `||body` literal form is supported — non-zero-arg closures would
    /// indicate a typechecker bug (the with_provider signature requires
    /// `() -> R`), and named closure values would need the indirect
    /// fat-pointer call path which v1 does not wire up here.
    fn compile_with_provider_body(
        &mut self,
        closure_expr: &Expr,
        resource: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ExprKind::Closure { params, body, .. } = &closure_expr.kind else {
            return Err(format!(
                "with_provider[{}]: closure argument must be an inline `||body` \
                 literal (named closure bindings unsupported in v1)",
                resource
            ));
        };
        if !params.is_empty() {
            return Err(format!(
                "with_provider[{}]: closure must take zero arguments, got {}",
                resource,
                params.len()
            ));
        }
        self.compile_expr(body)
    }

    /// Theme 6 sub-step 4: lower `R.method(args)` dispatch when `R` is an
    /// `effect resource R: T`. Returns `Some(value)` when dispatch fires;
    /// `None` when `name` isn't a known provider resource, in which case
    /// the caller falls through to `compile_assoc_call` (so non-resource
    /// `Type.method(...)` shapes — `Vec::new`, primitive ops, user
    /// `Type.method` — keep working unchanged).
    ///
    /// IR shape:
    /// ```text
    ///   %res = call %ProviderLookupResult @karac_provider_lookup(<id>)
    ///   %data = extractvalue %ProviderLookupResult %res, 0
    ///   %vt = extractvalue %ProviderLookupResult %res, 1
    ///   %fn_slot = getelementptr [N x ptr], ptr %vt, i64 0, i64 <method_idx>
    ///   %fn = load ptr, ptr %fn_slot
    ///   <ret> = call <FnTy> %fn(%data, <user_args>...)
    /// ```
    ///
    /// Method index comes from the trait's source-declaration order.
    /// The indirect-call FunctionType is borrowed from any concrete
    /// `<U>.<method>` symbol we already declared during impl-method
    /// declaration: every provider impl of the same trait method shares
    /// the same lowered LLVM signature (`*const Self` first arg lowers
    /// to `ptr`, primitives lower the same way regardless of `U`), so
    /// any one will do.
    ///
    /// v1 restriction: no scope-empty / null-vtable runtime check —
    /// the typechecker's effect-checker enforces `R` is in scope at
    /// every call site. A bug there or a programmatic misuse would
    /// crash via null-deref of the vtable load below; tightening this
    /// to a structured panic is a sub-step 6+ task.
    fn try_compile_provider_dispatch(
        &mut self,
        name: &str,
        method: &str,
        args: &[CallArg],
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        let Some(&resource_id) = self.provider_resource_ids.get(name) else {
            return Ok(None);
        };
        let Some(trait_name) = self.provider_resource_traits.get(name).cloned() else {
            // `effect resource R;` (no `: T`) — no dispatch possible.
            // Fall through to the regular assoc-call path so an
            // upstream typechecker error or a future R-as-ID use stays
            // observable.
            return Ok(None);
        };

        let method_order = self
            .provider_trait_methods
            .get(&trait_name)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "R.method dispatch: provider trait '{}' has no recorded method order \
                     (vtable emission and dispatch out of sync — codegen bug)",
                    trait_name
                )
            })?;
        let method_idx = method_order
            .iter()
            .position(|m| m == method)
            .ok_or_else(|| {
                format!(
                "R.method dispatch: '{}' is not a method of provider trait '{}' for resource '{}'",
                method, trait_name, name
            )
            })?;

        // Borrow the FunctionType from any impl of this trait method.
        // All impls of the same trait share the same lowered signature.
        let fn_type = self
            .provider_method_fn_type(&trait_name, method)
            .ok_or_else(|| {
                format!(
                    "R.method dispatch: no impl found for `{}::{}` — at least one \
                     `impl {} for U` must exist to populate the vtable",
                    trait_name, method, trait_name
                )
            })?;

        let i32_t = self.context.i32_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let id_v = i32_t.const_int(resource_id as u64, false);

        // 1. karac_provider_lookup(resource_id) → { data, vtable }.
        let lookup_call = self
            .builder
            .build_call(self.karac_provider_lookup_fn, &[id_v.into()], "wp.lookup")
            .unwrap();
        let lookup_sv = lookup_call
            .try_as_basic_value()
            .unwrap_basic()
            .into_struct_value();
        let data_ptr = self
            .builder
            .build_extract_value(lookup_sv, 0, "wp.lookup.data")
            .unwrap()
            .into_pointer_value();
        let vtable_ptr = self
            .builder
            .build_extract_value(lookup_sv, 1, "wp.lookup.vt")
            .unwrap()
            .into_pointer_value();

        // 2. GEP into the vtable for method_idx, load the fn pointer.
        //    Vtable layout is `[N x ptr]` per `emit_provider_vtables`,
        //    so the slot offset is just `method_idx` in pointer units.
        //    Use a flat offset GEP to avoid recomputing the array size.
        let idx_v = i32_t.const_int(method_idx as u64, false);
        let fn_slot = unsafe {
            self.builder
                .build_gep(ptr_ty, vtable_ptr, &[idx_v], "wp.fn.slot")
                .unwrap()
        };
        let fn_ptr = self
            .builder
            .build_load(ptr_ty, fn_slot, "wp.fn")
            .unwrap()
            .into_pointer_value();

        // 3. Build call args: data_ptr first, then user args.
        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
            vec![BasicMetadataValueEnum::from(data_ptr)];
        for a in args {
            let v = self.compile_expr(&a.value)?;
            call_args.push(BasicMetadataValueEnum::from(v));
        }

        // 4. Indirect call through the loaded fn pointer.
        let call = self
            .builder
            .build_indirect_call(fn_type, fn_ptr, &call_args, "wp.call")
            .unwrap();
        let basic = call.try_as_basic_value();
        if basic.is_instruction() {
            // void-returning method — fill the expression slot with
            // const-0 i64, mirroring how the user-impl-method dispatch
            // path handles unit-returning method calls.
            Ok(Some(self.context.i64_type().const_int(0, false).into()))
        } else {
            Ok(Some(basic.unwrap_basic()))
        }
    }

    /// Find the LLVM `FunctionType` for a provider trait method by
    /// looking up any concrete `<U>.<method>` symbol whose `(U, T)` pair
    /// is registered in `provider_vtables`. Returns `None` when no impl
    /// has been declared yet (which would mean the vtable couldn't have
    /// been emitted either — handled as a dispatch error by the caller).
    fn provider_method_fn_type(
        &self,
        trait_name: &str,
        method: &str,
    ) -> Option<inkwell::types::FunctionType<'ctx>> {
        for ((target, t), _) in &self.provider_vtables {
            if t == trait_name {
                let qualified = format!("{}.{}", target, method);
                if let Some(f) = self.module.get_function(&qualified) {
                    return Some(f.get_type());
                }
            }
        }
        None
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

    /// Apply `#[link_section("name")]`, `#[no_mangle]`, and `#[used]` to
    /// the given LLVM function. `link_section` calls
    /// `FunctionValue::set_section`. `no_mangle` is a no-op at this layer —
    /// the codegen already uses each function's source name as its LLVM
    /// symbol name (no mangling pass) so the attribute is purely an explicit
    /// programmer assertion that the symbol stays as-written; preserving it
    /// in the AST preserves the assertion for reviewers and any future
    /// mangler. `used` queues the symbol for inclusion in `@llvm.used`,
    /// emitted at the end of `compile_program` by `emit_llvm_used`.
    fn apply_linker_attrs(&mut self, fn_val: FunctionValue<'ctx>, attrs: &[Attribute]) {
        for attr in attrs {
            match attr.name.as_str() {
                "link_section" => {
                    // `#[link_section("name")]` — first positional arg or
                    // `string_value` carries the section literal. Skip
                    // silently when neither is present; the parser scaffolding
                    // accepts the attribute but does not yet enforce arg shape.
                    let section = attr.string_value.clone().or_else(|| {
                        attr.args.iter().find_map(|a| match a.value.as_ref() {
                            Some(Expr {
                                kind: ExprKind::StringLit(s),
                                ..
                            }) => Some(s.clone()),
                            _ => None,
                        })
                    });
                    if let Some(s) = section {
                        fn_val.as_global_value().set_section(Some(&s));
                    }
                }
                "no_mangle" => {
                    // No-op: codegen already emits the symbol under its
                    // source-level name. Tracked here so future mangling
                    // passes can opt out.
                }
                "used" if !self.used_symbols.contains(&fn_val) => {
                    self.used_symbols.push(fn_val);
                }
                _ => {}
            }
        }
    }

    fn declare_function(&mut self, func: &Function) -> Result<FunctionValue<'ctx>, String> {
        if func.name == "main" {
            let main_type = self.context.i32_type().fn_type(&[], false);
            return Ok(self.module.add_function("main", main_type, None));
        }

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

        // Record which params are ref for call-site argument passing.
        let ref_flags: Vec<bool> = func
            .params
            .iter()
            .map(|p| matches!(&p.ty.kind, TypeKind::Ref(_) | TypeKind::MutRef(_)))
            .collect();
        self.fn_param_ref.insert(func.name.clone(), ref_flags);
        // Record slice-param element types for call-site coercion.
        let slice_elems: Vec<Option<BasicTypeEnum<'ctx>>> = func
            .params
            .iter()
            .map(|p| self.extract_slice_elem_type(&p.ty))
            .collect();
        self.fn_param_slice_elem
            .insert(func.name.clone(), slice_elems);

        let fn_val = self.module.add_function(&func.name, fn_type, None);
        self.apply_linker_attrs(fn_val, &func.attributes);
        Ok(fn_val)
    }

    fn compile_function(&mut self, func: &Function) -> Result<(), String> {
        let fn_val = self
            .module
            .get_function(&func.name)
            .ok_or_else(|| format!("Function '{}' not declared", func.name))?;

        self.current_fn = Some(fn_val);
        self.current_fn_name = func.name.clone();
        self.variables.clear();
        self.var_type_names.clear();
        self.ref_params.clear();
        self.rc_fallback_heap_types.clear();
        self.scope_cleanup_actions.clear();
        self.scope_cleanup_actions.push(Vec::new());

        let entry = self.context.append_basic_block(fn_val, "entry");
        self.builder.position_at_end(entry);

        if func.name != "main" {
            for (i, param) in func.params.iter().enumerate() {
                let param_name = self.param_name(param);
                let param_val = fn_val.get_nth_param(i as u32).unwrap();
                let alloca = self.create_entry_alloca(fn_val, &param_name, param_val.get_type());
                self.builder.build_store(alloca, param_val).unwrap();
                // Track ref params: alloca holds a pointer-to-data.
                if let Some(inner_ty) = self.inner_type_of_ref(&param.ty) {
                    self.ref_params.insert(param_name.clone(), inner_ty);
                    // Also track vec_elem_types for ref Vec/String params.
                    if let TypeKind::Ref(inner) | TypeKind::MutRef(inner) = &param.ty.kind {
                        if let Some(elem) = self.extract_vec_elem_type(inner) {
                            self.vec_elem_types.insert(param_name.clone(), elem);
                            if let Some(inner_te) = vec_inner_type_expr(inner) {
                                self.var_elem_type_exprs
                                    .insert(param_name.clone(), inner_te);
                            }
                        }
                        if self.is_string_type_expr(inner) {
                            self.vec_elem_types
                                .insert(param_name.clone(), self.context.i8_type().into());
                        }
                    }
                }
                // Track slice params: both `Slice[T]` and `mut Slice[T]` use
                // the 2-field `{ptr, i64}` representation. Recording the
                // element type here lets indexing and iteration dispatch on
                // the slice shape.
                if let Some(elem) = self.extract_slice_elem_type(&param.ty) {
                    self.slice_elem_types.insert(param_name.clone(), elem);
                    if let Some(inner_te) = slice_inner_type_expr(&param.ty) {
                        self.var_elem_type_exprs
                            .insert(param_name.clone(), inner_te);
                    }
                }
                // Track Map params: both K and V LLVM types + per-position
                // TypeExprs so `for (k, v) in m` can register each binding.
                if let Some((k_ty, v_ty)) = self.extract_map_kv_types(&param.ty) {
                    self.map_key_types.insert(param_name.clone(), k_ty);
                    self.map_val_types.insert(param_name.clone(), v_ty);
                    if let Some(k_name) = Self::extract_map_key_name(&param.ty) {
                        self.map_key_type_names.insert(param_name.clone(), k_name);
                    }
                    if let Some((k_te, v_te)) = map_kv_type_exprs(&param.ty) {
                        self.map_key_type_exprs.insert(param_name.clone(), k_te);
                        self.var_elem_type_exprs.insert(param_name.clone(), v_te);
                    }
                }
                // Track the declared type name so field/variant lookups work on this param.
                // Both owned (`Type`) and ref-wrapped (`ref Type` / `mut ref Type`)
                // paths feed `var_type_names` with the inner struct/enum name —
                // `field_index_for` needs it to find the field index regardless of
                // whether the param is value-typed or pointer-typed.
                let path_for_type_name = match &param.ty.kind {
                    TypeKind::Path(p) => Some(p),
                    TypeKind::Ref(inner) | TypeKind::MutRef(inner) => match &inner.kind {
                        TypeKind::Path(p) => Some(p),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(path) = path_for_type_name {
                    if let Some(type_name) = path.segments.first() {
                        self.var_type_names
                            .insert(param_name.clone(), type_name.clone());
                        // rc_inc for shared-type parameters (caller keeps its
                        // reference). Only fires for owned Path params — a
                        // shared-typed `ref T` doesn't take ownership, so no
                        // refcount bump.
                        if matches!(&param.ty.kind, TypeKind::Path(_)) {
                            if let Some(info) = self.shared_types.get(type_name.as_str()).cloned() {
                                let ptr = param_val.into_pointer_value();
                                self.emit_refcount_inc(&param_name, info.heap_type, ptr);
                                self.track_rc_var(&param_name, ptr, info.heap_type);
                            }
                        }
                    }
                }
                // RC-fallback boxing for non-shared, non-Vec parameters flagged by the
                // ownership checker. The param value is boxed in {i64 rc, T} on the heap
                // so multiple "consumers" each get a copy of T and the heap object is freed
                // at scope exit when the refcount reaches zero.
                let is_ref_param = self.ref_params.contains_key(&param_name);
                let is_vec_param = self.vec_elem_types.contains_key(&param_name);
                let is_shared_param = if let TypeKind::Path(path) = &param.ty.kind {
                    path.segments
                        .first()
                        .is_some_and(|n| self.shared_types.contains_key(n.as_str()))
                } else {
                    false
                };
                if !is_ref_param
                    && !is_vec_param
                    && !is_shared_param
                    && self.is_rc_fallback_binding(&param_name)
                {
                    let val_ty = param_val.get_type();
                    let heap_type = self
                        .context
                        .struct_type(&[self.context.i64_type().into(), val_ty], false);
                    let heap_ptr = self.emit_rc_alloc(heap_type);
                    let val_field = self
                        .builder
                        .build_struct_gep(heap_type, heap_ptr, 1, "rc_fb_param_val")
                        .unwrap();
                    self.builder.build_store(val_field, param_val).unwrap();
                    // Overwrite alloca to hold heap ptr instead of T.
                    let ptr_ty = self.context.ptr_type(AddressSpace::default());
                    let ptr_alloca = self.create_entry_alloca(fn_val, &param_name, ptr_ty.into());
                    self.builder.build_store(ptr_alloca, heap_ptr).unwrap();
                    self.rc_fallback_heap_types
                        .insert(param_name.clone(), heap_type);
                    self.track_rc_var(&param_name, heap_ptr, heap_type);
                    self.variables.insert(
                        param_name,
                        VarSlot {
                            ptr: ptr_alloca,
                            ty: ptr_ty.into(),
                        },
                    );
                    continue;
                }
                self.variables.insert(
                    param_name,
                    VarSlot {
                        ptr: alloca,
                        ty: param_val.get_type(),
                    },
                );
            }
        }

        // Slice 2 (auto-par codegen MVP): route the function body through
        // `compile_function_body`, which dispatches inferred parallel
        // groups to `karac_par_run` when a `ConcurrencyAnalysis` was
        // threaded into codegen. With no analysis, `compile_function_body`
        // falls through to `compile_block` and behavior is unchanged.
        let result = self.compile_function_body(&func.body)?;

        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.emit_scope_cleanup();
            if func.name == "main" {
                let zero = self.context.i32_type().const_int(0, false);
                self.builder.build_return(Some(&zero)).unwrap();
            } else if let Some(val) = result {
                self.builder.build_return(Some(&val)).unwrap();
            } else {
                self.builder.build_return(None).unwrap();
            }
        }

        self.scope_cleanup_actions.clear();
        Ok(())
    }

    fn compile_block(&mut self, block: &Block) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        for stmt in &block.stmts {
            self.compile_stmt(stmt)?;
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_some()
            {
                return Ok(None);
            }
        }
        if let Some(ref expr) = block.final_expr {
            let val = self.compile_expr(expr)?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    /// Compile a function's top-level body, dispatching inferred parallel
    /// groups to `karac_par_run` (slice 2 — auto-par codegen MVP).
    ///
    /// Mirrors `compile_block` for the no-analysis path; on top of that,
    /// when the concurrency analysis identifies non-trivial parallel
    /// groups for the current function, the matching contiguous-or-not
    /// stmt sets are batched into a single `emit_par_run` call instead of
    /// being emitted sequentially. Trivial groups (per `is_trivial`) are
    /// skipped — their statements still emit sequentially. This is the
    /// only call site that consumes `parallel_groups_for_current_fn`;
    /// nested blocks (let-RHS, if-arms, loop bodies) keep flowing through
    /// plain `compile_block` because the analyzer's stmt indices only
    /// reference `func.body.stmts`.
    ///
    /// Hard-stop trigger 2 mitigation: a top-level `par {}` stmt has its
    /// inner effects collected by the analyzer (`collect_block_effects`
    /// in `concurrency.rs`), so an effectful par-block already serializes
    /// against neighbors. To stay defensive against pure par-block stmts
    /// being grouped, we drop any group that contains a par-block stmt —
    /// re-parallelizing an already-parallel block would be wasteful at
    /// best and semantically wrong at worst.
    #[allow(clippy::result_large_err)]
    fn compile_function_body(
        &mut self,
        body: &Block,
    ) -> Result<Option<BasicValueEnum<'ctx>>, String> {
        // Slice 6 (Parallax-lite workload): `KARAC_AUTO_PAR=0` flips
        // `auto_par_disabled` on, short-circuiting all parallel-group
        // dispatch back to plain sequential `compile_block`. This is
        // the gate for side-by-side wall-clock benchmarking of auto-par
        // vs sequential codegen on the same workload. The default
        // (auto-par on) is unchanged — gate-on programs continue to
        // hit the parallel-group dispatch path below.
        if self.auto_par_disabled {
            return self.compile_block(body);
        }

        // Snapshot the analysis up front to release the borrow on `self`
        // before the loop calls `&mut self` methods (`compile_stmt`,
        // `emit_par_run`). The clone is cheap — `ParallelGroup` holds a
        // small `Vec<usize>`, a short `String` reason, and a bool.
        let decision = self.parallel_groups_for_current_fn().cloned();

        let Some(decision) = decision else {
            return self.compile_block(body);
        };

        // Defensive guard: the analyzer walks `func.body.stmts` directly,
        // so its indices should always be in bounds. A `debug_assert!`
        // catches future drift between the analysis and codegen views of
        // the function body without paying the cost in release builds.
        let n = body.stmts.len();
        debug_assert!(
            decision
                .parallel_groups
                .iter()
                .all(|g| g.statement_indices.iter().all(|&i| i < n)),
            "parallel_groups statement_indices out of bounds for function body (len={n})"
        );

        // Build group-start and covered-index lookups. Trivial groups
        // (per the granularity heuristic) are skipped — their stmts emit
        // sequentially as if no group existed. Groups containing an
        // explicit `par {}` stmt are also skipped (hard-stop trigger 2
        // mitigation: don't re-parallelize an already-parallel block).
        // Groups that define a binding consumed *outside* the group are
        // also skipped: the existing `karac_par_run` lowering captures
        // bindings *into* branch fns but does not propagate let-bindings
        // *out*, so a let inside a group is invisible to later stmts in
        // the function body. Capturing-back is out of scope for slice 2
        // (the explicit-par lowering has the same limitation today; see
        // the "Out of scope" entries on `compile_par_block`).
        let mut group_starts: HashMap<usize, &ParallelGroup> = HashMap::new();
        let mut covered: HashSet<usize> = HashSet::new();
        for group in &decision.parallel_groups {
            if group.is_trivial {
                continue;
            }
            if group
                .statement_indices
                .iter()
                .any(|&i| i < n && Self::stmt_is_par_block(&body.stmts[i]))
            {
                continue;
            }
            if self.group_defines_binding_used_outside(group, body) {
                continue;
            }
            let Some(&min_idx) = group.statement_indices.iter().min() else {
                continue;
            };
            group_starts.insert(min_idx, group);
            for &i in &group.statement_indices {
                covered.insert(i);
            }
        }

        let mut i = 0;
        while i < n {
            if let Some(group) = group_starts.get(&i) {
                let group_stmts: Vec<Stmt> = group
                    .statement_indices
                    .iter()
                    .map(|&s| body.stmts[s].clone())
                    .collect();
                // Slice 3 (sub-step d.1): pass a per-group span so the
                // SpawnSiteRecord pinned by `emit_par_run`'s call to
                // `record_spawn_site` carries the location of the first
                // grouped stmt — the conceptual fire-point of the
                // inferred `par_run` — rather than the whole function-
                // body span (slice 2's MVP).
                let group_span = body.stmts[group.statement_indices[0]].span.clone();
                self.emit_par_run(&group_stmts, &group_span)?;
                let max_idx = group.statement_indices.iter().copied().max().unwrap_or(i);
                i = max_idx + 1;
            } else if covered.contains(&i) {
                // Mid-group index already emitted as part of an earlier
                // group-start dispatch.
                i += 1;
            } else {
                self.compile_stmt(&body.stmts[i])?;
                i += 1;
            }
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_some()
            {
                return Ok(None);
            }
        }

        if let Some(ref expr) = body.final_expr {
            let val = self.compile_expr(expr)?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    /// True iff `stmt` is a top-level `par { ... }` expression statement.
    /// Used in `compile_function_body` to skip auto-par groups that would
    /// otherwise re-parallelize an already-parallel block.
    fn stmt_is_par_block(stmt: &Stmt) -> bool {
        matches!(&stmt.kind, StmtKind::Expr(e) if matches!(&e.kind, ExprKind::Par(_)))
    }

    /// True iff any binding defined by a stmt in `group` is read by a
    /// stmt outside the group (or by `body.final_expr`). The existing
    /// `karac_par_run` lowering captures variables *into* branch fns
    /// but does not propagate let-bindings *out*, so a let inside an
    /// auto-par group would be invisible to later stmts in the function
    /// body. This guard is conservative — slice 2 deliberately leaves
    /// capture-back as out-of-scope; it matches the same limitation
    /// the explicit `par {}` codegen has today.
    fn group_defines_binding_used_outside(&self, group: &ParallelGroup, body: &Block) -> bool {
        // 1. Collect names defined by stmts in this group.
        let in_group: HashSet<usize> = group.statement_indices.iter().copied().collect();
        let mut defined: HashSet<String> = HashSet::new();
        for &idx in &in_group {
            if idx >= body.stmts.len() {
                continue;
            }
            match &body.stmts[idx].kind {
                StmtKind::Let { pattern, .. } | StmtKind::LetElse { pattern, .. } => {
                    for name in pattern.binding_names() {
                        defined.insert(name);
                    }
                }
                StmtKind::LetUninit { name, .. } => {
                    defined.insert(name.clone());
                }
                _ => {}
            }
        }
        if defined.is_empty() {
            return false;
        }

        // 2. Walk every other stmt + final_expr collecting reads.
        let mut refs: HashSet<String> = HashSet::new();
        let mut defs: HashSet<String> = HashSet::new();
        for (idx, stmt) in body.stmts.iter().enumerate() {
            if in_group.contains(&idx) {
                continue;
            }
            match &stmt.kind {
                StmtKind::Let { pattern, value, .. } | StmtKind::LetElse { pattern, value, .. } => {
                    self.refs_in_expr(value, &mut refs, &mut defs);
                    for name in pattern.binding_names() {
                        defs.insert(name);
                    }
                }
                StmtKind::Expr(e) => self.refs_in_expr(e, &mut refs, &mut defs),
                StmtKind::Assign { target, value } => {
                    self.refs_in_expr(target, &mut refs, &mut defs);
                    self.refs_in_expr(value, &mut refs, &mut defs);
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    self.refs_in_expr(target, &mut refs, &mut defs);
                    self.refs_in_expr(value, &mut refs, &mut defs);
                }
                _ => {}
            }
        }
        if let Some(e) = &body.final_expr {
            self.refs_in_expr(e, &mut refs, &mut defs);
        }

        defined.iter().any(|n| refs.contains(n))
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), String> {
        match &stmt.kind {
            StmtKind::Let {
                pattern, value, ty, ..
            } => {
                // Track Vec/String element types from type annotation or RHS.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    let mut detected = false;
                    // Explicit type annotation: let v: Vec[T] = ... or let s: String = ...
                    if let Some(ref te) = ty {
                        if let Some(elem_ty) = self.extract_vec_elem_type(te) {
                            self.vec_elem_types.insert(var_name.clone(), elem_ty);
                            if let Some(inner) = vec_inner_type_expr(te) {
                                self.var_elem_type_exprs.insert(var_name.clone(), inner);
                            }
                            detected = true;
                        }
                        if self.is_string_type_expr(te) {
                            self.vec_elem_types
                                .insert(var_name.clone(), self.context.i8_type().into());
                            detected = true;
                        }
                        if let Some(elem_ty) = self.extract_slice_elem_type(te) {
                            self.slice_elem_types.insert(var_name.clone(), elem_ty);
                            if let Some(inner) = slice_inner_type_expr(te) {
                                self.var_elem_type_exprs.insert(var_name.clone(), inner);
                            }
                            detected = true;
                        }
                        if let Some((k_ty, v_ty)) = self.extract_map_kv_types(te) {
                            self.map_key_types.insert(var_name.clone(), k_ty);
                            self.map_val_types.insert(var_name.clone(), v_ty);
                            if let Some(k_name) = Self::extract_map_key_name(te) {
                                self.map_key_type_names.insert(var_name.clone(), k_name);
                            }
                            if let Some((k_te, v_te)) = map_kv_type_exprs(te) {
                                self.map_key_type_exprs.insert(var_name.clone(), k_te);
                                self.var_elem_type_exprs.insert(var_name.clone(), v_te);
                            }
                            detected = true;
                        }
                        if let Some(elem_ty) = self.extract_set_elem_type(te) {
                            self.set_elem_types.insert(var_name.clone(), elem_ty);
                            if let Some(elem_name) = Self::extract_set_elem_name(te) {
                                self.set_elem_type_names.insert(var_name.clone(), elem_name);
                            }
                            if let Some(elem_te) = set_inner_type_expr(te) {
                                self.set_elem_type_exprs.insert(var_name.clone(), elem_te);
                            }
                            detected = true;
                        }
                    }
                    // Infer String from RHS: let s = "hello", let s = String::new(),
                    // or let s = a + b (string concat)
                    if !detected
                        && (matches!(&value.kind, ExprKind::StringLit(_))
                            || self.is_string_new_call(value)
                            || self.is_string_binary_op(value))
                    {
                        self.vec_elem_types
                            .insert(var_name.clone(), self.context.i8_type().into());
                    }
                    // Debugger Contract slice 5: register `let v =
                    // Runtime.list_par_blocks()` / `Runtime.list_tasks()`
                    // as a Vec-shaped binding so subsequent `v.len()` /
                    // `v.is_empty()` etc. dispatch through `compile_vec_method`.
                    // The element type is opaque from codegen's perspective
                    // (the baked-stdlib `ParBlockInfo` / `TaskInfo` structs
                    // aren't in `program.items` — see compile_program line
                    // 2720+). Using `i64` as a placeholder element type
                    // keeps Vec dispatch working for the v1 contract
                    // surface (`.len()` / `.is_empty()` ignore element
                    // type). Field access (`pb.spawn_site_id`) is a v1.x
                    // follow-up that requires registering the baked struct
                    // types.
                    if !detected && self.is_runtime_introspection_call(value) {
                        self.vec_elem_types
                            .insert(var_name.clone(), self.context.i64_type().into());
                    }
                    // Infer Slice element type from RHS shapes that produce
                    // a slice: `x.as_slice()` / `x.as_slice_mut()` on a known
                    // sequence variable, and `x[a..b]` range indexing.
                    if !self.slice_elem_types.contains_key(var_name.as_str()) {
                        if let Some(elem) = self.infer_slice_elem_from_rhs(value) {
                            self.slice_elem_types.insert(var_name.clone(), elem);
                        }
                    }
                }
                // SoA layout: if variable matches a layout name and RHS is Vec::new(),
                // produce the SoA struct type instead of the normal Vec.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let Some(soa) = self.soa_layouts.get(var_name.as_str()).cloned() {
                        if self.is_vec_new_call(value) {
                            return self.compile_soa_new(var_name, &soa);
                        }
                    }
                }
                // Map.new(): emit karac_map_new with sizes and (stub) fn pointers.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if self.is_map_new_call(value)
                        && self.map_key_types.contains_key(var_name.as_str())
                    {
                        let name = var_name.clone();
                        return self.compile_map_new_stmt(&name);
                    }
                }
                // Set.new(): emit karac_map_new with val_size = 0. Set[T]
                // lowers to Map[T, ()] at codegen — the C runtime handles
                // val_size = 0 correctly via `(key_size + val_size).max(1)`.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if self.is_set_new_call(value)
                        && self.set_elem_types.contains_key(var_name.as_str())
                    {
                        let name = var_name.clone();
                        return self.compile_set_new_stmt(&name);
                    }
                }
                // Map literal: `let m: Map[K, V] = ["k": v, ...]` (bare) or
                // `Map[k: v, ...]` (prefix). Both lower to `ExprKind::MapLiteral`.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let ExprKind::MapLiteral(entries) = &value.kind {
                        if self.map_key_types.contains_key(var_name.as_str()) {
                            let name = var_name.clone();
                            let entries = entries.clone();
                            return self.compile_map_literal_stmt(&name, &entries);
                        }
                    }
                }
                // Zero-init repeat literal fast path: `let buf: Array[T, N] = [0; N]`
                // (and `[false; N]`, `[0.0; N]`, etc. — any literal-zero RHS) is
                // lowered to alloca + `llvm.memset`, bypassing the aggregate-value
                // round-trip. The standard path emits `store [N x T] zeroinitializer`
                // which LLVM's downstream codegen passes crash on at N≥80K (verified
                // SIGSEGV in `write_to_file`); the memset path is correct at any N
                // and is what LLVM would lower the aggregate store to anyway.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if let Some(result) =
                        self.try_emit_zero_init_array_let(var_name, value, ty.as_ref())
                    {
                        return result;
                    }
                }
                // Prefer the explicit type annotation when present — it lets
                // `let c: Cm = i.into();` (lowered to `Cm.from(i)`, which
                // `type_name_of` can't classify) still register `c` as a
                // `Cm` so field accesses resolve.
                let type_hint = ty
                    .as_ref()
                    .and_then(|te| match &te.kind {
                        TypeKind::Path(p) => p.segments.last().cloned(),
                        _ => None,
                    })
                    .or_else(|| self.type_name_of(value));
                self.pending_closure_fn_type = None;
                let is_fresh_construction = matches!(&value.kind, ExprKind::StructLiteral { .. });
                let val = self.compile_expr(value)?;
                // Track variable → type name for field resolution.
                let mut shared_info: Option<(String, SharedTypeInfo<'ctx>)> = None;
                if let Some(ref type_name) = type_hint {
                    if let PatternKind::Binding(var_name) = &pattern.kind {
                        self.var_type_names
                            .insert(var_name.clone(), type_name.clone());
                        if let Some(info) = self.shared_types.get(type_name.as_str()).cloned() {
                            shared_info = Some((var_name.clone(), info));
                        }
                    }
                }
                // Fallback: when there is no type annotation and the RHS is a
                // call (or any expression `type_name_of` can't classify), but
                // the compiled value is a struct, reverse-lookup the type by
                // its LLVM struct identity. Lets `let f = Foo.default()` —
                // UFCS associated-fn calls returning the impl owner — still
                // populate `var_type_names` so `f.value` field access
                // resolves correctly.
                if type_hint.is_none() {
                    if let (BasicValueEnum::StructValue(sv), PatternKind::Binding(var_name)) =
                        (&val, &pattern.kind)
                    {
                        let st = sv.get_type();
                        if let Some((name, _)) = self.struct_types.iter().find(|(_, ty)| **ty == st)
                        {
                            let name = name.clone();
                            self.var_type_names.insert(var_name.clone(), name);
                        }
                    }
                }
                // For shared types: rc_inc when copying from another variable (not fresh construction).
                if let Some((ref var_name, ref info)) = shared_info {
                    if !is_fresh_construction {
                        // Copying a shared pointer — increment refcount.
                        let ptr = val.into_pointer_value();
                        self.emit_refcount_inc(var_name, info.heap_type, ptr);
                    }
                    // Track for scope-exit cleanup.
                    let ptr = val.into_pointer_value();
                    self.track_rc_var(var_name, ptr, info.heap_type);
                }
                // RC-fallback boxing: heap-box non-shared bindings flagged by the ownership checker.
                // Skipped for Vec/String bindings (their inner buffers need separate cleanup).
                let val = if let PatternKind::Binding(var_name) = &pattern.kind {
                    let is_vec = self.vec_elem_types.contains_key(var_name.as_str());
                    if shared_info.is_none() && !is_vec && self.is_rc_fallback_binding(var_name) {
                        let val_ty = val.get_type();
                        let heap_type = self
                            .context
                            .struct_type(&[self.context.i64_type().into(), val_ty], false);
                        let heap_ptr = self.emit_rc_alloc(heap_type);
                        let val_field = self
                            .builder
                            .build_struct_gep(heap_type, heap_ptr, 1, "rc_fb_val")
                            .unwrap();
                        self.builder.build_store(val_field, val).unwrap();
                        self.rc_fallback_heap_types
                            .insert(var_name.clone(), heap_type);
                        self.track_rc_var(var_name, heap_ptr, heap_type);
                        heap_ptr.into()
                    } else {
                        val
                    }
                } else {
                    val
                };
                // Register closure function type under bound names.
                if let Some(fn_type) = self.pending_closure_fn_type.take() {
                    for bound_name in pattern.binding_names() {
                        self.closure_fn_types.insert(bound_name, fn_type);
                    }
                }
                self.bind_pattern(pattern, val)?;
                // Track Vec variables for scope cleanup.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    if self.vec_elem_types.contains_key(var_name.as_str()) {
                        if let Some(slot) = self.variables.get(var_name.as_str()) {
                            self.track_vec_var(slot.ptr);
                        }
                    }
                }
                // Track Map/Set variables when the RHS is a fresh-handle-producing
                // method call (`clone`, `union`, `intersection`, `difference`).
                // `Map.new()` / `Set.new()` / map-literal RHS shapes already track
                // via their early-return paths above; `let n = m;` (move) bypasses
                // this since it's an Identifier RHS, not a MethodCall, so the
                // source's existing track stays the unique cleanup owner.
                if let PatternKind::Binding(var_name) = &pattern.kind {
                    let fresh_handle = matches!(
                        &value.kind,
                        ExprKind::MethodCall { method, .. }
                            if matches!(
                                method.as_str(),
                                "clone" | "union" | "intersection" | "difference"
                            )
                    );
                    if fresh_handle
                        && (self.map_key_types.contains_key(var_name.as_str())
                            || self.set_elem_types.contains_key(var_name.as_str()))
                    {
                        if let Some(slot) = self.variables.get(var_name.as_str()) {
                            self.track_map_var(slot.ptr);
                        }
                    }
                }
                Ok(())
                // (`Set.new()` and `Map.new()` register their own
                // `FreeMapHandle` cleanup inside `compile_set_new_stmt` /
                // `compile_map_new_stmt` — those are early returns so
                // they don't reach this fallback.)
            }
            StmtKind::Expr(expr) => {
                self.compile_expr(expr)?;
                Ok(())
            }
            StmtKind::Assign { target, value } => {
                let val = self.compile_expr(value)?;
                if let ExprKind::Identifier(name) = &target.kind {
                    // For shared types: rc_dec old value, rc_inc new value.
                    if let Some(type_name) = self.var_type_names.get(name).cloned() {
                        if let Some(info) = self.shared_types.get(&type_name).cloned() {
                            if let Some(slot) = self.variables.get(name).copied() {
                                // rc_dec old pointer
                                let old_ptr = self
                                    .builder
                                    .build_load(
                                        self.context.ptr_type(AddressSpace::default()),
                                        slot.ptr,
                                        "old_rc",
                                    )
                                    .unwrap()
                                    .into_pointer_value();
                                self.emit_refcount_dec(name, info.heap_type, old_ptr);
                                // rc_inc new pointer
                                let new_ptr = val.into_pointer_value();
                                self.emit_refcount_inc(name, info.heap_type, new_ptr);
                                self.builder.build_store(slot.ptr, val).unwrap();
                                return Ok(());
                            }
                        }
                    }
                    if let Some(slot) = self.variables.get(name).copied() {
                        self.builder.build_store(slot.ptr, val).unwrap();
                    }
                } else if let ExprKind::FieldAccess { object, field } = &target.kind {
                    self.compile_field_store(object, field, val)?;
                } else if let ExprKind::Index { object, index } = &target.kind {
                    self.compile_index_store(object, index, val)?;
                } else if let ExprKind::Unary {
                    op: UnaryOp::Deref,
                    operand,
                } = &target.kind
                {
                    // `*r = val` — store through the mut-ref pointer.
                    // get_data_ptr loads the raw pointer from the alloca (one
                    // load, not two), giving us the address to store into.
                    if let ExprKind::Identifier(name) = &operand.kind {
                        if let Some(ptr) = self.get_data_ptr(name) {
                            self.builder.build_store(ptr, val).unwrap();
                        }
                    }
                }
                Ok(())
            }
            StmtKind::CompoundAssign { target, op, value } => {
                if let ExprKind::Identifier(name) = &target.kind {
                    let current = self.load_variable(name)?;
                    let rhs = self.compile_expr(value)?;
                    let binop = match op {
                        CompoundOp::Add => BinOp::Add,
                        CompoundOp::Sub => BinOp::Sub,
                        CompoundOp::Mul => BinOp::Mul,
                        CompoundOp::Div => BinOp::Div,
                        CompoundOp::Mod => BinOp::Mod,
                        CompoundOp::BitAnd => BinOp::BitAnd,
                        CompoundOp::BitOr => BinOp::BitOr,
                        CompoundOp::BitXor => BinOp::BitXor,
                        CompoundOp::Shl => BinOp::Shl,
                        CompoundOp::Shr => BinOp::Shr,
                    };
                    let result = self.compile_binop(&binop, current, rhs)?;
                    if let Some(slot) = self.variables.get(name).copied() {
                        self.builder.build_store(slot.ptr, result).unwrap();
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, val: BasicValueEnum<'ctx>) -> Result<(), String> {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                let fn_val = self.current_fn.unwrap();
                let alloca = self.create_entry_alloca(fn_val, name, val.get_type());
                self.builder.build_store(alloca, val).unwrap();
                self.variables.insert(
                    name.clone(),
                    VarSlot {
                        ptr: alloca,
                        ty: val.get_type(),
                    },
                );
                Ok(())
            }
            PatternKind::Wildcard => Ok(()),
            // Struct destructuring: let Foo { x, y } = val
            PatternKind::Struct { path: _, fields } => {
                if let BasicValueEnum::StructValue(sv) = val {
                    for (idx, field_pat) in fields.iter().enumerate() {
                        let field_val = self
                            .builder
                            .build_extract_value(sv, idx as u32, "field")
                            .unwrap();
                        if let Some(pat) = &field_pat.pattern {
                            self.bind_pattern(pat, field_val)?;
                        } else {
                            // Shorthand `Foo { x }` — bind field name as variable
                            let fn_val = self.current_fn.unwrap();
                            let alloca = self.create_entry_alloca(
                                fn_val,
                                &field_pat.name,
                                field_val.get_type(),
                            );
                            self.builder.build_store(alloca, field_val).unwrap();
                            self.variables.insert(
                                field_pat.name.clone(),
                                VarSlot {
                                    ptr: alloca,
                                    ty: field_val.get_type(),
                                },
                            );
                        }
                    }
                }
                Ok(())
            }
            // Tuple destructuring: let (a, b) = val
            PatternKind::Tuple(pats) => {
                if let BasicValueEnum::StructValue(sv) = val {
                    for (idx, pat) in pats.iter().enumerate() {
                        let elem = self
                            .builder
                            .build_extract_value(sv, idx as u32, "elem")
                            .unwrap();
                        self.bind_pattern(pat, elem)?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<BasicValueEnum<'ctx>, String> {
        match &expr.kind {
            ExprKind::Integer(n, sfx) => Ok(self.const_int_for_suffix(*n, *sfx).into()),
            ExprKind::Float(f, sfx) => Ok(self.const_float_for_suffix(*f, *sfx).into()),
            ExprKind::Bool(b) => Ok(self
                .context
                .bool_type()
                .const_int(u64::from(*b), false)
                .into()),
            ExprKind::StringLit(s) => {
                let global = self.builder.build_global_string_ptr(s, "str").unwrap();
                let str_ty = self.vec_struct_type();
                let i64_t = self.context.i64_type();
                let len = i64_t.const_int(s.len() as u64, false);
                let cap_zero = i64_t.const_int(0, false); // cap=0 → static, don't free
                let mut agg = str_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, global.as_pointer_value(), 0, "str.data")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, len, 1, "str.len")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, cap_zero, 2, "str.cap")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            ExprKind::InterpolatedStringLit(parts) => {
                // Build an empty String alloca, then append each part.
                let vec_ty = self.vec_struct_type();
                let i64_t = self.context.i64_type();
                let ptr_ty = self.context.ptr_type(AddressSpace::default());
                let fn_val = self.current_fn.unwrap();

                let acc = self.create_entry_alloca(fn_val, "fstr.acc", vec_ty.into());
                // Initialize: {null, 0, 0} — empty heap string.
                let null = ptr_ty.const_null();
                let zero = i64_t.const_int(0, false);
                let data_pp = self
                    .builder
                    .build_struct_gep(vec_ty, acc, 0, "fstr.data.pp")
                    .unwrap();
                let len_p = self
                    .builder
                    .build_struct_gep(vec_ty, acc, 1, "fstr.len.p")
                    .unwrap();
                let cap_p = self
                    .builder
                    .build_struct_gep(vec_ty, acc, 2, "fstr.cap.p")
                    .unwrap();
                self.builder.build_store(data_pp, null).unwrap();
                self.builder.build_store(len_p, zero).unwrap();
                self.builder.build_store(cap_p, zero).unwrap();

                // Register acc for scope cleanup (non-zero cap will be freed).
                // vec_ty is the same struct type used for Vec/String.
                self.track_vec_var(acc);

                for part in parts {
                    match part {
                        ParsedInterpolationPart::Text(text) => {
                            if !text.is_empty() {
                                let gptr = self
                                    .builder
                                    .build_global_string_ptr(text, "fstr.text")
                                    .unwrap();
                                let text_len = i64_t.const_int(text.len() as u64, false);
                                self.emit_string_append_raw(acc, gptr.as_pointer_value(), text_len);
                            }
                        }
                        ParsedInterpolationPart::Expr(e) => {
                            let val = self.compile_expr(e)?;
                            let (src_ptr, src_len) = self.compile_fstr_part_to_cstr(val);
                            self.emit_string_append_raw(acc, src_ptr, src_len);
                        }
                    }
                }

                // Load the final String struct from the accumulator alloca.
                let result = self.builder.build_load(vec_ty, acc, "fstr.result").unwrap();
                Ok(result)
            }
            ExprKind::Identifier(name) => {
                // Resolution order: local variable (may shadow a const),
                // then unit enum variant, then top-level `const` (re-compile
                // the stored value expression at this use site so LLVM
                // folds it), and finally `load_variable` so the existing
                // "Undefined variable" diagnostic still fires for genuinely
                // unbound names.
                if self.variables.contains_key(name.as_str()) {
                    self.load_variable(name)
                } else if let Some(ev) = self.try_unit_enum_variant(name) {
                    Ok(ev)
                } else if let Some(const_value) = self.consts.get(name).cloned() {
                    self.compile_expr(&const_value)
                } else {
                    self.load_variable(name)
                }
            }
            ExprKind::SelfValue => {
                // `self` is bound as an ordinary local by `compile_function`'s
                // parameter loop (impl methods prepend a `self: Type` param).
                self.load_variable("self")
            }
            ExprKind::Binary { op, left, right } => {
                let lhs = self.compile_expr(left)?;
                let rhs = self.compile_expr(right)?;
                self.compile_binop(op, lhs, rhs)
            }
            ExprKind::Unary { op, operand } => {
                if matches!(op, UnaryOp::Deref) {
                    // `*r` — load the value the reference points to.
                    // `load_variable` already performs the two-step dereference
                    // for ref/mut-ref params (load alloca → load through ptr),
                    // so `compile_expr(operand)` already yields the inner value.
                    // Just return it directly.
                    return self.compile_expr(operand);
                }
                let val = self.compile_expr(operand)?;
                self.compile_unaryop(op, val)
            }
            ExprKind::Call { callee, args } => self.compile_call(callee, args),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => self.compile_if(condition, then_block, else_branch.as_deref()),
            ExprKind::While {
                condition, body, ..
            } => self.compile_while(condition, body),
            ExprKind::Loop { body, .. } => self.compile_loop(body),
            ExprKind::Break { value, .. } => self.compile_break(value.as_deref()),
            ExprKind::Continue { .. } => self.compile_continue(),
            ExprKind::Closure { params, body, .. } => self.compile_closure(params, body),
            ExprKind::Return(val) => {
                if let Some(e) = val {
                    let v = self.compile_expr(e)?;
                    self.builder.build_return(Some(&v)).unwrap();
                } else {
                    self.builder.build_return(None).unwrap();
                }
                Ok(self.context.i64_type().const_int(0, false).into())
            }
            ExprKind::Block(block) | ExprKind::Seq(block) => match self.compile_block(block)? {
                Some(v) => Ok(v),
                None => Ok(self.context.i64_type().const_int(0, false).into()),
            },
            ExprKind::FieldAccess { object, field } => self.compile_field_access(object, field),
            ExprKind::StructLiteral { path, fields, .. } => {
                let name = path.last().map(|s| s.as_str()).unwrap_or("");
                self.compile_struct_init(name, fields)
            }
            ExprKind::ArrayLiteral(elems) => self.compile_array_literal(elems),
            ExprKind::RepeatLiteral {
                type_name,
                value,
                count,
            } => self.compile_repeat_literal(type_name.as_deref(), value, count),
            ExprKind::Tuple(elems) => self.compile_tuple(elems),
            ExprKind::TupleIndex { object, index } => {
                self.compile_tuple_index(object, *index as usize)
            }
            ExprKind::Cast { expr: inner, ty } => {
                let val = self.compile_expr(inner)?;
                let target_ty = self.llvm_type_for_type_expr(ty);
                self.compile_cast(val, target_ty)
            }
            ExprKind::Match { scrutinee, arms } => self.compile_match(scrutinee, arms),
            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => self.compile_for(pattern, iterable, body),
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => self.compile_if_let(pattern, value, then_block, else_branch.as_deref()),
            // Unsafe blocks: safety checks live in earlier phases; codegen just
            // compiles the inner block normally.
            ExprKind::Unsafe(block) => match self.compile_block(block)? {
                Some(v) => Ok(v),
                None => Ok(self.context.i64_type().const_int(0, false).into()),
            },
            ExprKind::Par(block) => self.compile_par_block(block),
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => self.compile_method_call(object, method, args, &expr.span),
            ExprKind::Index { object, index } => self.compile_index(object, index),
            ExprKind::Question(inner) => self.compile_question(inner, &expr.span),
            ExprKind::Path { segments, .. } => self.compile_path_expr(segments),
            _ => Ok(self.context.i64_type().const_int(0, false).into()),
        }
    }

    /// Compile a `Type.Variant` path expression. The parser emits `Color.Red`
    /// as `ExprKind::Path(["Color", "Red"])` (any dotted ident sequence whose
    /// segments start with an uppercase letter). The only case currently
    /// reaching this arm is unit-variant construction — payload-bearing
    /// variants go through `ExprKind::Call { callee: Path(...), args }` and
    /// are dispatched by `compile_assoc_call`.
    fn compile_path_expr(&mut self, segments: &[String]) -> Result<BasicValueEnum<'ctx>, String> {
        if segments.len() == 2 {
            let type_name = &segments[0];
            let variant_name = &segments[1];
            if let Some(layout) = self.enum_layouts.get(type_name).cloned() {
                if let Some(&tag) = layout.tags.get(variant_name) {
                    if layout.field_counts.get(variant_name).copied().unwrap_or(0) == 0 {
                        let i64_t = self.context.i64_type();
                        if let Some(info) = self.shared_types.get(type_name).cloned() {
                            let ptr = self.emit_rc_alloc(info.heap_type);
                            let tag_ptr = self
                                .builder
                                .build_struct_gep(info.heap_type, ptr, 1, "sh_tag")
                                .unwrap();
                            self.builder
                                .build_store(tag_ptr, i64_t.const_int(tag, false))
                                .unwrap();
                            return Ok(ptr.into());
                        }
                        let mut agg = layout.llvm_type.get_undef();
                        agg = self
                            .builder
                            .build_insert_value(agg, i64_t.const_int(tag, false), 0, "tag")
                            .unwrap()
                            .into_struct_value();
                        return Ok(agg.into());
                    }
                }
            }
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    /// Compile the `?` early-propagation operator for `Result[T,E]` and `Option[T]`.
    ///
    /// The operand is a `{ i64 tag, i64 w0 }` enum struct. Tag semantics:
    ///   Result: Err=0, Ok=1
    ///   Option: None=0, Some=1
    ///
    /// On failure (tag == 0): early-return `{ 0, w0 }` from the current function,
    /// propagating the error/None payload unchanged.
    /// On success (tag == 1): yield `w0` (the unwrapped value) and continue.
    fn compile_question(
        &mut self,
        inner: &Expr,
        outer_span: &crate::token::Span,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let val = self.compile_expr(inner)?;
        let i64_t = self.context.i64_type();
        let enum_ty = self.context.struct_type(
            &[BasicTypeEnum::IntType(i64_t), BasicTypeEnum::IntType(i64_t)],
            false,
        );

        // Extract tag (field 0) and payload word (field 1)
        let tag = self
            .builder
            .build_extract_value(val.into_struct_value(), 0, "q_tag")
            .unwrap()
            .into_int_value();
        let w0 = self
            .builder
            .build_extract_value(val.into_struct_value(), 1, "q_w0")
            .unwrap();

        // Check tag == 0 (failure path)
        let is_failure = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                tag,
                i64_t.const_int(0, false),
                "q_is_fail",
            )
            .unwrap();

        let cur_fn = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let fail_bb = self.context.append_basic_block(cur_fn, "q_fail");
        let ok_bb = self.context.append_basic_block(cur_fn, "q_ok");

        self.builder
            .build_conditional_branch(is_failure, fail_bb, ok_bb)
            .unwrap();

        // Failure block: push an error-return-trace frame, drain scope
        // cleanup actions, optionally convert the err payload via
        // `Target.from(e)`, build `{ 0, w0' }`, and early-return.
        // The cleanup walks the full `scope_cleanup_actions` stack so any
        // heap-owning bindings live at this `?` site (Vec/String buffers, RC
        // values, Map handles) are released before the function returns.
        // The trace push happens BEFORE cleanup so the runtime sees the
        // failure site in source order even if cleanup itself crashes.
        self.builder.position_at_end(fail_bb);
        self.emit_error_trace_push(outer_span);
        self.emit_scope_cleanup();

        // Cross-error-type conversion: when the typechecker recorded a target
        // type for this `?` site, look up the LLVM function `Target.from` and
        // call it on the inner err payload. The user-impl `T.from` LLVM
        // function is already compiled by the impl-block pass.
        let key = (outer_span.offset, outer_span.length);
        let propagated_payload: BasicValueEnum<'ctx> =
            if let Some(target) = self.question_conversions.get(&key).cloned() {
                let qualified = format!("{}.from", target);
                if let Some(from_fn) = self.module.get_function(&qualified) {
                    // The inner err payload was unpacked into the uniform
                    // i64 word `w0` by the enum-payload codegen, but
                    // `Target.from(e: SourceError)` is declared at the
                    // surface level taking the error type itself — for any
                    // `struct SourceError { ... }` LLVM lowers that to the
                    // struct shape. Reconstitute the struct value from the
                    // i64 word so the call's argument matches the param
                    // type. Single-field structs (the common error-wrapper
                    // shape) take field 0 from `w0`; other shapes pass `w0`
                    // through unchanged (the typechecker rejects these
                    // before reaching codegen, so this is just a safety
                    // fallback).
                    let arg_ty = from_fn.get_nth_param(0).unwrap().get_type();
                    let arg: BasicValueEnum<'ctx> = match arg_ty {
                        BasicTypeEnum::StructType(st) if st.count_fields() == 1 => {
                            let undef = st.get_undef();
                            self.builder
                                .build_insert_value(undef, w0, 0, "q_from_arg")
                                .unwrap()
                                .into_struct_value()
                                .into()
                        }
                        _ => w0,
                    };
                    let call_site = self
                        .builder
                        .build_call(from_fn, &[arg.into()], "q_from")
                        .unwrap();
                    call_site.try_as_basic_value().unwrap_basic()
                } else {
                    // No matching impl emitted — propagate raw payload.
                    // The typechecker should have rejected this case; staying
                    // permissive keeps codegen non-fatal on unexpected inputs.
                    w0
                }
            } else {
                w0
            };

        // The error-payload slot is a uniform i64 word (matches the
        // tag+i64-words enum lowering). User-impl `Target.from(e)` returns
        // the target type's value — a struct for any `struct MyError { ... }`.
        // Coerce so `insertvalue` agrees with the slot's element type;
        // single-field structs (the common error-wrapper shape) extract to
        // their inner field.
        let propagated_word = self.coerce_to_i64(propagated_payload)?;

        let ret_struct = {
            let undef = enum_ty.get_undef();
            let s1 = self
                .builder
                .build_insert_value(undef, i64_t.const_int(0, false), 0, "q_ret_tag")
                .unwrap();
            self.builder
                .build_insert_value(s1, propagated_word, 1, "q_ret_val")
                .unwrap()
        };
        self.builder.build_return(Some(&ret_struct)).unwrap();

        // Ok/Some block: clear any frames a recovered earlier `?` had
        // pushed, then continue with the unwrapped payload word. Mirrors
        // the interpreter's `clear_error_trace` call on the success path
        // (src/interpreter.rs:1501).
        self.builder.position_at_end(ok_bb);
        self.builder
            .build_call(self.karac_error_trace_clear_fn, &[], "q_trace_clear")
            .unwrap();
        Ok(w0)
    }

    /// Emit a call to `karac_error_trace_push(file, file_len, line, col)`
    /// at the current insertion point. When `source_filename` is set, a
    /// deduped global string is materialized on first call and reused for
    /// every subsequent `?` site in the module — runtime-side, the printer
    /// formats `<file>:<line>:<col>` rows. When unset, file=null/len=0 and
    /// the runtime prints `<line>:<col>` only (one .kara file at a time).
    fn emit_error_trace_push(&mut self, outer_span: &crate::token::Span) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_ty = self.context.i64_type();
        let i32_ty = self.context.i32_type();
        let (file_ptr, file_len_val) = match self.ensure_source_filename_global() {
            Some((p, len)) => (p, i64_ty.const_int(len, false)),
            None => (ptr_ty.const_null(), i64_ty.const_int(0, false)),
        };
        let line = i32_ty.const_int(outer_span.line as u64, false);
        let col = i32_ty.const_int(outer_span.column as u64, false);
        self.builder
            .build_call(
                self.karac_error_trace_push_fn,
                &[
                    file_ptr.into(),
                    file_len_val.into(),
                    line.into(),
                    col.into(),
                ],
                "q_trace_push",
            )
            .unwrap();
    }

    /// Lazily materialize the LLVM global string for `source_filename` and
    /// return its `(ptr, byte_len)`. Returns `None` when no filename was
    /// threaded in. The byte length is the source filename's byte length —
    /// the runtime's printer writes that many bytes verbatim, so the
    /// trailing NUL added by `build_global_string_ptr` is intentionally
    /// excluded.
    fn ensure_source_filename_global(&mut self) -> Option<(PointerValue<'ctx>, u64)> {
        if let Some(cached) = self.source_filename_global {
            return Some(cached);
        }
        let name = self.source_filename.as_ref()?.clone();
        let len = name.len() as u64;
        let global = self
            .builder
            .build_global_string_ptr(&name, "karac.source_filename")
            .unwrap();
        let ptr = global.as_pointer_value();
        self.source_filename_global = Some((ptr, len));
        Some((ptr, len))
    }

    // ── Struct/tuple expressions ──────────────────────────────────

    fn compile_struct_init(
        &mut self,
        name: &str,
        fields: &[FieldInit],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Shared struct: heap-allocate with refcount header.
        if let Some(info) = self.shared_types.get(name).cloned() {
            if !info.is_enum {
                let ptr = self.emit_rc_alloc(info.heap_type);
                for (idx, field_init) in fields.iter().enumerate() {
                    let val = self.compile_expr(&field_init.value)?;
                    // Fields start at index 1 (index 0 is the refcount).
                    let field_ptr = self
                        .builder
                        .build_struct_gep(
                            info.heap_type,
                            ptr,
                            (idx + 1) as u32,
                            &format!("field_{}", field_init.name),
                        )
                        .unwrap();
                    self.builder.build_store(field_ptr, val).unwrap();
                }
                return Ok(ptr.into());
            }
        }
        // Non-shared struct: stack-allocated aggregate.
        if let Some(&st) = self.struct_types.get(name) {
            let mut agg = st.get_undef();
            for (idx, field_init) in fields.iter().enumerate() {
                let val = self.compile_expr(&field_init.value)?;
                agg = self
                    .builder
                    .build_insert_value(agg, val, idx as u32, "field")
                    .unwrap()
                    .into_struct_value();
            }
            Ok(agg.into())
        } else {
            Ok(self.context.i64_type().const_int(0, false).into())
        }
    }

    /// Compile `let <name>: Vec[T] = Vec::new()` for a SoA-laid-out collection.
    /// Produces `{ null, ..., [null_cold,] 0, 0 }` (one null ptr per group plus optional cold, len=0, cap=0).
    fn compile_soa_new(&mut self, var_name: &str, soa: &SoaLayout) -> Result<(), String> {
        let fn_val = self.current_fn.unwrap();
        let has_cold = soa.cold_group.is_some();
        let soa_ty = self.soa_vec_type(soa.num_groups, has_cold);
        let null_ptr = self.context.ptr_type(AddressSpace::default()).const_null();
        let zero = self.context.i64_type().const_int(0, false);
        let len_idx = Self::soa_len_index(soa.num_groups, has_cold);
        let cap_idx = Self::soa_cap_index(soa.num_groups, has_cold);

        let mut agg = soa_ty.get_undef();
        // Hot group pointers.
        for i in 0..soa.num_groups {
            agg = self
                .builder
                .build_insert_value(agg, null_ptr, i as u32, &format!("soa.g{}", i))
                .unwrap()
                .into_struct_value();
        }
        // Cold pointer (if present).
        if has_cold {
            let cold_idx = Self::soa_cold_ptr_index(soa.num_groups);
            agg = self
                .builder
                .build_insert_value(agg, null_ptr, cold_idx, "soa.cold")
                .unwrap()
                .into_struct_value();
        }
        // len
        agg = self
            .builder
            .build_insert_value(agg, zero, len_idx, "soa.len")
            .unwrap()
            .into_struct_value();
        // cap
        agg = self
            .builder
            .build_insert_value(agg, zero, cap_idx, "soa.cap")
            .unwrap()
            .into_struct_value();

        let alloca = self.create_entry_alloca(fn_val, var_name, soa_ty.into());
        self.builder.build_store(alloca, agg).unwrap();
        self.variables.insert(
            var_name.to_string(),
            VarSlot {
                ptr: alloca,
                ty: soa_ty.into(),
            },
        );
        // Track for scope cleanup (need to free each group buffer).
        self.track_vec_var(alloca);
        Ok(())
    }

    fn compile_soa_method(
        &mut self,
        _var_name: &str,
        soa: &SoaLayout,
        slot: VarSlot<'ctx>,
        method: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let has_cold = soa.cold_group.is_some();
        let soa_ty = self.soa_vec_type(soa.num_groups, has_cold);
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let len_idx = Self::soa_len_index(soa.num_groups, has_cold);
        let cap_idx = Self::soa_cap_index(soa.num_groups, has_cold);

        match method {
            "len" => {
                let len_ptr = self
                    .builder
                    .build_struct_gep(soa_ty, slot.ptr, len_idx, "soa.len.ptr")
                    .unwrap();
                let len = self.builder.build_load(i64_t, len_ptr, "soa.len").unwrap();
                Ok(len)
            }
            "push" => {
                if args.is_empty() {
                    return Err("push requires an argument".to_string());
                }
                let elem_val = self.compile_expr(&args[0].value)?;
                let elem_sv = elem_val.into_struct_value();

                // Load len, cap.
                let len_ptr = self
                    .builder
                    .build_struct_gep(soa_ty, slot.ptr, len_idx, "soa.len.ptr")
                    .unwrap();
                let cap_ptr = self
                    .builder
                    .build_struct_gep(soa_ty, slot.ptr, cap_idx, "soa.cap.ptr")
                    .unwrap();
                let len = self
                    .builder
                    .build_load(i64_t, len_ptr, "soa.len")
                    .unwrap()
                    .into_int_value();
                let cap = self
                    .builder
                    .build_load(i64_t, cap_ptr, "soa.cap")
                    .unwrap()
                    .into_int_value();

                // Growth check.
                let fn_val = self.current_fn.unwrap();
                let grow_bb = self.context.append_basic_block(fn_val, "soa.grow");
                let store_bb = self.context.append_basic_block(fn_val, "soa.store");
                let needs_grow = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::EQ, len, cap, "soa.needs_grow")
                    .unwrap();
                self.builder
                    .build_conditional_branch(needs_grow, grow_bb, store_bb)
                    .unwrap();

                // Grow each group buffer.
                self.builder.position_at_end(grow_bb);
                let two = i64_t.const_int(2, false);
                let four = i64_t.const_int(4, false);
                let doubled = self.builder.build_int_mul(cap, two, "doubled").unwrap();
                let cmp = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGT, doubled, four, "cmp")
                    .unwrap();
                let new_cap = self
                    .builder
                    .build_select(cmp, doubled, four, "new_cap")
                    .unwrap()
                    .into_int_value();

                // Collect all groups to grow: hot groups first, then cold (if present).
                let cold_group_vec: Vec<(usize, &SoaGroup)> = if let Some(ref cg) = soa.cold_group {
                    let cold_idx = soa.num_groups; // struct field index for cold ptr
                    vec![(cold_idx, cg)]
                } else {
                    Vec::new()
                };
                let all_groups: Vec<(usize, &SoaGroup)> = soa
                    .groups
                    .iter()
                    .enumerate()
                    .chain(cold_group_vec.iter().copied())
                    .collect();

                for (struct_field_idx, group) in &all_groups {
                    let group_elem_ty = self.soa_group_elem_type(&soa.struct_name, group);
                    let elem_size = group_elem_ty.size_of().unwrap();
                    let alloc_bytes = self
                        .builder
                        .build_int_mul(new_cap, elem_size, "g.alloc")
                        .unwrap();
                    // Use aligned malloc for groups with align(N).
                    let new_buf = if let Some(align_n) = group.align {
                        let align_val = i64_t.const_int(align_n as u64, false);
                        self.builder
                            .build_call(
                                self.aligned_alloc_fn(),
                                &[align_val.into(), alloc_bytes.into()],
                                "g.new_aligned",
                            )
                            .unwrap()
                            .try_as_basic_value()
                            .unwrap_basic()
                            .into_pointer_value()
                    } else {
                        self.builder
                            .build_call(self.malloc_fn, &[alloc_bytes.into()], "g.new")
                            .unwrap()
                            .try_as_basic_value()
                            .unwrap_basic()
                            .into_pointer_value()
                    };
                    // Copy old data.
                    let old_ptr_ptr = self
                        .builder
                        .build_struct_gep(
                            soa_ty,
                            slot.ptr,
                            *struct_field_idx as u32,
                            &format!("g{}.ptr", struct_field_idx),
                        )
                        .unwrap();
                    let old_buf = self
                        .builder
                        .build_load(ptr_ty, old_ptr_ptr, "g.old")
                        .unwrap()
                        .into_pointer_value();
                    let old_bytes = self
                        .builder
                        .build_int_mul(len, elem_size, "g.old_bytes")
                        .unwrap();
                    self.builder
                        .build_memcpy(new_buf, 8, old_buf, 8, old_bytes)
                        .unwrap();
                    self.builder
                        .build_call(self.free_fn, &[old_buf.into()], "")
                        .unwrap();
                    self.builder.build_store(old_ptr_ptr, new_buf).unwrap();
                }
                self.builder.build_store(cap_ptr, new_cap).unwrap();
                self.builder.build_unconditional_branch(store_bb).unwrap();

                // Store: decompose the struct into group fields.
                self.builder.position_at_end(store_bb);
                let cur_len = self
                    .builder
                    .build_load(i64_t, len_ptr, "cur_len")
                    .unwrap()
                    .into_int_value();

                // Store hot groups.
                for (gi, group) in soa.groups.iter().enumerate() {
                    let group_elem_ty = self.soa_group_elem_type(&soa.struct_name, group);
                    let grp_ptr_ptr = self
                        .builder
                        .build_struct_gep(soa_ty, slot.ptr, gi as u32, &format!("g{}.ptr", gi))
                        .unwrap();
                    let grp_buf = self
                        .builder
                        .build_load(ptr_ty, grp_ptr_ptr, &format!("g{}.buf", gi))
                        .unwrap()
                        .into_pointer_value();
                    let dest = unsafe {
                        self.builder
                            .build_gep(group_elem_ty, grp_buf, &[cur_len], &format!("g{}.dest", gi))
                            .unwrap()
                    };
                    let mut grp_val = group_elem_ty.get_undef();
                    for (fi, &src_idx) in group.field_indices.iter().enumerate() {
                        let field_val = self
                            .builder
                            .build_extract_value(elem_sv, src_idx as u32, "f")
                            .unwrap();
                        grp_val = self
                            .builder
                            .build_insert_value(grp_val, field_val, fi as u32, "gf")
                            .unwrap()
                            .into_struct_value();
                    }
                    self.builder.build_store(dest, grp_val).unwrap();
                }
                // Store cold group (separate allocation).
                if let Some(ref cold) = soa.cold_group.clone() {
                    let cold_idx = Self::soa_cold_ptr_index(soa.num_groups);
                    let cold_elem_ty = self.soa_group_elem_type(&soa.struct_name, cold);
                    let cold_ptr_ptr = self
                        .builder
                        .build_struct_gep(soa_ty, slot.ptr, cold_idx, "cold.ptr")
                        .unwrap();
                    let cold_buf = self
                        .builder
                        .build_load(ptr_ty, cold_ptr_ptr, "cold.buf")
                        .unwrap()
                        .into_pointer_value();
                    let dest = unsafe {
                        self.builder
                            .build_gep(cold_elem_ty, cold_buf, &[cur_len], "cold.dest")
                            .unwrap()
                    };
                    let mut cold_val = cold_elem_ty.get_undef();
                    for (fi, &src_idx) in cold.field_indices.iter().enumerate() {
                        let field_val = self
                            .builder
                            .build_extract_value(elem_sv, src_idx as u32, "f")
                            .unwrap();
                        cold_val = self
                            .builder
                            .build_insert_value(cold_val, field_val, fi as u32, "cf")
                            .unwrap()
                            .into_struct_value();
                    }
                    self.builder.build_store(dest, cold_val).unwrap();
                }

                // Increment len.
                let one = i64_t.const_int(1, false);
                let new_len = self.builder.build_int_add(cur_len, one, "new_len").unwrap();
                self.builder.build_store(len_ptr, new_len).unwrap();

                Ok(self.context.i64_type().const_int(0, false).into())
            }
            _ => Ok(self.context.i64_type().const_int(0, false).into()),
        }
    }

    fn compile_assoc_call(
        &mut self,
        type_name: &str,
        method: &str,
        _args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Numeric primitive From: `T.from(x)` for integer/float widening.
        // Codegen currently represents all ints as LLVM i64 and floats as
        // f64, so widening is a passthrough at this layer. When narrower
        // int types gain LLVM representation, this branch needs sext/zext.
        if method == "from"
            && matches!(
                type_name,
                "i8" | "i16"
                    | "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "usize"
                    | "f32"
                    | "f64"
            )
        {
            if let Some(arg) = _args.first() {
                return self.compile_expr(&arg.value);
            }
        }
        // Lowered operator dispatch: `<Primitive>.<op>(args)` — synthesized
        // by the lowering pass. Reroute to the existing BinOp/UnaryOp
        // intrinsic compilation so we don't have to duplicate codegen logic.
        let is_primitive = matches!(
            type_name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "usize"
                | "f32"
                | "f64"
                | "bool"
                | "char"
                | "String"
        );
        if is_primitive {
            let bin_op = match method {
                "add" => Some(BinOp::Add),
                "sub" => Some(BinOp::Sub),
                "mul" => Some(BinOp::Mul),
                "div" => Some(BinOp::Div),
                "rem" => Some(BinOp::Mod),
                "eq" => Some(BinOp::Eq),
                "ne" => Some(BinOp::NotEq),
                "lt" => Some(BinOp::Lt),
                "le" => Some(BinOp::LtEq),
                "gt" => Some(BinOp::Gt),
                "ge" => Some(BinOp::GtEq),
                "bitand" => Some(BinOp::BitAnd),
                "bitor" => Some(BinOp::BitOr),
                "bitxor" => Some(BinOp::BitXor),
                "shl" => Some(BinOp::Shl),
                "shr" => Some(BinOp::Shr),
                _ => None,
            };
            if let Some(op) = bin_op {
                if _args.len() == 2 {
                    let synth = Expr {
                        span: _args[0].value.span.clone(),
                        kind: ExprKind::Binary {
                            op,
                            left: Box::new(_args[0].value.clone()),
                            right: Box::new(_args[1].value.clone()),
                        },
                    };
                    return self.compile_expr(&synth);
                }
            }
            if method == "neg" && _args.len() == 1 {
                let synth = Expr {
                    span: _args[0].value.span.clone(),
                    kind: ExprKind::Unary {
                        op: UnaryOp::Neg,
                        operand: Box::new(_args[0].value.clone()),
                    },
                };
                return self.compile_expr(&synth);
            }
            if method == "not" && _args.len() == 1 {
                // `not` covers `!bool` and `~int` — target type disambiguates.
                let un_op = if type_name == "bool" {
                    UnaryOp::Not
                } else {
                    UnaryOp::BitNot
                };
                let synth = Expr {
                    span: _args[0].value.span.clone(),
                    kind: ExprKind::Unary {
                        op: un_op,
                        operand: Box::new(_args[0].value.clone()),
                    },
                };
                return self.compile_expr(&synth);
            }
        }
        // Debugger Contract slice 5 — `std.runtime` introspection APIs
        // declared in `runtime/stdlib/runtime.kara`. Three Kāra-callable
        // methods on the empty-marker `Runtime` struct that materialize the
        // slice-3 `KARAC_SPAWN_SITES` metadata + slice-4 `ACTIVE_FRAMES`
        // registry. Routes here because baked-stdlib impl methods are
        // typechecked but not emitted as LLVM functions (see compile_program
        // line 2720+ — only `program.items` impls compile), so the
        // `module.get_function("Runtime.has_debug_metadata")` lookup below
        // would miss and fall through to the i64-zero default. Explicit
        // dispatch keeps the contract surface stable regardless of how
        // baked stdlib codegen evolves.
        if type_name == "Runtime" {
            match method {
                "has_debug_metadata" => {
                    // Single call to `karac_runtime_has_debug_metadata` —
                    // returns the `i1` value directly. The runtime fn reads
                    // `KARAC_SPAWN_SITES_ENABLED`.
                    let f = self
                        .module
                        .get_function("karac_runtime_has_debug_metadata")
                        .expect("karac_runtime_has_debug_metadata declared in Codegen::new");
                    let call = self
                        .builder
                        .build_call(f, &[], "runtime.has_debug_metadata")
                        .unwrap();
                    return Ok(call.try_as_basic_value().unwrap_basic());
                }
                "list_par_blocks" => {
                    // Runtime-side Vec materialization (hard-stop trigger 3
                    // fallback per slice 5 plan). Alloca a `{ptr, i64, i64}`
                    // slot in the entry block, pass its address to the
                    // runtime fn, and load the resulting Vec value.
                    //
                    // The Vec's heap buffer is owned by the caller — the
                    // runtime allocates via `std::alloc::alloc`, the
                    // codegen scope-cleanup machinery treats the returned
                    // Vec like any other Kāra Vec for free-on-exit. Per
                    // `runtime/stdlib/runtime.kara`'s comment on the
                    // method, an empty result is the `{null, 0, 0}` form
                    // (no heap allocation), matching `Vec.new()` so cleanup
                    // is a no-op.
                    let vec_ty = self.vec_struct_type();
                    let fn_val = self
                        .current_fn
                        .ok_or_else(|| "list_par_blocks called outside fn".to_string())?;
                    let slot = self.create_entry_alloca(
                        fn_val,
                        "runtime.list_par_blocks.slot",
                        vec_ty.into(),
                    );
                    let f = self
                        .module
                        .get_function("karac_runtime_list_par_blocks_into")
                        .expect("karac_runtime_list_par_blocks_into declared in Codegen::new");
                    self.builder
                        .build_call(f, &[slot.into()], "runtime.list_par_blocks.fill")
                        .unwrap();
                    let value = self
                        .builder
                        .build_load(vec_ty, slot, "runtime.list_par_blocks.val")
                        .unwrap();
                    return Ok(value);
                }
                "list_tasks" => {
                    // v1 always returns the empty Vec — no real task
                    // suspension exists yet. Identical to the `Vec.new()`
                    // arm below: synthesize `{null, 0, 0}` directly.
                    // Phase 6.3's network event loop replaces this with a
                    // runtime-side materialization mirroring
                    // `list_par_blocks`; the v1 contract pin lives in the
                    // tests under `tests::test_list_tasks_returns_empty_in_v1`.
                    let vec_ty = self.vec_struct_type();
                    let null_ptr = self.context.ptr_type(AddressSpace::default()).const_null();
                    let zero = self.context.i64_type().const_int(0, false);
                    let mut agg = vec_ty.get_undef();
                    agg = self
                        .builder
                        .build_insert_value(agg, null_ptr, 0, "tasks.data")
                        .unwrap()
                        .into_struct_value();
                    agg = self
                        .builder
                        .build_insert_value(agg, zero, 1, "tasks.len")
                        .unwrap()
                        .into_struct_value();
                    agg = self
                        .builder
                        .build_insert_value(agg, zero, 2, "tasks.cap")
                        .unwrap()
                        .into_struct_value();
                    return Ok(agg.into());
                }
                _ => {}
            }
        }

        if type_name == "String" && method == "new" {
            let str_ty = self.vec_struct_type();
            let null_ptr = self.context.ptr_type(AddressSpace::default()).const_null();
            let zero = self.context.i64_type().const_int(0, false);
            let mut agg = str_ty.get_undef();
            agg = self
                .builder
                .build_insert_value(agg, null_ptr, 0, "str.data")
                .unwrap()
                .into_struct_value();
            agg = self
                .builder
                .build_insert_value(agg, zero, 1, "str.len")
                .unwrap()
                .into_struct_value();
            agg = self
                .builder
                .build_insert_value(agg, zero, 2, "str.cap")
                .unwrap()
                .into_struct_value();
            return Ok(agg.into());
        }
        // User impl-block method: if a function named `Type.method` exists
        // in the module (declared by the impl-block pass in `compile`),
        // route the call there. Covers both source-form `Type.method(args)`
        // and the operator-lowered `Call(Path([Type, method]))` form.
        let qualified = format!("{}.{}", type_name, method);
        if let Some(fn_val) = self.module.get_function(&qualified) {
            let ref_flags = self
                .fn_param_ref
                .get(&qualified)
                .cloned()
                .unwrap_or_default();
            let mut compiled_args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::new();
            for (i, a) in _args.iter().enumerate() {
                let is_ref = ref_flags.get(i).copied().unwrap_or(false);
                if is_ref {
                    if let ExprKind::Identifier(var_name) = &a.value.kind {
                        if let Some(ptr) = self.get_data_ptr(var_name) {
                            compiled_args.push(ptr.into());
                            continue;
                        }
                    }
                }
                compiled_args.push(self.compile_expr(&a.value)?.into());
            }
            let call_site = self
                .builder
                .build_call(fn_val, &compiled_args, "usercall")
                .unwrap();
            let basic_val = call_site.try_as_basic_value();
            return if basic_val.is_instruction() {
                Ok(self.context.i64_type().const_int(0, false).into())
            } else {
                Ok(basic_val.unwrap_basic())
            };
        }

        if type_name == "Vec" && method == "new" {
            let vec_ty = self.vec_struct_type();
            let null_ptr = self.context.ptr_type(AddressSpace::default()).const_null();
            let zero = self.context.i64_type().const_int(0, false);
            let mut agg = vec_ty.get_undef();
            agg = self
                .builder
                .build_insert_value(agg, null_ptr, 0, "vec.data")
                .unwrap()
                .into_struct_value();
            agg = self
                .builder
                .build_insert_value(agg, zero, 1, "vec.len")
                .unwrap()
                .into_struct_value();
            agg = self
                .builder
                .build_insert_value(agg, zero, 2, "vec.cap")
                .unwrap()
                .into_struct_value();
            return Ok(agg.into());
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_method_call(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
        call_span: &crate::token::Span,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Cooperative cancel check before each call inside a par-branch.
        // The receiver's `Type.method` key is precomputed by lowering and
        // stored in `method_callee_types`; consult it so a provably pure
        // method elides the check, mirroring the narrowing applied to
        // free-function calls in `compile_call`.
        let callee_key = self
            .method_callee_types
            .get(&(call_span.offset, call_span.length))
            .cloned();
        self.emit_branch_cancel_check("mcall", callee_key.as_deref());

        // Map.entry(k) chain dispatch — `m.entry(k){.and_modify(f)}*.{or_insert(d)|
        // or_insert_with(f)|and_modify(f)}` is lowered as a single sequence
        // around one `karac_map_entry` call so the slot pointer stays valid
        // and there's exactly one hash. Returns Some(_) only when the receiver
        // chain is recognised; otherwise the regular dispatch below runs.
        if let Some(value) = self.try_compile_entry_chain(object, method, args)? {
            return Ok(value);
        }

        // `clone()` dispatch on collection variables — Vec[T], String,
        // Map[K, V], Set[T]. Routes through the per-type clone-fn machinery
        // (`emit_clone_fn_for_type_expr`); see the `Clone trait surface for
        // collections` bullet in `phase-8-stdlib-floor.md`. Returns Some(_)
        // when the receiver is an identifier-bound collection variable;
        // otherwise the regular dispatch below runs (so user `impl X { fn
        // clone(...) }` continues to resolve through the impl-block path).
        if method == "clone" && args.is_empty() {
            if let Some(value) = self.try_compile_clone(object)? {
                return Ok(value);
            }
        }

        // Type-receiver associated calls: `T.method(...)` where `T` is a
        // primitive type name. Receiver `T` is an identifier naming a type,
        // not a variable, so the normal receiver pipeline would fail. Handle
        // `.from` (numeric widening = passthrough) and the operator methods
        // (add/sub/eq/lt/bitand/not/…) by delegating to `compile_assoc_call`,
        // which already knows the primitive fast-path.
        if let ExprKind::Identifier(type_name) = &object.kind {
            let is_primitive = matches!(
                type_name.as_str(),
                "i8" | "i16"
                    | "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "usize"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "char"
                    | "String"
            );
            if is_primitive {
                const OP_METHODS: &[&str] = &[
                    "from", "add", "sub", "mul", "div", "rem", "neg", "eq", "ne", "lt", "le", "gt",
                    "ge", "bitand", "bitor", "bitxor", "shl", "shr", "not",
                ];
                if OP_METHODS.contains(&method) {
                    return self.compile_assoc_call(type_name.as_str(), method, args);
                }
            }
        }

        // `.as_slice()` / `.as_slice_mut()` on Array, Vec, or Slice —
        // synthesize a `{ptr, i64}` slice header. The element type for the
        // resulting slice is inferred from the source variable, not from a
        // user-supplied argument. See design.md § Slices.
        if (method == "as_slice" || method == "as_slice_mut") && args.is_empty() {
            if let ExprKind::Identifier(name) = &object.kind {
                if let Some(slot) = self.variables.get(name.as_str()).copied() {
                    let i64_t = self.context.i64_type();
                    let slice_ty = self.slice_struct_type();
                    if let BasicTypeEnum::ArrayType(at) = slot.ty {
                        let len = i64_t.const_int(at.len() as u64, false);
                        return Ok(self.build_slice_header(slice_ty, slot.ptr, len));
                    }
                    if self.slice_elem_types.contains_key(name.as_str()) {
                        return Ok(self
                            .builder
                            .build_load(slice_ty, slot.ptr, "as_slice.passthrough")
                            .unwrap());
                    }
                    if self.vec_elem_types.contains_key(name.as_str()) {
                        let ptr_ty = self.context.ptr_type(AddressSpace::default());
                        let vec_ty = self.vec_struct_type();
                        let data_pp = self
                            .builder
                            .build_struct_gep(vec_ty, slot.ptr, 0, "as_slice.v.data.pp")
                            .unwrap();
                        let data = self
                            .builder
                            .build_load(ptr_ty, data_pp, "as_slice.v.data")
                            .unwrap()
                            .into_pointer_value();
                        let len_p = self
                            .builder
                            .build_struct_gep(vec_ty, slot.ptr, 1, "as_slice.v.len.p")
                            .unwrap();
                        let len = self
                            .builder
                            .build_load(i64_t, len_p, "as_slice.v.len")
                            .unwrap()
                            .into_int_value();
                        return Ok(self.build_slice_header(slice_ty, data, len));
                    }
                }
            }
        }

        if let ExprKind::Identifier(name) = &object.kind {
            if let Some(slot) = self.variables.get(name.as_str()).copied() {
                // Array methods (owned — slot.ty is ArrayType)
                if let BasicTypeEnum::ArrayType(at) = slot.ty {
                    if method == "len" {
                        return Ok(self
                            .context
                            .i64_type()
                            .const_int(at.len() as u64, false)
                            .into());
                    }
                }
                // Ref Array methods — ref_params has the inner type
                if let Some(&BasicTypeEnum::ArrayType(at)) = self.ref_params.get(name.as_str()) {
                    if method == "len" {
                        return Ok(self
                            .context
                            .i64_type()
                            .const_int(at.len() as u64, false)
                            .into());
                    }
                }
                // SoA layout methods
                if let Some(soa) = self.soa_layouts.get(name.as_str()).cloned() {
                    return self.compile_soa_method(name, &soa, slot, method, args);
                }
                // Vec/String methods (owned or ref)
                if self.vec_elem_types.contains_key(name.as_str()) {
                    let data_ptr = self.get_data_ptr(name).unwrap();
                    return self.compile_vec_method(name, data_ptr, method, args);
                }
                // Slice[T] / mut Slice[T] read-only methods. The slice's
                // stack alloca holds the 2-field `{ptr, i64}` struct (see
                // `slice_struct_type`); GEP field 1 is the length.
                if self.slice_elem_types.contains_key(name.as_str()) {
                    let i64_t = self.context.i64_type();
                    let slice_ty = self.slice_struct_type();
                    match method {
                        "len" => {
                            let len_ptr = self
                                .builder
                                .build_struct_gep(slice_ty, slot.ptr, 1, "slice.len.ptr")
                                .unwrap();
                            let len = self
                                .builder
                                .build_load(i64_t, len_ptr, "slice.len")
                                .unwrap();
                            return Ok(len);
                        }
                        "is_empty" => {
                            let len_ptr = self
                                .builder
                                .build_struct_gep(slice_ty, slot.ptr, 1, "slice.len.ptr")
                                .unwrap();
                            let len = self
                                .builder
                                .build_load(i64_t, len_ptr, "slice.len")
                                .unwrap()
                                .into_int_value();
                            let zero = i64_t.const_zero();
                            let is_empty = self
                                .builder
                                .build_int_compare(IntPredicate::EQ, len, zero, "slice.is_empty")
                                .unwrap();
                            return Ok(is_empty.into());
                        }
                        _ => {
                            return Err(format!(
                                "codegen: no handler for slice method '{}' on '{}'",
                                method, name
                            ));
                        }
                    }
                }
                // Map methods
                if self.map_key_types.contains_key(name.as_str()) {
                    let name = name.clone();
                    return self.compile_map_method(&name, method, args);
                }
                // Set methods
                if self.set_elem_types.contains_key(name.as_str()) {
                    let name = name.clone();
                    return self.compile_set_method(&name, method, args);
                }
            }
        }

        // User impl-block method on a struct receiver: route `obj.method(args)`
        // through the `Type.method` function emitted by the impl-block pass.
        // Requires knowing the object's declared type; the typechecker stashes
        // it via `var_type_names` for struct-kind locals.
        if let Some(receiver_type) = self.inferred_receiver_type(object) {
            let qualified = format!("{}.{}", receiver_type, method);
            if let Some(fn_val) = self.module.get_function(&qualified) {
                // Inspect the resolved fn's first param to decide the receiver
                // calling convention: pointer-typed (ref self / mut ref self)
                // means pass the address of the receiver's storage; struct-
                // typed (owned self) means pass the value. Mismatch silently
                // miscompiles, which is exactly what shipped before this slice.
                let first_param_is_ptr = fn_val
                    .get_type()
                    .get_param_types()
                    .first()
                    .map(|t| matches!(t, BasicMetadataTypeEnum::PointerType(_)))
                    .unwrap_or(false);
                let receiver_arg: BasicMetadataValueEnum<'ctx> = if first_param_is_ptr {
                    if let ExprKind::Identifier(var_name) = &object.kind {
                        if let Some(ptr) = self.get_data_ptr(var_name) {
                            ptr.into()
                        } else {
                            self.compile_expr(object)?.into()
                        }
                    } else {
                        // Non-identifier receiver into a ref-self method:
                        // unsupported in v1 (would require materializing a
                        // temporary alloca). Fall through to compile_expr;
                        // mismatched ABI may surface at link time.
                        self.compile_expr(object)?.into()
                    }
                } else {
                    self.compile_expr(object)?.into()
                };
                let mut compiled_args: Vec<BasicMetadataValueEnum<'ctx>> = vec![receiver_arg];
                for a in args {
                    compiled_args.push(self.compile_expr(&a.value)?.into());
                }
                let call_site = self
                    .builder
                    .build_call(fn_val, &compiled_args, "usermethod")
                    .unwrap();
                let basic_val = call_site.try_as_basic_value();
                return if basic_val.is_instruction() {
                    // Void-return placeholder: callee returns unit, so fill the
                    // expression slot with const-0 i64. NOT a dispatch fall-through.
                    Ok(self.context.i64_type().const_int(0, false).into())
                } else {
                    Ok(basic_val.unwrap_basic())
                };
            }
        }

        let receiver_desc = match &object.kind {
            ExprKind::Identifier(name) => format!("variable '{}'", name),
            _ => "non-identifier receiver".to_string(),
        };
        Err(format!(
            "codegen: no handler for method '{}' on {} (method dispatch fell through; \
             this is a codegen bug — add a dispatcher arm in `compile_method_call` \
             or mark the test `#[ignore]` if the method is genuinely deferred)",
            method, receiver_desc
        ))
    }

    /// Infer the declared struct/enum type name of a method-call receiver,
    /// or `None` if we can't — in which case the caller falls back to its
    /// built-in/primitive handling. Keys off `var_type_names`, which the
    /// existing struct-literal and struct-param paths populate.
    fn inferred_receiver_type(&self, object: &Expr) -> Option<String> {
        if let ExprKind::Identifier(name) = &object.kind {
            return self.var_type_names.get(name.as_str()).cloned();
        }
        None
    }

    fn compile_vec_method(
        &mut self,
        var_name: &str,
        data_ptr: PointerValue<'ctx>,
        method: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let vec_ty = self.vec_struct_type();
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let elem_ty = self.vec_elem_type_for_var(var_name);

        match method {
            "len" => {
                let len_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 1, "vec.len.ptr")
                    .unwrap();
                let len = self.builder.build_load(i64_t, len_ptr, "vec.len").unwrap();
                Ok(len)
            }
            "push" => {
                if args.is_empty() {
                    return Err("Vec.push requires an argument".to_string());
                }
                let elem_val = self.compile_expr(&args[0].value)?;

                // Load current vec fields.
                let data_ptr_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 0, "vec.data.ptr")
                    .unwrap();
                let len_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 1, "vec.len.ptr")
                    .unwrap();
                let cap_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 2, "vec.cap.ptr")
                    .unwrap();
                let data = self
                    .builder
                    .build_load(ptr_ty, data_ptr_ptr, "data")
                    .unwrap()
                    .into_pointer_value();
                let len = self
                    .builder
                    .build_load(i64_t, len_ptr, "len")
                    .unwrap()
                    .into_int_value();
                let cap = self
                    .builder
                    .build_load(i64_t, cap_ptr, "cap")
                    .unwrap()
                    .into_int_value();

                // Growth check: if len == cap, grow.
                let fn_val = self.current_fn.unwrap();
                let grow_bb = self.context.append_basic_block(fn_val, "push.grow");
                let store_bb = self.context.append_basic_block(fn_val, "push.store");
                let needs_grow = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::EQ, len, cap, "needs_grow")
                    .unwrap();
                self.builder
                    .build_conditional_branch(needs_grow, grow_bb, store_bb)
                    .unwrap();

                // Grow path: new_cap = max(4, cap * 2); malloc; memcpy; free old.
                self.builder.position_at_end(grow_bb);
                let two = i64_t.const_int(2, false);
                let four = i64_t.const_int(4, false);
                let doubled = self.builder.build_int_mul(cap, two, "doubled").unwrap();
                let cmp = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGT, doubled, four, "cmp")
                    .unwrap();
                let new_cap = self
                    .builder
                    .build_select(cmp, doubled, four, "new_cap")
                    .unwrap()
                    .into_int_value();

                // Compute byte size: new_cap * sizeof(elem)
                let elem_size = elem_ty.size_of().unwrap();
                let alloc_bytes = self
                    .builder
                    .build_int_mul(new_cap, elem_size, "alloc_bytes")
                    .unwrap();
                let new_data = self
                    .builder
                    .build_call(self.malloc_fn, &[alloc_bytes.into()], "new_data")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_pointer_value();

                // memcpy old data if non-null.
                let old_bytes = self
                    .builder
                    .build_int_mul(len, elem_size, "old_bytes")
                    .unwrap();
                self.builder
                    .build_memcpy(new_data, 8, data, 8, old_bytes)
                    .unwrap();

                // Free old buffer (free(null) is a no-op per C spec).
                self.builder
                    .build_call(self.free_fn, &[data.into()], "")
                    .unwrap();

                // Update vec fields.
                self.builder.build_store(data_ptr_ptr, new_data).unwrap();
                self.builder.build_store(cap_ptr, new_cap).unwrap();
                self.builder.build_unconditional_branch(store_bb).unwrap();

                // Store element at data[len].
                self.builder.position_at_end(store_bb);
                let cur_data = self
                    .builder
                    .build_load(ptr_ty, data_ptr_ptr, "cur_data")
                    .unwrap()
                    .into_pointer_value();
                let elem_ptr = unsafe {
                    self.builder
                        .build_gep(elem_ty, cur_data, &[len], "elem.ptr")
                        .unwrap()
                };
                self.builder.build_store(elem_ptr, elem_val).unwrap();

                // Increment len.
                let one = i64_t.const_int(1, false);
                let new_len = self.builder.build_int_add(len, one, "new_len").unwrap();
                self.builder.build_store(len_ptr, new_len).unwrap();

                Ok(self.context.i64_type().const_int(0, false).into())
            }
            "pop" => {
                let len_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 1, "vec.len.ptr")
                    .unwrap();
                let data_ptr_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 0, "vec.data.ptr")
                    .unwrap();
                let len = self
                    .builder
                    .build_load(i64_t, len_ptr, "len")
                    .unwrap()
                    .into_int_value();
                let data = self
                    .builder
                    .build_load(ptr_ty, data_ptr_ptr, "data")
                    .unwrap()
                    .into_pointer_value();

                let one = i64_t.const_int(1, false);
                let new_len = self.builder.build_int_sub(len, one, "new_len").unwrap();
                self.builder.build_store(len_ptr, new_len).unwrap();

                let elem_ptr = unsafe {
                    self.builder
                        .build_gep(elem_ty, data, &[new_len], "pop.ptr")
                        .unwrap()
                };
                let val = self
                    .builder
                    .build_load(elem_ty, elem_ptr, "pop.val")
                    .unwrap();
                Ok(val)
            }
            "push_str" => {
                if args.is_empty() {
                    return Err("push_str requires an argument".to_string());
                }
                let src_val = self.compile_expr(&args[0].value)?;
                // Extract src string's ptr and len.
                let src_ptr = self
                    .builder
                    .build_extract_value(src_val.into_struct_value(), 0, "src.ptr")
                    .unwrap()
                    .into_pointer_value();
                let src_len = self
                    .builder
                    .build_extract_value(src_val.into_struct_value(), 1, "src.len")
                    .unwrap()
                    .into_int_value();

                // Load target fields.
                let data_ptr_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 0, "t.data.ptr")
                    .unwrap();
                let len_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 1, "t.len.ptr")
                    .unwrap();
                let cap_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 2, "t.cap.ptr")
                    .unwrap();
                let data = self
                    .builder
                    .build_load(ptr_ty, data_ptr_ptr, "t.data")
                    .unwrap()
                    .into_pointer_value();
                let len = self
                    .builder
                    .build_load(i64_t, len_ptr, "t.len")
                    .unwrap()
                    .into_int_value();
                let cap = self
                    .builder
                    .build_load(i64_t, cap_ptr, "t.cap")
                    .unwrap()
                    .into_int_value();

                // Required capacity = len + src_len.
                let new_len = self.builder.build_int_add(len, src_len, "new_len").unwrap();

                // Growth check: if new_len > cap, grow.
                let fn_val = self.current_fn.unwrap();
                let grow_bb = self.context.append_basic_block(fn_val, "pstr.grow");
                let copy_bb = self.context.append_basic_block(fn_val, "pstr.copy");
                let needs_grow = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGT, new_len, cap, "needs_grow")
                    .unwrap();
                self.builder
                    .build_conditional_branch(needs_grow, grow_bb, copy_bb)
                    .unwrap();

                // Grow: new_cap = max(new_len, max(4, cap * 2))
                self.builder.position_at_end(grow_bb);
                let two = i64_t.const_int(2, false);
                let four = i64_t.const_int(4, false);
                let doubled = self.builder.build_int_mul(cap, two, "doubled").unwrap();
                let cmp1 = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGT, doubled, four, "cmp1")
                    .unwrap();
                let growth_min = self
                    .builder
                    .build_select(cmp1, doubled, four, "growth_min")
                    .unwrap()
                    .into_int_value();
                let cmp2 = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGT, new_len, growth_min, "cmp2")
                    .unwrap();
                let new_cap = self
                    .builder
                    .build_select(cmp2, new_len, growth_min, "new_cap")
                    .unwrap()
                    .into_int_value();

                let new_data = self
                    .builder
                    .build_call(self.malloc_fn, &[new_cap.into()], "new_data")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_pointer_value();
                // Copy old data.
                self.builder
                    .build_memcpy(new_data, 1, data, 1, len)
                    .unwrap();
                // Free old if cap > 0 (heap-allocated).
                let zero_val = i64_t.const_int(0, false);
                let was_heap = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGT, cap, zero_val, "was_heap")
                    .unwrap();
                let free_bb = self.context.append_basic_block(fn_val, "pstr.free");
                let after_free_bb = self.context.append_basic_block(fn_val, "pstr.after_free");
                self.builder
                    .build_conditional_branch(was_heap, free_bb, after_free_bb)
                    .unwrap();
                self.builder.position_at_end(free_bb);
                self.builder
                    .build_call(self.free_fn, &[data.into()], "")
                    .unwrap();
                self.builder
                    .build_unconditional_branch(after_free_bb)
                    .unwrap();
                self.builder.position_at_end(after_free_bb);

                self.builder.build_store(data_ptr_ptr, new_data).unwrap();
                self.builder.build_store(cap_ptr, new_cap).unwrap();
                self.builder.build_unconditional_branch(copy_bb).unwrap();

                // Copy src bytes to data + len.
                self.builder.position_at_end(copy_bb);
                let cur_data = self
                    .builder
                    .build_load(ptr_ty, data_ptr_ptr, "cur_data")
                    .unwrap()
                    .into_pointer_value();
                let cur_len = self
                    .builder
                    .build_load(i64_t, len_ptr, "cur_len")
                    .unwrap()
                    .into_int_value();
                let dest = unsafe {
                    self.builder
                        .build_gep(self.context.i8_type(), cur_data, &[cur_len], "dest")
                        .unwrap()
                };
                self.builder
                    .build_memcpy(dest, 1, src_ptr, 1, src_len)
                    .unwrap();
                // Update len.
                let updated_len = self
                    .builder
                    .build_int_add(cur_len, src_len, "updated_len")
                    .unwrap();
                self.builder.build_store(len_ptr, updated_len).unwrap();

                Ok(self.context.i64_type().const_int(0, false).into())
            }
            "is_empty" => {
                let len_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 1, "vec.len.ptr")
                    .unwrap();
                let len = self
                    .builder
                    .build_load(i64_t, len_ptr, "vec.len")
                    .unwrap()
                    .into_int_value();
                let zero = i64_t.const_int(0, false);
                let is_empty = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::EQ, len, zero, "is_empty")
                    .unwrap();
                Ok(is_empty.into())
            }
            "first" | "last" => {
                let option_ty = self.enum_layouts["Option"].llvm_type;
                let len_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 1, "vec.len.ptr")
                    .unwrap();
                let data_ptr_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 0, "vec.data.ptr")
                    .unwrap();
                let len = self
                    .builder
                    .build_load(i64_t, len_ptr, "len")
                    .unwrap()
                    .into_int_value();

                let fn_val = self.current_fn.unwrap();
                let empty_bb = self
                    .context
                    .append_basic_block(fn_val, &format!("{method}.empty"));
                let some_bb = self
                    .context
                    .append_basic_block(fn_val, &format!("{method}.some"));
                let merge_bb = self
                    .context
                    .append_basic_block(fn_val, &format!("{method}.merge"));

                let zero = i64_t.const_int(0, false);
                let is_empty = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::EQ, len, zero, "is_empty")
                    .unwrap();
                self.builder
                    .build_conditional_branch(is_empty, empty_bb, some_bb)
                    .unwrap();

                // Empty branch — return None.
                self.builder.position_at_end(empty_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Some branch — load element at index 0 (first) or len-1 (last).
                self.builder.position_at_end(some_bb);
                let data = self
                    .builder
                    .build_load(ptr_ty, data_ptr_ptr, "data")
                    .unwrap()
                    .into_pointer_value();
                let idx = if method == "first" {
                    zero
                } else {
                    let one = i64_t.const_int(1, false);
                    self.builder.build_int_sub(len, one, "last_idx").unwrap()
                };
                let elem_ptr = unsafe {
                    self.builder
                        .build_gep(elem_ty, data, &[idx], "elem.ptr")
                        .unwrap()
                };
                let elem_val = self.builder.build_load(elem_ty, elem_ptr, "elem").unwrap();
                let elem_word = self.coerce_to_i64(elem_val)?;
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Merge — phi on tag and payload word, then build Option struct.
                self.builder.position_at_end(merge_bb);
                let tag_phi = self.builder.build_phi(i64_t, "opt.tag").unwrap();
                tag_phi.add_incoming(&[
                    (&i64_t.const_int(0, false), empty_bb),
                    (&i64_t.const_int(1, false), some_bb),
                ]);
                let word_phi = self.builder.build_phi(i64_t, "opt.word").unwrap();
                word_phi.add_incoming(&[
                    (&i64_t.const_int(0, false), empty_bb),
                    (&elem_word, some_bb),
                ]);
                let mut agg = option_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, tag_phi.as_basic_value(), 0, "opt.tag.f")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, word_phi.as_basic_value(), 1, "opt.word.f")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            "get" => {
                if args.is_empty() {
                    return Err("Vec.get requires an index argument".to_string());
                }
                let option_ty = self.enum_layouts["Option"].llvm_type;
                let idx_val = self.compile_expr(&args[0].value)?.into_int_value();
                let len_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 1, "vec.len.ptr")
                    .unwrap();
                let data_ptr_ptr = self
                    .builder
                    .build_struct_gep(vec_ty, data_ptr, 0, "vec.data.ptr")
                    .unwrap();
                let len = self
                    .builder
                    .build_load(i64_t, len_ptr, "len")
                    .unwrap()
                    .into_int_value();

                let fn_val = self.current_fn.unwrap();
                let oob_bb = self.context.append_basic_block(fn_val, "get.oob");
                let valid_bb = self.context.append_basic_block(fn_val, "get.valid");
                let merge_bb = self.context.append_basic_block(fn_val, "get.merge");

                let in_bounds = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::ULT, idx_val, len, "in_bounds")
                    .unwrap();
                self.builder
                    .build_conditional_branch(in_bounds, valid_bb, oob_bb)
                    .unwrap();

                // Out-of-bounds branch — return None.
                self.builder.position_at_end(oob_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Valid branch — return Some(data[idx]).
                self.builder.position_at_end(valid_bb);
                let data = self
                    .builder
                    .build_load(ptr_ty, data_ptr_ptr, "data")
                    .unwrap()
                    .into_pointer_value();
                let elem_ptr = unsafe {
                    self.builder
                        .build_gep(elem_ty, data, &[idx_val], "elem.ptr")
                        .unwrap()
                };
                let elem_val = self.builder.build_load(elem_ty, elem_ptr, "elem").unwrap();
                let elem_word = self.coerce_to_i64(elem_val)?;
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Merge — phi, then build Option struct.
                self.builder.position_at_end(merge_bb);
                let tag_phi = self.builder.build_phi(i64_t, "opt.tag").unwrap();
                tag_phi.add_incoming(&[
                    (&i64_t.const_int(0, false), oob_bb),
                    (&i64_t.const_int(1, false), valid_bb),
                ]);
                let word_phi = self.builder.build_phi(i64_t, "opt.word").unwrap();
                word_phi
                    .add_incoming(&[(&i64_t.const_int(0, false), oob_bb), (&elem_word, valid_bb)]);
                let mut agg = option_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, tag_phi.as_basic_value(), 0, "opt.tag.f")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, word_phi.as_basic_value(), 1, "opt.word.f")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            _ => Ok(self.context.i64_type().const_int(0, false).into()),
        }
    }

    // ── Map codegen ───────────────────────────────────────────────

    /// Emit an FNV-1a byte loop over `byte_count` bytes starting at `data_ptr`.
    /// Appends basic blocks to `hash_fn_val`. Builder must be positioned just before
    /// the first block of the loop; on return it is positioned at the exit block.
    /// Returns the accumulated hash `IntValue` (i64).
    fn emit_fnv1a_over_bytes(
        &mut self,
        hash_fn_val: FunctionValue<'ctx>,
        data_ptr: PointerValue<'ctx>,
        byte_count: IntValue<'ctx>,
    ) -> IntValue<'ctx> {
        let i64_t = self.context.i64_type();
        let i8_t = self.context.i8_type();
        let fnv_basis = i64_t.const_int(14695981039346656037_u64, false);
        let fnv_prime = i64_t.const_int(1099511628211_u64, false);

        let pre_bb = self.builder.get_insert_block().unwrap();
        let hdr_bb = self.context.append_basic_block(hash_fn_val, "fnv.hdr");
        let bdy_bb = self.context.append_basic_block(hash_fn_val, "fnv.bdy");
        let exit_bb = self.context.append_basic_block(hash_fn_val, "fnv.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let i_phi = self.builder.build_phi(i64_t, "fnv.i").unwrap();
        let hash_phi = self.builder.build_phi(i64_t, "fnv.hash").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), pre_bb)]);
        hash_phi.add_incoming(&[(&fnv_basis, pre_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let hash_val = hash_phi.as_basic_value().into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, i_val, byte_count, "fnv.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        let byte_ptr = unsafe {
            self.builder
                .build_gep(i8_t, data_ptr, &[i_val], "fnv.bp")
                .unwrap()
        };
        let byte = self
            .builder
            .build_load(i8_t, byte_ptr, "fnv.b")
            .unwrap()
            .into_int_value();
        let byte64 = self
            .builder
            .build_int_z_extend(byte, i64_t, "fnv.b64")
            .unwrap();
        let xored = self.builder.build_xor(hash_val, byte64, "fnv.xor").unwrap();
        let new_hash = self
            .builder
            .build_int_mul(xored, fnv_prime, "fnv.mul")
            .unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "fnv.i1")
            .unwrap();
        i_phi.add_incoming(&[(&i_next, bdy_bb)]);
        hash_phi.add_incoming(&[(&new_hash, bdy_bb)]);
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        hash_val
    }

    /// Emit (or reuse) a module-level `karac_hash_{type_name}(ptr) -> i64` function.
    ///
    /// - Integer/float primitives: FNV-1a over the raw `sizeof(K)` bytes.
    /// - `String`: loads `{ ptr data, i64 len }` from the struct and hashes `data[0..len]`.
    /// - Structs/other: FNV-1a over the raw struct bytes (correct for value-only structs).
    fn emit_hash_fn_for_type(
        &mut self,
        type_name: &str,
        key_ty: BasicTypeEnum<'ctx>,
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_hash_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let saved_bb = self.builder.get_insert_block();

        let hash_fn_ty = i64_t.fn_type(&[ptr_ty.into()], false);
        let hash_fn = self
            .module
            .add_function(&fn_name, hash_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(hash_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let key_ptr = hash_fn.get_nth_param(0).unwrap().into_pointer_value();

        if type_name == "String" || type_name == "str" {
            // String struct: { ptr data, i64 len, i64 cap }
            let str_ty = self.vec_struct_type();
            let data_pp = self
                .builder
                .build_struct_gep(str_ty, key_ptr, 0, "s.data.pp")
                .unwrap();
            let data_ptr = self
                .builder
                .build_load(ptr_ty, data_pp, "s.data")
                .unwrap()
                .into_pointer_value();
            let len_p = self
                .builder
                .build_struct_gep(str_ty, key_ptr, 1, "s.len.p")
                .unwrap();
            let len = self
                .builder
                .build_load(i64_t, len_p, "s.len")
                .unwrap()
                .into_int_value();
            let hash = self.emit_fnv1a_over_bytes(hash_fn, data_ptr, len);
            self.builder.build_return(Some(&hash)).unwrap();
        } else {
            // All other types: FNV-1a over sizeof(K) raw bytes.
            let raw_size = key_ty
                .size_of()
                .unwrap_or_else(|| i64_t.const_int(8, false));
            let size64 = if raw_size.get_type().get_bit_width() == 64 {
                raw_size
            } else {
                self.builder
                    .build_int_z_extend(raw_size, i64_t, "ksz64")
                    .unwrap()
            };
            let hash = self.emit_fnv1a_over_bytes(hash_fn, key_ptr, size64);
            self.builder.build_return(Some(&hash)).unwrap();
        }

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        hash_fn
    }

    /// Emit (or reuse) a module-level `karac_eq_{type_name}(ptr, ptr) -> i1` function.
    ///
    /// - Integer primitives: load both values and `icmp eq`.
    /// - `String`: compare lengths then byte-by-byte.
    /// - Structs/other: byte-by-byte over raw `sizeof(K)` bytes.
    fn emit_eq_fn_for_type(
        &mut self,
        type_name: &str,
        key_ty: BasicTypeEnum<'ctx>,
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_eq_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let i8_t = self.context.i8_type();
        let bool_t = self.context.bool_type();

        let saved_bb = self.builder.get_insert_block();

        let eq_fn_ty = bool_t.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let eq_fn = self
            .module
            .add_function(&fn_name, eq_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(eq_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let a_ptr = eq_fn.get_nth_param(0).unwrap().into_pointer_value();
        let b_ptr = eq_fn.get_nth_param(1).unwrap().into_pointer_value();

        if type_name == "String" || type_name == "str" {
            // String: compare lengths first, then byte-by-byte on content.
            let str_ty = self.vec_struct_type();
            let la_p = self
                .builder
                .build_struct_gep(str_ty, a_ptr, 1, "la.p")
                .unwrap();
            let lb_p = self
                .builder
                .build_struct_gep(str_ty, b_ptr, 1, "lb.p")
                .unwrap();
            let len_a = self
                .builder
                .build_load(i64_t, la_p, "la")
                .unwrap()
                .into_int_value();
            let len_b = self
                .builder
                .build_load(i64_t, lb_p, "lb")
                .unwrap()
                .into_int_value();

            let neq_bb = self.context.append_basic_block(eq_fn, "neq");
            let bytes_bb = self.context.append_basic_block(eq_fn, "bytes");

            let len_eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, len_a, len_b, "len.eq")
                .unwrap();
            self.builder
                .build_conditional_branch(len_eq, bytes_bb, neq_bb)
                .unwrap();

            // neq_bb: return false
            self.builder.position_at_end(neq_bb);
            self.builder
                .build_return(Some(&bool_t.const_int(0, false)))
                .unwrap();

            // bytes_bb: load data ptrs, enter byte loop
            self.builder.position_at_end(bytes_bb);
            let da_p = self
                .builder
                .build_struct_gep(str_ty, a_ptr, 0, "da.p")
                .unwrap();
            let db_p = self
                .builder
                .build_struct_gep(str_ty, b_ptr, 0, "db.p")
                .unwrap();
            let data_a = self
                .builder
                .build_load(ptr_ty, da_p, "da")
                .unwrap()
                .into_pointer_value();
            let data_b = self
                .builder
                .build_load(ptr_ty, db_p, "db")
                .unwrap()
                .into_pointer_value();

            let loop_hdr = self.context.append_basic_block(eq_fn, "eq.hdr");
            let loop_bdy = self.context.append_basic_block(eq_fn, "eq.bdy");
            let loop_exit = self.context.append_basic_block(eq_fn, "eq.exit");

            self.builder.build_unconditional_branch(loop_hdr).unwrap();

            self.builder.position_at_end(loop_hdr);
            let i_phi = self.builder.build_phi(i64_t, "eq.i").unwrap();
            i_phi.add_incoming(&[(&i64_t.const_zero(), bytes_bb)]);
            let i_val = i_phi.as_basic_value().into_int_value();
            let cond = self
                .builder
                .build_int_compare(IntPredicate::ULT, i_val, len_a, "eq.cond")
                .unwrap();
            self.builder
                .build_conditional_branch(cond, loop_bdy, loop_exit)
                .unwrap();

            self.builder.position_at_end(loop_bdy);
            let bpa = unsafe {
                self.builder
                    .build_gep(i8_t, data_a, &[i_val], "bpa")
                    .unwrap()
            };
            let bpb = unsafe {
                self.builder
                    .build_gep(i8_t, data_b, &[i_val], "bpb")
                    .unwrap()
            };
            let ba = self
                .builder
                .build_load(i8_t, bpa, "ba")
                .unwrap()
                .into_int_value();
            let bb_v = self
                .builder
                .build_load(i8_t, bpb, "bb")
                .unwrap()
                .into_int_value();
            let bytes_eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, ba, bb_v, "beq")
                .unwrap();
            let i_next = self
                .builder
                .build_int_add(i_val, i64_t.const_int(1, false), "eq.i1")
                .unwrap();
            i_phi.add_incoming(&[(&i_next, loop_bdy)]);
            self.builder
                .build_conditional_branch(bytes_eq, loop_hdr, neq_bb)
                .unwrap();

            self.builder.position_at_end(loop_exit);
            self.builder
                .build_return(Some(&bool_t.const_int(1, false)))
                .unwrap();
        } else if let BasicTypeEnum::IntType(int_ty) = key_ty {
            // Integer primitives: load and compare directly.
            let va = self
                .builder
                .build_load(int_ty, a_ptr, "va")
                .unwrap()
                .into_int_value();
            let vb = self
                .builder
                .build_load(int_ty, b_ptr, "vb")
                .unwrap()
                .into_int_value();
            let eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, va, vb, "eq")
                .unwrap();
            self.builder.build_return(Some(&eq)).unwrap();
        } else {
            // Structs and other fixed-size types: byte-by-byte comparison.
            let raw_size = key_ty
                .size_of()
                .unwrap_or_else(|| i64_t.const_int(8, false));
            let size64 = if raw_size.get_type().get_bit_width() == 64 {
                raw_size
            } else {
                self.builder
                    .build_int_z_extend(raw_size, i64_t, "ksz64")
                    .unwrap()
            };

            let neq_bb = self.context.append_basic_block(eq_fn, "neq");
            let loop_hdr = self.context.append_basic_block(eq_fn, "eq.hdr");
            let loop_bdy = self.context.append_basic_block(eq_fn, "eq.bdy");
            let loop_exit = self.context.append_basic_block(eq_fn, "eq.exit");

            self.builder.build_unconditional_branch(loop_hdr).unwrap();

            self.builder.position_at_end(neq_bb);
            self.builder
                .build_return(Some(&bool_t.const_int(0, false)))
                .unwrap();

            self.builder.position_at_end(loop_hdr);
            let i_phi = self.builder.build_phi(i64_t, "eq.i").unwrap();
            i_phi.add_incoming(&[(&i64_t.const_zero(), entry_bb)]);
            let i_val = i_phi.as_basic_value().into_int_value();
            let cond = self
                .builder
                .build_int_compare(IntPredicate::ULT, i_val, size64, "eq.cond")
                .unwrap();
            self.builder
                .build_conditional_branch(cond, loop_bdy, loop_exit)
                .unwrap();

            self.builder.position_at_end(loop_bdy);
            let bpa = unsafe {
                self.builder
                    .build_gep(i8_t, a_ptr, &[i_val], "bpa")
                    .unwrap()
            };
            let bpb = unsafe {
                self.builder
                    .build_gep(i8_t, b_ptr, &[i_val], "bpb")
                    .unwrap()
            };
            let ba = self
                .builder
                .build_load(i8_t, bpa, "ba")
                .unwrap()
                .into_int_value();
            let bb_v = self
                .builder
                .build_load(i8_t, bpb, "bb")
                .unwrap()
                .into_int_value();
            let bytes_eq = self
                .builder
                .build_int_compare(IntPredicate::EQ, ba, bb_v, "beq")
                .unwrap();
            let i_next = self
                .builder
                .build_int_add(i_val, i64_t.const_int(1, false), "eq.i1")
                .unwrap();
            i_phi.add_incoming(&[(&i_next, loop_bdy)]);
            self.builder
                .build_conditional_branch(bytes_eq, loop_hdr, neq_bb)
                .unwrap();

            self.builder.position_at_end(loop_exit);
            self.builder
                .build_return(Some(&bool_t.const_int(1, false)))
                .unwrap();
        }

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        eq_fn
    }

    /// TypeExpr-aware hash-fn wrapper. Dispatches tuples to a recursive
    /// composition (per-field hash + FNV tail-mix combine) and falls through
    /// to the primitive `emit_hash_fn_for_type` path for everything else.
    ///
    /// Cache key is the mangled type name (`Self::mangled_type_name`), so a
    /// `(String, i32)` tuple key emits `karac_hash_tuple_String_i32` once per
    /// module and reuses it across all `Map[(String, i32), V]` / nested
    /// occurrences.
    fn emit_hash_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::mangled_type_name(te);
        let fn_name = format!("karac_hash_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => {
                self.emit_hash_fn_for_tuple(&type_name, elems)
            }
            _ => {
                let key_ty = self.llvm_type_for_type_expr(te);
                self.emit_hash_fn_for_type(&type_name, key_ty)
            }
        }
    }

    /// TypeExpr-aware eq-fn wrapper. Mirror of `emit_hash_fn_for_type_expr`.
    fn emit_eq_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::mangled_type_name(te);
        let fn_name = format!("karac_eq_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => {
                self.emit_eq_fn_for_tuple(&type_name, elems)
            }
            _ => {
                let key_ty = self.llvm_type_for_type_expr(te);
                self.emit_eq_fn_for_type(&type_name, key_ty)
            }
        }
    }

    /// Emit a per-field-recursive hash function for an n-tuple. Each field's
    /// hash is computed by recursing into `emit_hash_fn_for_type_expr` (so
    /// `(String, i64)` correctly hashes the String contents, not the struct
    /// bytes), then combined into a running state via the FNV tail-mix
    /// `state = (state * FNV_PRIME) ^ field_hash`.
    fn emit_hash_fn_for_tuple(
        &mut self,
        type_name: &str,
        elems: &[TypeExpr],
    ) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_hash_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        let elems_owned: Vec<TypeExpr> = elems.to_vec();
        let child_fns: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_hash_fn_for_type_expr(e))
            .collect();
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let saved_bb = self.builder.get_insert_block();
        let hash_fn_ty = i64_t.fn_type(&[ptr_ty.into()], false);
        let hash_fn = self
            .module
            .add_function(&fn_name, hash_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(hash_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let key_ptr = hash_fn.get_nth_param(0).unwrap().into_pointer_value();

        let fnv_basis = i64_t.const_int(14695981039346656037_u64, false);
        let fnv_prime = i64_t.const_int(1099511628211_u64, false);
        let mut state: IntValue<'ctx> = fnv_basis;
        for (i, child_fn) in child_fns.iter().enumerate() {
            let field_ptr = self
                .builder
                .build_struct_gep(tuple_ty, key_ptr, i as u32, &format!("t.f{i}.p"))
                .unwrap();
            let elem_hash = self
                .builder
                .build_call(*child_fn, &[field_ptr.into()], &format!("t.f{i}.h"))
                .unwrap()
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value();
            let mul = self
                .builder
                .build_int_mul(state, fnv_prime, &format!("t.f{i}.mul"))
                .unwrap();
            state = self
                .builder
                .build_xor(mul, elem_hash, &format!("t.f{i}.xor"))
                .unwrap();
        }
        self.builder.build_return(Some(&state)).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        hash_fn
    }

    /// Emit a per-field-recursive eq function for an n-tuple. Each field is
    /// compared via the recursively-emitted per-field eq fn; the function
    /// short-circuits to `false` on the first mismatch.
    fn emit_eq_fn_for_tuple(&mut self, type_name: &str, elems: &[TypeExpr]) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_eq_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            return f;
        }
        let elems_owned: Vec<TypeExpr> = elems.to_vec();
        let child_fns: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_eq_fn_for_type_expr(e))
            .collect();
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let bool_t = self.context.bool_type();

        let saved_bb = self.builder.get_insert_block();
        let eq_fn_ty = bool_t.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let eq_fn = self
            .module
            .add_function(&fn_name, eq_fn_ty, Some(Linkage::Internal));

        let entry_bb = self.context.append_basic_block(eq_fn, "entry");
        let neq_bb = self.context.append_basic_block(eq_fn, "neq");
        self.builder.position_at_end(neq_bb);
        self.builder
            .build_return(Some(&bool_t.const_int(0, false)))
            .unwrap();

        self.builder.position_at_end(entry_bb);
        let a_ptr = eq_fn.get_nth_param(0).unwrap().into_pointer_value();
        let b_ptr = eq_fn.get_nth_param(1).unwrap().into_pointer_value();

        for (i, child_fn) in child_fns.iter().enumerate() {
            let fa = self
                .builder
                .build_struct_gep(tuple_ty, a_ptr, i as u32, &format!("t.fa{i}"))
                .unwrap();
            let fb = self
                .builder
                .build_struct_gep(tuple_ty, b_ptr, i as u32, &format!("t.fb{i}"))
                .unwrap();
            let r = self
                .builder
                .build_call(*child_fn, &[fa.into(), fb.into()], &format!("t.eq{i}"))
                .unwrap()
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value();
            let next_bb = self
                .context
                .append_basic_block(eq_fn, &format!("eq.next{i}"));
            self.builder
                .build_conditional_branch(r, next_bb, neq_bb)
                .unwrap();
            self.builder.position_at_end(next_bb);
        }
        self.builder
            .build_return(Some(&bool_t.const_int(1, false)))
            .unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        eq_fn
    }

    /// Emit (or reuse) a module-level Display function for the given type.
    ///
    /// Signature: `void karac_display_<type_name>(*const T)`. The function
    /// reads `*ptr` (or extracts struct fields, depending on the type) and
    /// writes a textual representation to stdout via `printf`. No trailing
    /// newline — callers append `\n` themselves for `println`.
    ///
    /// Subtask 1+2 scope: primitives (`i8`..`i64` / `u8`..`u64` / `f32`/`f64`
    /// / `bool` / `char` / `String`/`str`). Compound types (Vec/Map/Set/Tuple)
    /// land in subtasks 3-6, each as a new arm in this function that recurses
    /// into `emit_display_fn_for_type` for element/field types.
    ///
    /// Cache is keyed by the canonical `type_name` string — same convention
    /// used by `emit_hash_fn_for_type`. Caller is responsible for ensuring
    /// `type_name` uniquely identifies the type (for primitives this is
    /// trivial; for compound types the caller composes a mangled name).
    ///
    /// `dead_code` is allowed because subtasks 1+2 of the Display canonical
    /// bullet ship the machinery + primitive Display fns ahead of subtasks
    /// 3-7 which add the callers. Remove the allow when subtask 7 lands.
    #[allow(dead_code)]
    fn emit_display_fn_for_type(
        &mut self,
        type_name: &str,
        ty: BasicTypeEnum<'ctx>,
    ) -> FunctionValue<'ctx> {
        if let Some(&f) = self.display_fn_cache.get(type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name.to_string(), f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i32_t = self.context.i32_type();
        let i64_t = self.context.i64_type();
        let f64_t = self.context.f64_type();

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache
            .insert(type_name.to_string(), display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        match type_name {
            "i8" | "i16" | "i32" | "i64" | "isize" => {
                // Sign-extend to i64, printf "%lld".
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let v64 = self.builder.build_int_s_extend(v, i64_t, "v64").unwrap();
                let fmt = self.builder.build_global_string_ptr("%lld", "fi").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v64.into()],
                        "p",
                    )
                    .unwrap();
            }
            "u8" | "u16" | "u32" | "u64" | "usize" => {
                // Zero-extend to i64, printf "%llu".
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let v64 = self.builder.build_int_z_extend(v, i64_t, "v64").unwrap();
                let fmt = self.builder.build_global_string_ptr("%llu", "fu").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v64.into()],
                        "p",
                    )
                    .unwrap();
            }
            "f32" => {
                // Widen to f64, printf "%g".
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_float_value();
                let v64 = self.builder.build_float_ext(v, f64_t, "v64").unwrap();
                let fmt = self.builder.build_global_string_ptr("%g", "ff").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v64.into()],
                        "p",
                    )
                    .unwrap();
            }
            "f64" => {
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_float_value();
                let fmt = self.builder.build_global_string_ptr("%g", "ff").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v.into()],
                        "p",
                    )
                    .unwrap();
            }
            "bool" => {
                // Select between "true" / "false" static strings.
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let true_s = self.builder.build_global_string_ptr("true", "ts").unwrap();
                let false_s = self.builder.build_global_string_ptr("false", "fs").unwrap();
                let sel = self
                    .builder
                    .build_select(
                        v,
                        true_s.as_pointer_value(),
                        false_s.as_pointer_value(),
                        "bsel",
                    )
                    .unwrap();
                let fmt = self.builder.build_global_string_ptr("%s", "fs").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), sel.into()],
                        "p",
                    )
                    .unwrap();
            }
            "char" => {
                // Char is a Unicode scalar (i32). For ASCII (the common case)
                // %c prints correctly. Non-ASCII codepoints get truncated to
                // i32 by printf — UTF-8 encoding refinement is a follow-up.
                let v = self
                    .builder
                    .build_load(ty, val_ptr, "v")
                    .unwrap()
                    .into_int_value();
                let fmt = self.builder.build_global_string_ptr("%c", "fc").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), v.into()],
                        "p",
                    )
                    .unwrap();
            }
            "String" | "str" => {
                // 24-byte struct {data, len, cap}. Use %.*s to bound by len —
                // String values are NOT NUL-terminated.
                let str_ty = self.vec_struct_type();
                let data_pp = self
                    .builder
                    .build_struct_gep(str_ty, val_ptr, 0, "s.data.pp")
                    .unwrap();
                let len_p = self
                    .builder
                    .build_struct_gep(str_ty, val_ptr, 1, "s.len.p")
                    .unwrap();
                let data = self
                    .builder
                    .build_load(ptr_ty, data_pp, "s.data")
                    .unwrap()
                    .into_pointer_value();
                let len = self
                    .builder
                    .build_load(i64_t, len_p, "s.len")
                    .unwrap()
                    .into_int_value();
                let len32 = self
                    .builder
                    .build_int_truncate(len, i32_t, "len32")
                    .unwrap();
                let fmt = self.builder.build_global_string_ptr("%.*s", "fs").unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[fmt.as_pointer_value().into(), len32.into(), data.into()],
                        "p",
                    )
                    .unwrap();
            }
            other if other.starts_with("Vec_") => {
                // Vec[T]'s element TypeExpr can't be unambiguously recovered
                // from the mangled cache name once nested compound shapes
                // (e.g. `Vec_tuple_i64_String`) are in play — string-splitting
                // on `_` is brittle. Callers should hold the element
                // `TypeExpr` and dispatch via `emit_display_fn_for_type_expr`,
                // which routes Vec to `emit_vec_display_fn_te(elem_te)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_vec_display_fn_te(elem_te) (or emit_display_fn_for_type_expr)"
                );
            }
            other if other.starts_with("Map_") => {
                // Map types have two type parameters and so cannot recover
                // (key_ty, val_ty) by string-splitting the cache key. Callers
                // that already hold K and V `TypeExpr`s should dispatch via
                // `emit_display_fn_for_type_expr`, which routes Map to
                // `emit_map_display_fn(key_te, val_te)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_map_display_fn(key_te, val_te) (or emit_display_fn_for_type_expr)"
                );
            }
            other if other.starts_with("Set_") => {
                // Set's element TypeExpr can't be unambiguously recovered
                // from a mangled cache name once nested compound shapes are
                // in play. Callers should hold the element `TypeExpr` and
                // dispatch via `emit_display_fn_for_type_expr`, which
                // routes Set to `emit_set_display_fn(elem_te)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_set_display_fn(elem_te) (or emit_display_fn_for_type_expr)"
                );
            }
            other if other.starts_with("tuple_") => {
                // n-tuples cannot recover their per-field TypeExprs from the
                // mangled name alone. Callers that already hold the field
                // `TypeExpr`s should dispatch via
                // `emit_display_fn_for_type_expr`, which routes Tuple to
                // `emit_tuple_display_fn(elems)`.
                panic!(
                    "emit_display_fn_for_type: '{other}' must be emitted via \
                     emit_tuple_display_fn(elems) (or emit_display_fn_for_type_expr)"
                );
            }
            other => {
                // Set_*, user structs not yet supported.
                // Subtask 5 of the Display canonical bullet
                // (phase-7-codegen.md § Phase 7.2) extends this match for Set.
                panic!("emit_display_fn_for_type: type_name '{other}' not yet supported");
            }
        }

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Emit the body of a `Vec[T]` Display function. Reads `data`/`len` from
    /// the 24-byte Vec struct at `val_ptr`, prints `[`, walks elements with
    /// `, ` separators recursing into the element Display fn, prints `]`.
    ///
    /// `elem_te` describes the element type. Recursion into the per-element
    /// Display fn goes through the TypeExpr-aware dispatcher
    /// (`emit_display_fn_for_type_expr`) so compound elements (`Vec[Vec[T]]`,
    /// `Vec[(i64, String)]`, `Vec[Map[K, V]]`) compose correctly without the
    /// by-name path having to recover `TypeExpr`s from a mangled string.
    ///
    /// Caller is expected to have positioned the builder at the entry block
    /// of `display_fn` and to emit the trailing `ret void` after this returns.
    fn emit_vec_display_body(
        &mut self,
        display_fn: FunctionValue<'ctx>,
        val_ptr: PointerValue<'ctx>,
        elem_te: &TypeExpr,
    ) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i8_t = self.context.i8_type();
        let i64_t = self.context.i64_type();
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);

        // Materialize (or fetch) the element Display fn first — the recursive
        // emit may switch the builder's insert block, so do it before the
        // remaining body emission positions us at `display_fn`'s entry. The
        // dispatcher saves/restores so the caller's position is preserved.
        let elem_disp = self.emit_display_fn_for_type_expr(elem_te);

        // Print "[".
        let lb = self.builder.build_global_string_ptr("[", "vd.lb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[lb.as_pointer_value().into()], "p")
            .unwrap();

        // Load data (i8*) and len (i64) from the Vec struct.
        let data_pp = self
            .builder
            .build_struct_gep(vec_ty, val_ptr, 0, "v.data.pp")
            .unwrap();
        let len_p = self
            .builder
            .build_struct_gep(vec_ty, val_ptr, 1, "v.len.p")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "v.data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_p, "v.len")
            .unwrap()
            .into_int_value();

        // Element size in bytes — drives the GEP stride.
        let raw_size = elem_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let elem_size = if raw_size.get_type().get_bit_width() == 64 {
            raw_size
        } else {
            self.builder
                .build_int_z_extend(raw_size, i64_t, "esz64")
                .unwrap()
        };

        // Loop: i in 0..len, with ", " separator before every elem after first.
        let pre_bb = self.builder.get_insert_block().unwrap();
        let hdr_bb = self.context.append_basic_block(display_fn, "vec.hdr");
        let bdy_bb = self.context.append_basic_block(display_fn, "vec.bdy");
        let sep_bb = self.context.append_basic_block(display_fn, "vec.sep");
        let elem_bb = self.context.append_basic_block(display_fn, "vec.elem");
        let exit_bb = self.context.append_basic_block(display_fn, "vec.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let i_phi = self.builder.build_phi(i64_t, "vec.i").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), pre_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, i_val, len, "vec.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, bdy_bb, exit_bb)
            .unwrap();

        // bdy: branch to sep if i > 0, else straight to elem.
        self.builder.position_at_end(bdy_bb);
        let is_first = self
            .builder
            .build_int_compare(IntPredicate::EQ, i_val, i64_t.const_zero(), "is.first")
            .unwrap();
        self.builder
            .build_conditional_branch(is_first, elem_bb, sep_bb)
            .unwrap();

        // sep: print ", ", then fall to elem.
        self.builder.position_at_end(sep_bb);
        let sep = self
            .builder
            .build_global_string_ptr(", ", "vd.sep")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
            .unwrap();
        self.builder.build_unconditional_branch(elem_bb).unwrap();

        // elem: GEP to data + i * elem_size, call element Display fn.
        self.builder.position_at_end(elem_bb);
        let offset = self.builder.build_int_mul(i_val, elem_size, "off").unwrap();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(i8_t, data, &[offset], "elem.p")
                .unwrap()
        };
        self.builder
            .build_call(elem_disp, &[elem_ptr.into()], "ed")
            .unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "vec.i1")
            .unwrap();
        i_phi.add_incoming(&[(&i_next, elem_bb)]);
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        // exit: print "]".
        self.builder.position_at_end(exit_bb);
        let rb = self.builder.build_global_string_ptr("]", "vd.rb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rb.as_pointer_value().into()], "p")
            .unwrap();
    }

    /// Emit (or reuse) a Display function for `Map[K, V]`. Typed entry point —
    /// distinct from `emit_display_fn_for_type` because Map's two type
    /// parameters can't be recovered from a single mangled name string.
    ///
    /// The emitted function is named `karac_display_Map_<key>_<val>` (deeply
    /// mangled via `display_mangle_te`) and is shared with the generic Display
    /// cache under the same key, so a later `emit_display_fn_for_type` cache
    /// hit returns the same function (the catch-all `Map_*` arm panics on
    /// cache miss to steer callers here).
    ///
    /// Calling convention: `void karac_display_Map_K_V(ptr slot)` where `slot`
    /// is the address of a slot holding the opaque map handle (matches the
    /// shape produced by `compile_map_new_stmt`). Body loads the handle,
    /// drives `karac_map_iter_*` (mirroring `compile_for_map_var`),
    /// per-iteration recurses into `emit_display_fn_for_type_expr` for K and
    /// V (so `Map[(i64, String), Vec[bool]]` etc. compose correctly), and
    /// frees the iterator before returning. Iteration order is unspecified
    /// per `design.md` line 1588 — tests must not assert order.
    fn emit_map_display_fn(&mut self, key_te: &TypeExpr, val_te: &TypeExpr) -> FunctionValue<'ctx> {
        let key_name = Self::display_mangle_te(key_te);
        let val_name = Self::display_mangle_te(val_te);
        let type_name = format!("Map_{key_name}_{val_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let slot_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        self.emit_map_display_body(display_fn, slot_ptr, key_te, val_te);

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Emit the body of a `Map[K, V]` Display function. Loads the map handle
    /// from `slot_ptr`, prints `"{"`, drives `karac_map_iter_new` /
    /// `karac_map_iter_next` to walk pairs, per-iteration recurses via
    /// `emit_display_fn_for_type_expr` for K and V with `": "` between
    /// key/value and `", "` between pairs, frees the iterator in the exit
    /// block, and prints `"}"`.
    ///
    /// `is_first` flag is tracked via an i1 alloca because the iterator-driven
    /// loop has no scalar counter (unlike Vec where `i == 0` works).
    ///
    /// Caller positions the builder at `display_fn`'s entry block and is
    /// responsible for emitting the trailing `ret void`.
    fn emit_map_display_body(
        &mut self,
        display_fn: FunctionValue<'ctx>,
        slot_ptr: PointerValue<'ctx>,
        key_te: &TypeExpr,
        val_te: &TypeExpr,
    ) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let bool_t = self.context.bool_type();
        let key_ty = self.llvm_type_for_type_expr(key_te);
        let val_ty = self.llvm_type_for_type_expr(val_te);

        // Print "{".
        let lb = self.builder.build_global_string_ptr("{", "md.lb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[lb.as_pointer_value().into()], "p")
            .unwrap();

        // Load the opaque map handle from slot_ptr.
        let map_handle = self
            .builder
            .build_load(ptr_ty, slot_ptr, "md.handle")
            .unwrap()
            .into_pointer_value();

        // Allocas for the loop's iterator handle, the is_first flag, and the
        // out_key / out_val staging slots. Place them in the entry block via
        // `create_entry_alloca` so they dominate the loop.
        let iter_slot = self.create_entry_alloca(display_fn, "md.iter.slot", ptr_ty.into());
        let first_slot = self.create_entry_alloca(display_fn, "md.first", bool_t.into());
        let out_key = self.create_entry_alloca(display_fn, "md.out_key", key_ty);
        let out_val = self.create_entry_alloca(display_fn, "md.out_val", val_ty);

        // Initialize iter, is_first.
        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[map_handle.into()], "md.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        self.builder.build_store(iter_slot, iter_ptr).unwrap();
        self.builder
            .build_store(first_slot, bool_t.const_int(1, false))
            .unwrap();

        // Materialize (or fetch) the per-key and per-value Display fns.
        let key_disp = self.emit_display_fn_for_type_expr(key_te);
        let val_disp = self.emit_display_fn_for_type_expr(val_te);

        let hdr_bb = self.context.append_basic_block(display_fn, "map.hdr");
        let bdy_bb = self.context.append_basic_block(display_fn, "map.bdy");
        let sep_bb = self.context.append_basic_block(display_fn, "map.sep");
        let pair_bb = self.context.append_basic_block(display_fn, "map.pair");
        let exit_bb = self.context.append_basic_block(display_fn, "map.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        // hdr: advance iterator; loop while it returns true.
        self.builder.position_at_end(hdr_bb);
        let iter_cur = self
            .builder
            .build_load(ptr_ty, iter_slot, "md.iter.cur")
            .unwrap()
            .into_pointer_value();
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_cur.into(), out_key.into(), out_val.into()],
                "md.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, bdy_bb, exit_bb)
            .unwrap();

        // bdy: branch on is_first — first iteration skips the ", " separator
        // and clears the flag; subsequent iterations print ", " first.
        self.builder.position_at_end(bdy_bb);
        let f = self
            .builder
            .build_load(bool_t, first_slot, "md.f")
            .unwrap()
            .into_int_value();
        self.builder
            .build_conditional_branch(f, pair_bb, sep_bb)
            .unwrap();

        // sep: print ", " then fall through to pair.
        self.builder.position_at_end(sep_bb);
        let sep = self
            .builder
            .build_global_string_ptr(", ", "md.sep")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
            .unwrap();
        self.builder.build_unconditional_branch(pair_bb).unwrap();

        // pair: clear is_first (idempotent on second+ iters), print key, ": ",
        // value, then loop back to hdr.
        self.builder.position_at_end(pair_bb);
        self.builder
            .build_store(first_slot, bool_t.const_int(0, false))
            .unwrap();
        self.builder
            .build_call(key_disp, &[out_key.into()], "md.kd")
            .unwrap();
        let colon = self
            .builder
            .build_global_string_ptr(": ", "md.col")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[colon.as_pointer_value().into()], "p")
            .unwrap();
        self.builder
            .build_call(val_disp, &[out_val.into()], "md.vd")
            .unwrap();
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        // exit: free iterator, print "}".
        self.builder.position_at_end(exit_bb);
        let iter_final = self
            .builder
            .build_load(ptr_ty, iter_slot, "md.iter.final")
            .unwrap()
            .into_pointer_value();
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_final.into()], "")
            .unwrap();
        let rb = self.builder.build_global_string_ptr("}", "md.rb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rb.as_pointer_value().into()], "p")
            .unwrap();
    }

    /// Emit (or reuse) a Display function for `Set[T]`. Typed entry point —
    /// shape mirrors `emit_map_display_fn` minus the value-side Display
    /// (Set lowers to `Map[T, ()]`; the iterator's value out-slot is sized
    /// 0 and the contents are discarded).
    ///
    /// The emitted function is named `karac_display_Set_<elem>` (deeply
    /// mangled via `display_mangle_te`) and shares the generic Display
    /// cache. Format `Set{a, b, c}` with the literal `Set` prefix matches
    /// the interpreter at `src/interpreter.rs:292`. Iteration order is
    /// unspecified per `design.md` line 1588 — tests must not assert order.
    fn emit_set_display_fn(&mut self, elem_te: &TypeExpr) -> FunctionValue<'ctx> {
        let elem_name = Self::display_mangle_te(elem_te);
        let type_name = format!("Set_{elem_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let slot_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        self.emit_set_display_body(display_fn, slot_ptr, elem_te);

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Body of the Set Display fn. Loads the opaque map handle (Set lowers
    /// to `Map[T, ()]`), prints `Set{`, walks `karac_map_iter_*` printing
    /// each element via the per-type Display fn with `, ` between, frees
    /// the iterator, prints `}`. The val out-slot is sized 0 — a single
    /// shared `i8` alloca — and its contents are discarded.
    fn emit_set_display_body(
        &mut self,
        display_fn: FunctionValue<'ctx>,
        slot_ptr: PointerValue<'ctx>,
        elem_te: &TypeExpr,
    ) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let bool_t = self.context.bool_type();
        let i8_t = self.context.i8_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);

        // Print "Set{" — literal prefix matches the interpreter format at
        // `src/interpreter.rs:292`.
        let lb = self
            .builder
            .build_global_string_ptr("Set{", "sd.lb")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[lb.as_pointer_value().into()], "p")
            .unwrap();

        // Load the opaque set/map handle from slot_ptr.
        let set_handle = self
            .builder
            .build_load(ptr_ty, slot_ptr, "sd.handle")
            .unwrap()
            .into_pointer_value();

        let iter_slot = self.create_entry_alloca(display_fn, "sd.iter.slot", ptr_ty.into());
        let first_slot = self.create_entry_alloca(display_fn, "sd.first", bool_t.into());
        let out_elem = self.create_entry_alloca(display_fn, "sd.out_elem", elem_ty);
        // val_size = 0 — a single shared i8 alloca for the discarded
        // value out-slot. Runtime stores zero bytes regardless.
        let dummy_val = self.create_entry_alloca(display_fn, "sd.dummy", i8_t.into());

        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[set_handle.into()], "sd.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        self.builder.build_store(iter_slot, iter_ptr).unwrap();
        self.builder
            .build_store(first_slot, bool_t.const_int(1, false))
            .unwrap();

        let elem_disp = self.emit_display_fn_for_type_expr(elem_te);

        let hdr_bb = self.context.append_basic_block(display_fn, "set.hdr");
        let bdy_bb = self.context.append_basic_block(display_fn, "set.bdy");
        let sep_bb = self.context.append_basic_block(display_fn, "set.sep");
        let elem_bb = self.context.append_basic_block(display_fn, "set.elem");
        let exit_bb = self.context.append_basic_block(display_fn, "set.exit");

        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let iter_cur = self
            .builder
            .build_load(ptr_ty, iter_slot, "sd.iter.cur")
            .unwrap()
            .into_pointer_value();
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_cur.into(), out_elem.into(), dummy_val.into()],
                "sd.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        let f = self
            .builder
            .build_load(bool_t, first_slot, "sd.f")
            .unwrap()
            .into_int_value();
        self.builder
            .build_conditional_branch(f, elem_bb, sep_bb)
            .unwrap();

        self.builder.position_at_end(sep_bb);
        let sep = self
            .builder
            .build_global_string_ptr(", ", "sd.sep")
            .unwrap();
        self.builder
            .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
            .unwrap();
        self.builder.build_unconditional_branch(elem_bb).unwrap();

        self.builder.position_at_end(elem_bb);
        self.builder
            .build_store(first_slot, bool_t.const_int(0, false))
            .unwrap();
        self.builder
            .build_call(elem_disp, &[out_elem.into()], "sd.ed")
            .unwrap();
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        let iter_final = self
            .builder
            .build_load(ptr_ty, iter_slot, "sd.iter.final")
            .unwrap()
            .into_pointer_value();
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_final.into()], "")
            .unwrap();
        let rb = self.builder.build_global_string_ptr("}", "sd.rb").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rb.as_pointer_value().into()], "p")
            .unwrap();
    }

    /// Deeply mangled type name suitable for Display cache keys. Unlike
    /// `mangled_type_name` (which is shallow on `Path` types — used for
    /// hash/eq, where `Map[Vec[T], V]` is unreachable so deep mangling is
    /// unnecessary), this walks generic args so `Vec[i64]` → `Vec_i64`,
    /// `Map[String, i64]` → `Map_String_i64`, and nested shapes compose.
    /// Tuples use the same `tuple_T1_T2_...` form `mangled_type_name`
    /// produces — the recursive shapes match.
    fn display_mangle_te(te: &TypeExpr) -> String {
        match &te.kind {
            TypeKind::Tuple(elems) if elems.is_empty() => "unit".to_string(),
            TypeKind::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(Self::display_mangle_te).collect();
                format!("tuple_{}", parts.join("_"))
            }
            TypeKind::Path(p) => {
                let head = p
                    .segments
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                if let Some(args) = p.generic_args.as_ref() {
                    let parts: Vec<String> = args
                        .iter()
                        .filter_map(|a| match a {
                            GenericArg::Type(t) => Some(Self::display_mangle_te(t)),
                            _ => None,
                        })
                        .collect();
                    if !parts.is_empty() {
                        return format!("{head}_{}", parts.join("_"));
                    }
                }
                head
            }
            _ => "unknown".to_string(),
        }
    }

    /// TypeExpr-aware Display dispatcher. Canonical entry point for any
    /// caller that holds a source-level `TypeExpr`: routes by shape to the
    /// typed Vec / Map / Tuple entry points, and falls through to the
    /// by-name `emit_display_fn_for_type` for primitives. Mirror of
    /// `emit_hash_fn_for_type_expr` / `emit_eq_fn_for_type_expr`.
    ///
    /// Cache-key check up front so the dispatcher itself is cheap on repeat
    /// calls — every typed entry point (`emit_*_display_fn_te` /
    /// `emit_tuple_display_fn`) also re-checks before emitting, but doing it
    /// here avoids the per-shape branching cost when the function already
    /// exists.
    fn emit_display_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::display_mangle_te(te);
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => self.emit_tuple_display_fn(elems),
            TypeKind::Path(p) => {
                let head = p.segments.first().map(String::as_str);
                if head == Some("Vec") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        return self.emit_vec_display_fn_te(&elem_te);
                    }
                }
                if head == Some("Map") {
                    let args = p.generic_args.as_ref();
                    let k_te = args.and_then(|a| a.first()).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    let v_te = args.and_then(|a| a.get(1)).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    if let (Some(k), Some(v)) = (k_te, v_te) {
                        return self.emit_map_display_fn(&k, &v);
                    }
                }
                if head == Some("Set") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        return self.emit_set_display_fn(&elem_te);
                    }
                }
                // Primitive (or unsupported path) — fall through to by-name.
                let llvm_ty = self.llvm_type_for_type_expr(te);
                self.emit_display_fn_for_type(&type_name, llvm_ty)
            }
            _ => {
                let llvm_ty = self.llvm_type_for_type_expr(te);
                self.emit_display_fn_for_type(&type_name, llvm_ty)
            }
        }
    }

    /// Emit (or reuse) a typed Display function for `Vec[T]`. The function
    /// is named `karac_display_Vec_<elem_mangled>` and shares the generic
    /// `display_fn_cache` keyed on the same mangled name; the catch-all
    /// `Vec_*` arm in `emit_display_fn_for_type` panics on cache miss to
    /// steer callers here. Body delegates to `emit_vec_display_body` which
    /// recurses via `emit_display_fn_for_type_expr` for the element type.
    fn emit_vec_display_fn_te(&mut self, elem_te: &TypeExpr) -> FunctionValue<'ctx> {
        let elem_name = Self::display_mangle_te(elem_te);
        let type_name = format!("Vec_{elem_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_display_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();

        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        self.emit_vec_display_body(display_fn, val_ptr, elem_te);

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        display_fn
    }

    /// Emit (or reuse) a typed Display function for an n-tuple
    /// `(T1, T2, …, Tn)`. Typed entry point — distinct from the by-name
    /// `emit_display_fn_for_type` because per-field `TypeExpr`s can't be
    /// recovered from a single mangled name string once nested compound
    /// shapes (`((i64, i64), String)`) are in play. Mirror of the
    /// `emit_map_display_fn` pattern.
    ///
    /// Cache key (and function name suffix) is the deeply-mangled name —
    /// `tuple_T1_T2_..._Tn`. Shares the generic `display_fn_cache` so a
    /// later `emit_display_fn_for_type` cache hit on the same name returns
    /// this function (the catch-all `tuple_*` arm panics on cache miss to
    /// steer callers here).
    ///
    /// Calling convention: `void karac_display_tuple_T1_T2_..._Tn(ptr p)`
    /// where `p` points to the LLVM tuple struct value (one alloca'd or
    /// in-struct field address). Body reads each field via `getelementptr`
    /// on the tuple's LLVM struct type, recurses via
    /// `emit_display_fn_for_type_expr` for each field, and prints
    /// `(field0, field1, ...)` with `, ` between fields. Format matches
    /// the interpreter's tuple Display at `src/interpreter.rs:215`.
    fn emit_tuple_display_fn(&mut self, elems: &[TypeExpr]) -> FunctionValue<'ctx> {
        // Cache lookup. Compute the canonical name first so module + cache
        // checks share one key.
        let parts: Vec<String> = elems.iter().map(Self::display_mangle_te).collect();
        let type_name = format!("tuple_{}", parts.join("_"));
        let fn_name = format!("karac_display_{type_name}");
        if let Some(&f) = self.display_fn_cache.get(&type_name) {
            return f;
        }
        if let Some(f) = self.module.get_function(&fn_name) {
            self.display_fn_cache.insert(type_name, f);
            return f;
        }

        let elems_owned: Vec<TypeExpr> = elems.to_vec();

        // Materialize per-field Display fns first. Each recursive emit
        // saves and restores the builder position, so calling them before
        // we open this function's body is safe — the alternative (calling
        // mid-emission) would require careful position management.
        let field_disps: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_display_fn_for_type_expr(e))
            .collect();

        // Compute the tuple's LLVM struct type. Must match exactly what
        // `llvm_type_for_type_expr(Tuple(...))` produces so callers can pass
        // their tuple value's address directly to this function.
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let saved_bb = self.builder.get_insert_block();
        let display_fn_ty = self.context.void_type().fn_type(&[ptr_ty.into()], false);
        let display_fn = self
            .module
            .add_function(&fn_name, display_fn_ty, Some(Linkage::Internal));
        self.display_fn_cache.insert(type_name, display_fn);

        let entry_bb = self.context.append_basic_block(display_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let val_ptr = display_fn.get_nth_param(0).unwrap().into_pointer_value();

        // Print "(".
        let lp = self.builder.build_global_string_ptr("(", "td.lp").unwrap();
        self.builder
            .build_call(self.printf_fn, &[lp.as_pointer_value().into()], "p")
            .unwrap();

        for (i, fd) in field_disps.iter().enumerate() {
            if i > 0 {
                let sep = self
                    .builder
                    .build_global_string_ptr(", ", "td.sep")
                    .unwrap();
                self.builder
                    .build_call(self.printf_fn, &[sep.as_pointer_value().into()], "p")
                    .unwrap();
            }
            let field_ptr = self
                .builder
                .build_struct_gep(tuple_ty, val_ptr, i as u32, &format!("t.f{i}.p"))
                .unwrap();
            self.builder
                .build_call(*fd, &[field_ptr.into()], &format!("t.f{i}.d"))
                .unwrap();
        }

        // Print ")".
        let rp = self.builder.build_global_string_ptr(")", "td.rp").unwrap();
        self.builder
            .build_call(self.printf_fn, &[rp.as_pointer_value().into()], "p")
            .unwrap();

        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        display_fn
    }

    /// Emit `karac_map_new`, alloca a ptr slot to hold the opaque handle, and
    /// register a scope-exit `karac_map_free` cleanup action.
    /// Called from `compile_stmt` when the RHS is `Map.new()`.
    fn compile_map_new_stmt(&mut self, var_name: &str) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let key_ty = self
            .map_key_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        let key_size = key_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let val_size = val_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));

        // Emit (or reuse) hash/eq functions for the concrete key type.
        // Prefer the TypeExpr-aware path so compound key shapes (tuples, …)
        // compose correctly via per-field recursion. The plain
        // `emit_hash_fn_for_type` path is the fallback for code paths that
        // never registered a `TypeExpr` for the variable.
        let (hash_fn, eq_fn) = if let Some(key_te) = self.map_key_type_exprs.get(var_name).cloned()
        {
            (
                self.emit_hash_fn_for_type_expr(&key_te),
                self.emit_eq_fn_for_type_expr(&key_te),
            )
        } else {
            let type_name = self
                .map_key_type_names
                .get(var_name)
                .cloned()
                .unwrap_or_else(|| "i64".to_string());
            (
                self.emit_hash_fn_for_type(&type_name, key_ty),
                self.emit_eq_fn_for_type(&type_name, key_ty),
            )
        };
        let hash_fn_ptr = hash_fn.as_global_value().as_pointer_value();
        let eq_fn_ptr = eq_fn.as_global_value().as_pointer_value();

        let map_handle = self
            .builder
            .build_call(
                self.karac_map_new_fn,
                &[
                    key_size.into(),
                    val_size.into(),
                    hash_fn_ptr.into(),
                    eq_fn_ptr.into(),
                ],
                "map.new",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let fn_val = self.current_fn.unwrap();
        let slot_ptr = self.create_entry_alloca(fn_val, &format!("{var_name}.slot"), ptr_ty.into());
        self.builder.build_store(slot_ptr, map_handle).unwrap();
        self.variables.insert(
            var_name.to_string(),
            VarSlot {
                ptr: slot_ptr,
                ty: ptr_ty.into(),
            },
        );
        self.track_map_var(slot_ptr);
        Ok(())
    }

    /// Compile `let s: Set[T] = Set.new()` — emit `karac_map_new(elem_size,
    /// 0, hash_fn, eq_fn)` (val_size = 0 → key-only table), alloca a slot
    /// for the opaque handle, register the scope-exit `karac_map_free`
    /// cleanup. Mirrors `compile_map_new_stmt` with the value side stripped.
    fn compile_set_new_stmt(&mut self, var_name: &str) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let elem_ty = self
            .set_elem_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        let elem_size = elem_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let val_size = i64_t.const_int(0, false);

        // Hash/eq fns for the element type. Prefer the TypeExpr-aware path
        // so compound element shapes (tuples, …) compose correctly.
        let (hash_fn, eq_fn) =
            if let Some(elem_te) = self.set_elem_type_exprs.get(var_name).cloned() {
                (
                    self.emit_hash_fn_for_type_expr(&elem_te),
                    self.emit_eq_fn_for_type_expr(&elem_te),
                )
            } else {
                let type_name = self
                    .set_elem_type_names
                    .get(var_name)
                    .cloned()
                    .unwrap_or_else(|| "i64".to_string());
                (
                    self.emit_hash_fn_for_type(&type_name, elem_ty),
                    self.emit_eq_fn_for_type(&type_name, elem_ty),
                )
            };
        let hash_fn_ptr = hash_fn.as_global_value().as_pointer_value();
        let eq_fn_ptr = eq_fn.as_global_value().as_pointer_value();

        let set_handle = self
            .builder
            .build_call(
                self.karac_map_new_fn,
                &[
                    elem_size.into(),
                    val_size.into(),
                    hash_fn_ptr.into(),
                    eq_fn_ptr.into(),
                ],
                "set.new",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let fn_val = self.current_fn.unwrap();
        let slot_ptr = self.create_entry_alloca(fn_val, &format!("{var_name}.slot"), ptr_ty.into());
        self.builder.build_store(slot_ptr, set_handle).unwrap();
        self.variables.insert(
            var_name.to_string(),
            VarSlot {
                ptr: slot_ptr,
                ty: ptr_ty.into(),
            },
        );
        // Set handles use the same `karac_map_free` cleanup as Map handles —
        // the runtime is the same; only the type-system identity differs.
        self.track_map_var(slot_ptr);
        Ok(())
    }

    /// Compile `let m: Map[K, V] = ["k1": v1, "k2": v2, ...]` (bare or prefix
    /// `Map[k1: v1, ...]` form — both lower to `ExprKind::MapLiteral`). Calls
    /// `compile_map_new_stmt` first to build the empty map + register the
    /// binding + cleanup tracking, then inserts each entry via
    /// `karac_map_insert_old` (discarding the previous-value out-slot since
    /// every key is fresh on construction).
    fn compile_map_literal_stmt(
        &mut self,
        var_name: &str,
        entries: &[(Expr, Expr)],
    ) -> Result<(), String> {
        // Build the empty map first (registers slot + cleanup).
        self.compile_map_new_stmt(var_name)?;

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();

        let slot = self
            .variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("compile_map_literal_stmt: '{var_name}' not registered"))?;
        let key_ty = self
            .map_key_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        // Reuse a single set of allocas across all inserts in the literal —
        // the storage is overwritten per iteration.
        let key_slot = self.create_entry_alloca(fn_val, "map.lit.key", key_ty);
        let val_slot = self.create_entry_alloca(fn_val, "map.lit.val", val_ty);
        let old_slot = self.create_entry_alloca(fn_val, "map.lit.old", val_ty);

        for (k_expr, v_expr) in entries {
            let map_handle = self
                .builder
                .build_load(ptr_ty, slot.ptr, "map.lit.handle")
                .unwrap()
                .into_pointer_value();
            let k_val = self.compile_expr(k_expr)?;
            let v_val = self.compile_expr(v_expr)?;
            self.builder.build_store(key_slot, k_val).unwrap();
            self.builder.build_store(val_slot, v_val).unwrap();
            self.builder
                .build_call(
                    self.karac_map_insert_old_fn,
                    &[
                        map_handle.into(),
                        key_slot.into(),
                        val_slot.into(),
                        old_slot.into(),
                    ],
                    "map.lit.insert",
                )
                .unwrap();
        }

        Ok(())
    }

    /// Compile `m[k] = v` index-store on a `Map[K, V]` variable. Lowers to
    /// `karac_map_insert_old` and discards the previous-value out-slot. The
    /// write path is uniform regardless of whether the key already exists —
    /// `karac_map_insert_old` overwrites or fresh-inserts as appropriate.
    fn compile_map_index_store(
        &mut self,
        name: &str,
        index: &Expr,
        val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let slot = self
            .variables
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{name}' in index-store"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, slot.ptr, "map.idxst.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());

        let key_val = self.compile_expr(index)?;
        let fn_val = self.current_fn.unwrap();
        let key_slot = self.create_entry_alloca(fn_val, "map.idxst.key", key_ty);
        let val_slot = self.create_entry_alloca(fn_val, "map.idxst.val", val_ty);
        let old_slot = self.create_entry_alloca(fn_val, "map.idxst.old", val_ty);
        self.builder.build_store(key_slot, key_val).unwrap();
        self.builder.build_store(val_slot, val).unwrap();

        self.builder
            .build_call(
                self.karac_map_insert_old_fn,
                &[
                    map_handle.into(),
                    key_slot.into(),
                    val_slot.into(),
                    old_slot.into(),
                ],
                "map.idxst.existed",
            )
            .unwrap();

        Ok(())
    }

    /// Compile `m[k]` indexing on a `Map[K, V]` variable. Panics at runtime if
    /// the key is missing — matches the spec's `fn index(ref self, key: ref K)
    /// -> ref V` semantics. The returned value is a bit-copy of the bucket's V,
    /// not a borrow into the bucket; this matches the existing `Map.get`
    /// codegen behavior. Proper `ref V` return semantics is a follow-up that
    /// applies uniformly to both `[]` and `Map.get`.
    fn compile_map_index(
        &mut self,
        name: &str,
        index: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let slot = self
            .variables
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{name}' in index expression"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, slot.ptr, "map.idx.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(name)
            .copied()
            .unwrap_or(i64_t.into());

        let key_val = self.compile_expr(index)?;
        let fn_val = self.current_fn.unwrap();
        let key_slot = self.create_entry_alloca(fn_val, "map.idx.key", key_ty);
        let val_slot = self.create_entry_alloca(fn_val, "map.idx.val", val_ty);
        self.builder.build_store(key_slot, key_val).unwrap();

        let found = self
            .builder
            .build_call(
                self.karac_map_get_fn,
                &[map_handle.into(), key_slot.into(), val_slot.into()],
                "map.idx.found",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();

        let found_bb = self.context.append_basic_block(fn_val, "map.idx.found");
        let notfound_bb = self.context.append_basic_block(fn_val, "map.idx.notfound");

        self.builder
            .build_conditional_branch(found, found_bb, notfound_bb)
            .unwrap();

        self.builder.position_at_end(notfound_bb);
        self.emit_panic("Map index: key not present");
        self.builder.build_unreachable().unwrap();

        self.builder.position_at_end(found_bb);
        let elem_val = self
            .builder
            .build_load(val_ty, val_slot, "map.idx.val")
            .unwrap();
        Ok(elem_val)
    }

    /// Compile `Map.keys()`, `Map.values()`, or `Map.entries()` — each
    /// materializes a fresh Vec by iterating the map. Pre-allocates the result
    /// buffer at `karac_map_len` capacity (matches Rust's reserve-then-fill
    /// pattern for known-size collections), then writes elements at index `i`
    /// via the iterator. Returns the resulting Vec struct value `{data, len,
    /// cap}` directly; the receiving binding's let-statement registers it for
    /// scope cleanup via the existing `vec_elem_types` machinery (the type
    /// annotation `let v: Vec[K] = m.keys()` drives that path).
    ///
    /// Iteration order is unspecified — matches the spec at design.md
    /// "Iteration order is unspecified" (line 1588).
    fn compile_map_keys_values_entries(
        &mut self,
        var_name: &str,
        method: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let fn_val = self.current_fn.unwrap();

        let slot = self
            .variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, slot.ptr, "kvg.map.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        // Resulting Vec's element type depends on which method we're emitting.
        // For `entries`, the element is the {K, V} tuple struct — same shape
        // as `extract_vec_elem_type` produces for `Vec[(K, V)]`.
        let elem_ty: BasicTypeEnum<'ctx> = match method {
            "keys" => key_ty,
            "values" => val_ty,
            "entries" => self.context.struct_type(&[key_ty, val_ty], false).into(),
            _ => {
                return Err(format!(
                    "compile_map_keys_values_entries: unexpected method '{method}'"
                ))
            }
        };

        let elem_size = elem_ty.size_of().unwrap();

        // len = karac_map_len(map)
        let len = self
            .builder
            .build_call(self.karac_map_len_fn, &[map_handle.into()], "kvg.len")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();

        // Allocate buffer: malloc(len * elem_size). On len == 0 this calls
        // malloc(0) — implementation-defined; the resulting Vec carries cap=0
        // so scope cleanup never frees it (the bytes leak only on empty maps,
        // a pre-existing pattern shared with empty Vec literals).
        let alloc_bytes = self
            .builder
            .build_int_mul(len, elem_size, "kvg.alloc.bytes")
            .unwrap();
        let buf = self
            .builder
            .build_call(self.malloc_fn, &[alloc_bytes.into()], "kvg.buf")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Map iterator + per-iteration out-slots.
        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[map_handle.into()], "kvg.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let out_key = self.create_entry_alloca(fn_val, "kvg.out.k", key_ty);
        let out_val = self.create_entry_alloca(fn_val, "kvg.out.v", val_ty);
        let i_slot = self.create_entry_alloca(fn_val, "kvg.i", i64_t.into());
        self.builder
            .build_store(i_slot, i64_t.const_int(0, false))
            .unwrap();

        let loop_bb = self.context.append_basic_block(fn_val, "kvg.loop");
        let body_bb = self.context.append_basic_block(fn_val, "kvg.body");
        let exit_bb = self.context.append_basic_block(fn_val, "kvg.exit");

        self.builder.build_unconditional_branch(loop_bb).unwrap();

        // loop_bb: advance iterator; branch on result.
        self.builder.position_at_end(loop_bb);
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_ptr.into(), out_key.into(), out_val.into()],
                "kvg.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, body_bb, exit_bb)
            .unwrap();

        // body_bb: load key/val from slots, build the element value, write
        // into buf[i], increment i.
        self.builder.position_at_end(body_bb);
        let i_val = self
            .builder
            .build_load(i64_t, i_slot, "kvg.i.cur")
            .unwrap()
            .into_int_value();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, buf, &[i_val], "kvg.elem.ptr")
                .unwrap()
        };
        let written: BasicValueEnum<'ctx> = match method {
            "keys" => self
                .builder
                .build_load(key_ty, out_key, "kvg.k.load")
                .unwrap(),
            "values" => self
                .builder
                .build_load(val_ty, out_val, "kvg.v.load")
                .unwrap(),
            "entries" => {
                let kv_struct_ty = self.context.struct_type(&[key_ty, val_ty], false);
                let key_val = self
                    .builder
                    .build_load(key_ty, out_key, "kvg.k.load")
                    .unwrap();
                let val_val = self
                    .builder
                    .build_load(val_ty, out_val, "kvg.v.load")
                    .unwrap();
                let mut kv = kv_struct_ty.get_undef();
                kv = self
                    .builder
                    .build_insert_value(kv, key_val, 0, "kv.k")
                    .unwrap()
                    .into_struct_value();
                kv = self
                    .builder
                    .build_insert_value(kv, val_val, 1, "kv.v")
                    .unwrap()
                    .into_struct_value();
                kv.into()
            }
            _ => unreachable!(),
        };
        self.builder.build_store(elem_ptr, written).unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "kvg.i.next")
            .unwrap();
        self.builder.build_store(i_slot, i_next).unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();

        // exit_bb: free iterator, build Vec struct {data, len, cap=len}.
        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_ptr.into()], "")
            .unwrap();

        let mut vec_val = vec_ty.get_undef();
        vec_val = self
            .builder
            .build_insert_value(vec_val, buf, 0, "vec.data")
            .unwrap()
            .into_struct_value();
        vec_val = self
            .builder
            .build_insert_value(vec_val, len, 1, "vec.len")
            .unwrap()
            .into_struct_value();
        vec_val = self
            .builder
            .build_insert_value(vec_val, len, 2, "vec.cap")
            .unwrap()
            .into_struct_value();

        Ok(vec_val.into())
    }

    /// Compile a method call on a `Map[K,V]` variable.
    fn compile_map_method(
        &mut self,
        var_name: &str,
        method: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();

        let slot = self
            .variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;

        // Load the opaque map handle from the ptr alloca.
        let map_handle = self
            .builder
            .build_load(ptr_ty, slot.ptr, "map.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        match method {
            "len" => {
                let len = self
                    .builder
                    .build_call(self.karac_map_len_fn, &[map_handle.into()], "map.len")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(len)
            }
            "is_empty" => {
                let len = self
                    .builder
                    .build_call(self.karac_map_len_fn, &[map_handle.into()], "map.len")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                let zero = i64_t.const_int(0, false);
                let cmp = self
                    .builder
                    .build_int_compare(inkwell::IntPredicate::EQ, len, zero, "map.is_empty")
                    .unwrap();
                Ok(cmp.into())
            }
            "insert" => {
                if args.len() < 2 {
                    return Err("Map.insert requires key and value arguments".to_string());
                }
                let option_ty = self.enum_layouts["Option"].llvm_type;
                let key_val = self.compile_expr(&args[0].value)?;
                let val_val = self.compile_expr(&args[1].value)?;
                let fn_val = self.current_fn.unwrap();
                let key_slot = self.create_entry_alloca(fn_val, "map.insert.key", key_ty);
                let val_slot = self.create_entry_alloca(fn_val, "map.insert.val", val_ty);
                let old_slot = self.create_entry_alloca(fn_val, "map.insert.old", val_ty);
                self.builder.build_store(key_slot, key_val).unwrap();
                self.builder.build_store(val_slot, val_val).unwrap();
                let existed = self
                    .builder
                    .build_call(
                        self.karac_map_insert_old_fn,
                        &[
                            map_handle.into(),
                            key_slot.into(),
                            val_slot.into(),
                            old_slot.into(),
                        ],
                        "map.insert.existed",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                // Build Option[V]: Some(old) if existed, None if fresh insert.
                let some_bb = self.context.append_basic_block(fn_val, "map.ins.some");
                let none_bb = self.context.append_basic_block(fn_val, "map.ins.none");
                let merge_bb = self.context.append_basic_block(fn_val, "map.ins.merge");
                self.builder
                    .build_conditional_branch(existed, some_bb, none_bb)
                    .unwrap();
                self.builder.position_at_end(some_bb);
                let old_val = self
                    .builder
                    .build_load(val_ty, old_slot, "map.ins.old")
                    .unwrap();
                let old_word = self.coerce_to_i64(old_val)?;
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(none_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                let tag_phi = self.builder.build_phi(i64_t, "ins.opt.tag").unwrap();
                tag_phi.add_incoming(&[
                    (&i64_t.const_int(1, false), some_bb),
                    (&i64_t.const_int(0, false), none_bb),
                ]);
                let word_phi = self.builder.build_phi(i64_t, "ins.opt.word").unwrap();
                word_phi
                    .add_incoming(&[(&old_word, some_bb), (&i64_t.const_int(0, false), none_bb)]);
                let mut agg = option_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, tag_phi.as_basic_value(), 0, "ins.opt.tag.f")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, word_phi.as_basic_value(), 1, "ins.opt.word.f")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            "get" => {
                if args.is_empty() {
                    return Err("Map.get requires a key argument".to_string());
                }
                let option_ty = self.enum_layouts["Option"].llvm_type;
                let key_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                let key_slot = self.create_entry_alloca(fn_val, "map.get.key", key_ty);
                let val_slot = self.create_entry_alloca(fn_val, "map.get.val", val_ty);
                self.builder.build_store(key_slot, key_val).unwrap();

                let found = self
                    .builder
                    .build_call(
                        self.karac_map_get_fn,
                        &[map_handle.into(), key_slot.into(), val_slot.into()],
                        "map.get.found",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();

                let found_bb = self.context.append_basic_block(fn_val, "map.get.found.bb");
                let notfound_bb = self
                    .context
                    .append_basic_block(fn_val, "map.get.notfound.bb");
                let merge_bb = self.context.append_basic_block(fn_val, "map.get.merge");

                self.builder
                    .build_conditional_branch(found, found_bb, notfound_bb)
                    .unwrap();

                // Found — load value and coerce to i64 payload.
                self.builder.position_at_end(found_bb);
                let elem_val = self
                    .builder
                    .build_load(val_ty, val_slot, "map.get.val")
                    .unwrap();
                let elem_word = self.coerce_to_i64(elem_val)?;
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Not found.
                self.builder.position_at_end(notfound_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();

                // Merge — phi and build Option struct.
                self.builder.position_at_end(merge_bb);
                let tag_phi = self.builder.build_phi(i64_t, "opt.tag").unwrap();
                tag_phi.add_incoming(&[
                    (&i64_t.const_int(1, false), found_bb),
                    (&i64_t.const_int(0, false), notfound_bb),
                ]);
                let word_phi = self.builder.build_phi(i64_t, "opt.word").unwrap();
                word_phi.add_incoming(&[
                    (&elem_word, found_bb),
                    (&i64_t.const_int(0, false), notfound_bb),
                ]);
                let mut agg = option_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, tag_phi.as_basic_value(), 0, "opt.tag.f")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, word_phi.as_basic_value(), 1, "opt.word.f")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            "remove" => {
                if args.is_empty() {
                    return Err("Map.remove requires a key argument".to_string());
                }
                let option_ty = self.enum_layouts["Option"].llvm_type;
                let key_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                let key_slot = self.create_entry_alloca(fn_val, "map.remove.key", key_ty);
                let old_slot = self.create_entry_alloca(fn_val, "map.remove.old", val_ty);
                self.builder.build_store(key_slot, key_val).unwrap();
                let found = self
                    .builder
                    .build_call(
                        self.karac_map_remove_old_fn,
                        &[map_handle.into(), key_slot.into(), old_slot.into()],
                        "map.remove.found",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                // Build Option[V]: Some(old) if found, None otherwise.
                let found_bb = self.context.append_basic_block(fn_val, "map.rm.found");
                let notfound_bb = self.context.append_basic_block(fn_val, "map.rm.notfound");
                let merge_bb = self.context.append_basic_block(fn_val, "map.rm.merge");
                self.builder
                    .build_conditional_branch(found, found_bb, notfound_bb)
                    .unwrap();
                self.builder.position_at_end(found_bb);
                let old_val = self
                    .builder
                    .build_load(val_ty, old_slot, "map.rm.old")
                    .unwrap();
                let old_word = self.coerce_to_i64(old_val)?;
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(notfound_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                let tag_phi = self.builder.build_phi(i64_t, "rm.opt.tag").unwrap();
                tag_phi.add_incoming(&[
                    (&i64_t.const_int(1, false), found_bb),
                    (&i64_t.const_int(0, false), notfound_bb),
                ]);
                let word_phi = self.builder.build_phi(i64_t, "rm.opt.word").unwrap();
                word_phi.add_incoming(&[
                    (&old_word, found_bb),
                    (&i64_t.const_int(0, false), notfound_bb),
                ]);
                let mut agg = option_ty.get_undef();
                agg = self
                    .builder
                    .build_insert_value(agg, tag_phi.as_basic_value(), 0, "rm.opt.tag.f")
                    .unwrap()
                    .into_struct_value();
                agg = self
                    .builder
                    .build_insert_value(agg, word_phi.as_basic_value(), 1, "rm.opt.word.f")
                    .unwrap()
                    .into_struct_value();
                Ok(agg.into())
            }
            "contains_key" => {
                if args.is_empty() {
                    return Err("Map.contains_key requires a key argument".to_string());
                }
                let key_val = self.compile_expr(&args[0].value)?;
                let fn_val = self.current_fn.unwrap();
                let key_slot = self.create_entry_alloca(fn_val, "map.contains.key", key_ty);
                self.builder.build_store(key_slot, key_val).unwrap();
                let found = self
                    .builder
                    .build_call(
                        self.karac_map_contains_fn,
                        &[map_handle.into(), key_slot.into()],
                        "map.contains",
                    )
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                Ok(found.into())
            }
            "clear" => {
                self.builder
                    .build_call(self.karac_map_clear_fn, &[map_handle.into()], "")
                    .unwrap();
                // Map.clear returns Unit — codegen represents Unit as i64 0.
                Ok(i64_t.const_int(0, false).into())
            }
            "keys" | "values" | "entries" => self.compile_map_keys_values_entries(var_name, method),
            _ => Err(format!("codegen: Map.{method} not yet implemented")),
        }
    }

    /// TypeExpr-aware clone-fn dispatcher. Canonical entry point for any
    /// caller that needs a `void karac_clone_<typename>(*const T, *mut T)`
    /// function for type `T`. Routes by shape: primitives (load+store),
    /// String (call runtime helper), Vec[T] (deep clone with elem
    /// recursion), Map[K, V] (iterate + insert into fresh map),
    /// Set[T] (Map[T, ()]), Tuple (per-field recurse). Mirrors
    /// `emit_display_fn_for_type_expr` / `emit_hash_fn_for_type_expr`.
    /// Cached via `clone_fn_cache` on `display_mangle_te(te)`.
    ///
    /// `#[derive(Clone)]` user struct support is a follow-up — emit at the
    /// derive site by walking field types and recursing through this fn.
    fn emit_clone_fn_for_type_expr(&mut self, te: &TypeExpr) -> FunctionValue<'ctx> {
        let type_name = Self::display_mangle_te(te);
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }
        match &te.kind {
            TypeKind::Tuple(elems) if !elems.is_empty() => self.emit_tuple_clone_fn(elems),
            TypeKind::Path(p) => {
                let head = p.segments.first().map(String::as_str);
                if head == Some("Vec") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        return self.emit_vec_clone_fn(&elem_te);
                    }
                }
                if head == Some("Map") {
                    let args = p.generic_args.as_ref();
                    let k_te = args.and_then(|a| a.first()).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    let v_te = args.and_then(|a| a.get(1)).and_then(|a| match a {
                        GenericArg::Type(t) => Some(t.clone()),
                        _ => None,
                    });
                    if let (Some(k), Some(v)) = (k_te, v_te) {
                        return self.emit_map_clone_fn(&k, &v);
                    }
                }
                if head == Some("Set") {
                    if let Some(GenericArg::Type(elem_te)) =
                        p.generic_args.as_ref().and_then(|a| a.first()).cloned()
                    {
                        // Set[T] clones as Map[T, ()] — same iterator + insert
                        // path with a zero-byte value half. The runtime's
                        // `(key_size + val_size).max(1)` keeps allocations
                        // valid (val_size = 0).
                        let unit_te = TypeExpr {
                            kind: TypeKind::Tuple(Vec::new()),
                            span: elem_te.span.clone(),
                        };
                        return self.emit_map_clone_fn(&elem_te, &unit_te);
                    }
                }
                if head == Some("String") {
                    return self.emit_string_clone_fn();
                }
                // Primitive (or unsupported path) — emit the load+store body.
                self.emit_primitive_clone_fn(&type_name, te)
            }
            _ => self.emit_primitive_clone_fn(&type_name, te),
        }
    }

    /// Emit a primitive `karac_clone_<typename>(*const T, *mut T)` whose
    /// body is `*dst = *src` — single load + store. Covers every Copy-by-
    /// memcpy type (i8…i64, u8…u64, f32/f64, bool, char, unit). Cache-keyed
    /// on `type_name` so repeat callers reuse the same fn.
    fn emit_primitive_clone_fn(&mut self, type_name: &str, te: &TypeExpr) -> FunctionValue<'ctx> {
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name.to_string(), f);
            return f;
        }
        let val_ty = self.llvm_type_for_type_expr(te);
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name.to_string(), clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();
        let v = self.builder.build_load(val_ty, src, "v").unwrap();
        self.builder.build_store(dst, v).unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Emit (or fetch) the cloned-String fn — a thin wrapper that just
    /// tail-calls the `karac_string_clone` runtime helper. The wrapper
    /// keeps the per-type clone-fn signature uniform with other types so
    /// callers don't special-case Strings.
    fn emit_string_clone_fn(&mut self) -> FunctionValue<'ctx> {
        let type_name = "String".to_string();
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = "karac_clone_String".to_string();
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();
        self.builder
            .build_call(self.karac_string_clone_fn, &[src.into(), dst.into()], "")
            .unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Emit `karac_clone_Vec_<elem>` — read the source `{data, len, cap}`,
    /// allocate a fresh buffer of the same capacity, walk `0..len` calling
    /// the per-element clone fn through the new dispatcher, write the new
    /// `{data, len, cap}` to dst.
    ///
    /// Empty-source fast path (subtask 9): `len == 0` skips the malloc;
    /// dst gets `{null, 0, 0}` with `cap == 0` matching the static-literal
    /// convention so scope-exit cleanup is a no-op.
    fn emit_vec_clone_fn(&mut self, elem_te: &TypeExpr) -> FunctionValue<'ctx> {
        let elem_name = Self::display_mangle_te(elem_te);
        let type_name = format!("Vec_{elem_name}");
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i8_t = self.context.i8_type();
        let i64_t = self.context.i64_type();
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.llvm_type_for_type_expr(elem_te);
        // Recurse first — emit may switch the builder's insert block.
        let elem_clone = self.emit_clone_fn_for_type_expr(elem_te);

        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();

        // Load src.{data, len, cap}
        let src_data_pp = self
            .builder
            .build_struct_gep(vec_ty, src, 0, "src.data.pp")
            .unwrap();
        let src_len_p = self
            .builder
            .build_struct_gep(vec_ty, src, 1, "src.len.p")
            .unwrap();
        let src_cap_p = self
            .builder
            .build_struct_gep(vec_ty, src, 2, "src.cap.p")
            .unwrap();
        let src_data = self
            .builder
            .build_load(ptr_ty, src_data_pp, "src.data")
            .unwrap()
            .into_pointer_value();
        let src_len = self
            .builder
            .build_load(i64_t, src_len_p, "src.len")
            .unwrap()
            .into_int_value();
        let src_cap = self
            .builder
            .build_load(i64_t, src_cap_p, "src.cap")
            .unwrap()
            .into_int_value();

        // dst.{data, len, cap} GEPs
        let dst_data_pp = self
            .builder
            .build_struct_gep(vec_ty, dst, 0, "dst.data.pp")
            .unwrap();
        let dst_len_p = self
            .builder
            .build_struct_gep(vec_ty, dst, 1, "dst.len.p")
            .unwrap();
        let dst_cap_p = self
            .builder
            .build_struct_gep(vec_ty, dst, 2, "dst.cap.p")
            .unwrap();

        // Empty fast path: len == 0 → {null, 0, 0}.
        let empty_bb = self.context.append_basic_block(clone_fn, "empty");
        let alloc_bb = self.context.append_basic_block(clone_fn, "alloc");
        let is_empty = self
            .builder
            .build_int_compare(IntPredicate::EQ, src_len, i64_t.const_zero(), "is.empty")
            .unwrap();
        self.builder
            .build_conditional_branch(is_empty, empty_bb, alloc_bb)
            .unwrap();

        self.builder.position_at_end(empty_bb);
        self.builder
            .build_store(dst_data_pp, ptr_ty.const_null())
            .unwrap();
        self.builder
            .build_store(dst_len_p, i64_t.const_zero())
            .unwrap();
        self.builder
            .build_store(dst_cap_p, i64_t.const_zero())
            .unwrap();
        self.builder.build_return(None).unwrap();

        // alloc + memcpy-loop path.
        self.builder.position_at_end(alloc_bb);
        let raw_size = elem_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let elem_size = if raw_size.get_type().get_bit_width() == 64 {
            raw_size
        } else {
            self.builder
                .build_int_z_extend(raw_size, i64_t, "esz64")
                .unwrap()
        };
        // Buffer cap matches src.cap when > 0; otherwise (static-literal
        // source with cap=0 but non-zero len) allocate len-byte buffer.
        let cap_zero = self
            .builder
            .build_int_compare(IntPredicate::EQ, src_cap, i64_t.const_zero(), "cap.zero")
            .unwrap();
        let new_cap = self
            .builder
            .build_select(cap_zero, src_len, src_cap, "new.cap")
            .unwrap()
            .into_int_value();
        let alloc_bytes = self
            .builder
            .build_int_mul(new_cap, elem_size, "alloc.bytes")
            .unwrap();
        let new_data = self
            .builder
            .build_call(self.malloc_fn, &[alloc_bytes.into()], "new.data")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Loop: i in 0..len; call elem_clone(src.data + i*size, new_data + i*size).
        let pre_bb = self.builder.get_insert_block().unwrap();
        let hdr_bb = self.context.append_basic_block(clone_fn, "loop.hdr");
        let bdy_bb = self.context.append_basic_block(clone_fn, "loop.bdy");
        let exit_bb = self.context.append_basic_block(clone_fn, "loop.exit");
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let i_phi = self.builder.build_phi(i64_t, "i").unwrap();
        i_phi.add_incoming(&[(&i64_t.const_zero(), pre_bb)]);
        let i_val = i_phi.as_basic_value().into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, i_val, src_len, "cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        let offset = self.builder.build_int_mul(i_val, elem_size, "off").unwrap();
        let src_elem = unsafe {
            self.builder
                .build_gep(i8_t, src_data, &[offset], "src.elem")
                .unwrap()
        };
        let dst_elem = unsafe {
            self.builder
                .build_gep(i8_t, new_data, &[offset], "dst.elem")
                .unwrap()
        };
        self.builder
            .build_call(elem_clone, &[src_elem.into(), dst_elem.into()], "")
            .unwrap();
        let i_next = self
            .builder
            .build_int_add(i_val, i64_t.const_int(1, false), "i.next")
            .unwrap();
        i_phi.add_incoming(&[(&i_next, bdy_bb)]);
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        self.builder.build_store(dst_data_pp, new_data).unwrap();
        self.builder.build_store(dst_len_p, src_len).unwrap();
        self.builder.build_store(dst_cap_p, new_cap).unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Emit a Map[K, V] clone fn. Iterates the source via `karac_map_iter_*`,
    /// per-entry: clone K and V into stack allocas, then `karac_map_insert`
    /// into the fresh destination map. Hash/eq fn pointers come from the
    /// existing TypeExpr-aware emit fns, so compound keys (`Map[(i64, String), V]`)
    /// compose correctly.
    ///
    /// Set[T] reuses this path with V = unit (empty-tuple). The runtime's
    /// `(key_size + val_size).max(1)` keeps the bucket allocation valid
    /// when val_size = 0.
    fn emit_map_clone_fn(&mut self, key_te: &TypeExpr, val_te: &TypeExpr) -> FunctionValue<'ctx> {
        let key_name = Self::display_mangle_te(key_te);
        let val_name = Self::display_mangle_te(val_te);
        let type_name = format!("Map_{key_name}_{val_name}");
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let key_ty = self.llvm_type_for_type_expr(key_te);
        let val_ty = self.llvm_type_for_type_expr(val_te);
        let hash_fn = self.emit_hash_fn_for_type_expr(key_te);
        let eq_fn = self.emit_eq_fn_for_type_expr(key_te);
        let key_clone = self.emit_clone_fn_for_type_expr(key_te);
        let val_clone = self.emit_clone_fn_for_type_expr(val_te);

        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();

        // Load source map handle.
        let src_handle = self
            .builder
            .build_load(ptr_ty, src, "src.handle")
            .unwrap()
            .into_pointer_value();

        // Allocate a fresh map. Sizes = sizeof(K), sizeof(V); val_size = 0
        // for Set's unit-tuple case is fine since llvm_type_for_type_expr
        // on empty-tuple returns i64 → size 8. For a true zero-size value,
        // we'd need extra plumbing; the runtime's `.max(1)` already keeps
        // the allocation valid so 8-byte slots are harmless overhead.
        let key_size = key_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let val_size = val_ty
            .size_of()
            .unwrap_or_else(|| i64_t.const_int(8, false));
        let new_handle = self
            .builder
            .build_call(
                self.karac_map_new_fn,
                &[
                    key_size.into(),
                    val_size.into(),
                    hash_fn.as_global_value().as_pointer_value().into(),
                    eq_fn.as_global_value().as_pointer_value().into(),
                ],
                "new.map",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Stack allocas for the iterator's key/val out-slots and for the
        // cloned key/val we pass to `karac_map_insert`.
        let key_out = self.create_entry_alloca(clone_fn, "k.out", key_ty);
        let val_out = self.create_entry_alloca(clone_fn, "v.out", val_ty);
        let key_clone_slot = self.create_entry_alloca(clone_fn, "k.clone", key_ty);
        let val_clone_slot = self.create_entry_alloca(clone_fn, "v.clone", val_ty);

        // Iterator handle.
        let iter_handle = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[src_handle.into()], "iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let hdr_bb = self.context.append_basic_block(clone_fn, "iter.hdr");
        let bdy_bb = self.context.append_basic_block(clone_fn, "iter.bdy");
        let exit_bb = self.context.append_basic_block(clone_fn, "iter.exit");
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(hdr_bb);
        let has = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_handle.into(), key_out.into(), val_out.into()],
                "iter.has",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has, bdy_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(bdy_bb);
        // Clone key and value into fresh allocas, then insert.
        self.builder
            .build_call(key_clone, &[key_out.into(), key_clone_slot.into()], "")
            .unwrap();
        self.builder
            .build_call(val_clone, &[val_out.into(), val_clone_slot.into()], "")
            .unwrap();
        self.builder
            .build_call(
                self.karac_map_insert_fn(),
                &[
                    new_handle.into(),
                    key_clone_slot.into(),
                    val_clone_slot.into(),
                ],
                "",
            )
            .unwrap();
        self.builder.build_unconditional_branch(hdr_bb).unwrap();

        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_handle.into()], "")
            .unwrap();
        self.builder.build_store(dst, new_handle).unwrap();
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
    }

    /// Helper: get-or-declare the `karac_map_insert(map, key, val) -> void`
    /// runtime fn. We don't use `karac_map_insert_old` here because the
    /// fresh destination map is empty by construction — there's no old
    /// value to capture.
    fn karac_map_insert_fn(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("karac_map_insert") {
            return f;
        }
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into(), ptr_ty.into()], false);
        self.module
            .add_function("karac_map_insert", ty, Some(Linkage::External))
    }

    /// Emit a per-field-recursive clone fn for an n-tuple. Mirrors the
    /// tuple Hash/Eq/Display pattern — recursive per-field calls into the
    /// per-field clone fn via struct GEP.
    fn emit_tuple_clone_fn(&mut self, elems: &[TypeExpr]) -> FunctionValue<'ctx> {
        let elems_owned: Vec<TypeExpr> = elems.to_vec();
        let parts: Vec<String> = elems_owned.iter().map(Self::display_mangle_te).collect();
        let type_name = format!("tuple_{}", parts.join("_"));
        if let Some(&f) = self.clone_fn_cache.get(&type_name) {
            return f;
        }
        let fn_name = format!("karac_clone_{type_name}");
        if let Some(f) = self.module.get_function(&fn_name) {
            self.clone_fn_cache.insert(type_name, f);
            return f;
        }

        let child_fns: Vec<FunctionValue<'ctx>> = elems_owned
            .iter()
            .map(|e| self.emit_clone_fn_for_type_expr(e))
            .collect();
        let field_tys: Vec<BasicTypeEnum<'ctx>> = elems_owned
            .iter()
            .map(|e| self.llvm_type_for_type_expr(e))
            .collect();
        let tuple_ty = self.context.struct_type(&field_tys, false);

        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let saved_bb = self.builder.get_insert_block();
        let clone_fn_ty = self
            .context
            .void_type()
            .fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let clone_fn = self
            .module
            .add_function(&fn_name, clone_fn_ty, Some(Linkage::Internal));
        self.clone_fn_cache.insert(type_name, clone_fn);

        let entry_bb = self.context.append_basic_block(clone_fn, "entry");
        self.builder.position_at_end(entry_bb);
        let src = clone_fn.get_nth_param(0).unwrap().into_pointer_value();
        let dst = clone_fn.get_nth_param(1).unwrap().into_pointer_value();
        for (i, child_fn) in child_fns.iter().enumerate() {
            let src_field = self
                .builder
                .build_struct_gep(tuple_ty, src, i as u32, &format!("t.f{i}.s"))
                .unwrap();
            let dst_field = self
                .builder
                .build_struct_gep(tuple_ty, dst, i as u32, &format!("t.f{i}.d"))
                .unwrap();
            self.builder
                .build_call(*child_fn, &[src_field.into(), dst_field.into()], "")
                .unwrap();
        }
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }
        clone_fn
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
        if let ExprKind::Identifier(name) = &object.kind {
            if self.vec_elem_types.contains_key(name.as_str()) {
                return self.compile_vec_index(name, index);
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
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.vec_elem_type_for_var(name);

        let vec_ptr = self
            .get_data_ptr(name)
            .ok_or_else(|| format!("Undefined Vec variable '{}' in index expression", name))?;
        let idx_val = self.compile_expr(index)?.into_int_value();

        let len_ptr = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 1, "v.len.ptr")
            .unwrap();
        let len = self
            .builder
            .build_load(i64_t, len_ptr, "v.len")
            .unwrap()
            .into_int_value();
        let data_pp = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 0, "v.data.ptr")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "v.data")
            .unwrap()
            .into_pointer_value();

        let fn_val = self.current_fn.unwrap();
        let oob_bb = self.context.append_basic_block(fn_val, "vidx.oob");
        let ok_bb = self.context.append_basic_block(fn_val, "vidx.ok");
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

    fn compile_slice_index_store(
        &mut self,
        var_name: &str,
        index: &Expr,
        val: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();
        let elem_ty = *self.slice_elem_types.get(var_name).unwrap();
        let slice_ptr = self.get_data_ptr(var_name).unwrap();
        let idx_val = self.compile_expr(index)?.into_int_value();

        let data_pp = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 0, "s.st.data.pp")
            .unwrap();
        let len_p = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 1, "s.st.len.p")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "s.st.data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_p, "s.st.len")
            .unwrap()
            .into_int_value();

        let fn_val = self.current_fn.unwrap();
        let oob_bb = self.context.append_basic_block(fn_val, "s.st.oob");
        let ok_bb = self.context.append_basic_block(fn_val, "s.st.ok");
        let cmp = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGE, idx_val, len, "bounds")
            .unwrap();
        self.builder
            .build_conditional_branch(cmp, oob_bb, ok_bb)
            .unwrap();
        self.builder.position_at_end(oob_bb);
        self.emit_panic("slice index out of bounds");
        self.builder.build_unreachable().unwrap();

        self.builder.position_at_end(ok_bb);
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
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();
        let elem_ty = *self.slice_elem_types.get(var_name).unwrap();
        let slice_ptr = self.get_data_ptr(var_name).unwrap();
        let idx_val = self.compile_expr(index)?.into_int_value();

        let data_pp = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 0, "s.data.pp")
            .unwrap();
        let len_p = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 1, "s.len.p")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "s.data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_p, "s.len")
            .unwrap()
            .into_int_value();

        let fn_val = self.current_fn.unwrap();
        let oob_bb = self.context.append_basic_block(fn_val, "sidx.oob");
        let ok_bb = self.context.append_basic_block(fn_val, "sidx.ok");
        let cmp = self
            .builder
            .build_int_compare(inkwell::IntPredicate::UGE, idx_val, len, "bounds")
            .unwrap();
        self.builder
            .build_conditional_branch(cmp, oob_bb, ok_bb)
            .unwrap();
        self.builder.position_at_end(oob_bb);
        self.emit_panic("slice index out of bounds");
        self.builder.build_unreachable().unwrap();

        self.builder.position_at_end(ok_bb);
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
            if let Some(&inner_ty) = self.ref_params.get(var_name) {
                if let BasicTypeEnum::StructType(struct_ty) = inner_ty {
                    if let Some(idx) = self.field_index_for(object, field) {
                        if let Some(ptr) = self.get_data_ptr(var_name) {
                            let field_ptr = self
                                .builder
                                .build_struct_gep(
                                    struct_ty,
                                    ptr,
                                    idx,
                                    &format!("ref_{}_ptr", field),
                                )
                                .unwrap();
                            self.builder.build_store(field_ptr, new_val).unwrap();
                            return Ok(());
                        }
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
        // Try to resolve by variable name → type name → field registry.
        // `self` inside impl methods is stored as a regular local named "self"
        // with its type recorded in `var_type_names`.
        let var_name = match &object.kind {
            ExprKind::Identifier(n) => Some(n.as_str()),
            ExprKind::SelfValue => Some("self"),
            _ => None,
        };
        if let Some(var_name) = var_name {
            if let Some(type_name) = self.var_type_names.get(var_name) {
                if let Some(names) = self.struct_field_names.get(type_name.as_str()) {
                    if let Some(idx) = names.iter().position(|n| n == field) {
                        return Some(idx as u32);
                    }
                }
            }
        }
        // Fall back: numeric index for tuple fields like `.0`, `.1`
        field.parse::<u32>().ok()
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

        let name = match &callee.kind {
            ExprKind::Identifier(n) => n.clone(),
            _ => return Ok(self.context.i64_type().const_int(0, false).into()),
        };

        if name == "println" || name == "print" {
            return self.compile_print(&name, args);
        }

        // Check if this is an enum variant constructor (tuple variant)
        if let Some(enum_val) = self.try_compile_enum_variant(&name, args)? {
            return Ok(enum_val);
        }

        // Check if this is a call to a generic function (monomorphize on demand)
        if self.generic_fns.contains_key(&name) {
            return self.compile_generic_call(&name, args);
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
        // Find which enum this variant belongs to.
        let enum_name = {
            let mut found = None;
            for (en, layout) in &self.enum_layouts {
                if layout.tags.contains_key(name) {
                    found = Some(en.clone());
                    break;
                }
            }
            found
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
            // Payload words at heap indices 2, 3, …
            for (i, arg) in args.iter().enumerate() {
                let val = self.compile_expr(&arg.value)?;
                let word = self.coerce_to_i64(val)?;
                let word_ptr = self
                    .builder
                    .build_struct_gep(info.heap_type, ptr, (i + 2) as u32, "sh_word")
                    .unwrap();
                self.builder.build_store(word_ptr, word).unwrap();
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

        // Store payload args as fields 1, 2, …
        for (i, arg) in args.iter().enumerate() {
            let val = self.compile_expr(&arg.value)?;
            // Coerce to i64 for storage in the uniform payload
            let word = self.coerce_to_i64(val)?;
            agg = self
                .builder
                .build_insert_value(agg, word, (i + 1) as u32, "word")
                .unwrap()
                .into_struct_value();
        }

        Ok(Some(agg.into()))
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
        let mut found = None;
        for (enum_name, layout) in &self.enum_layouts {
            if let Some(&tag) = layout.tags.get(name) {
                if layout.field_counts.get(name).copied().unwrap_or(0) == 0 {
                    let i64_t = self.context.i64_type();

                    // Shared enum: heap-allocate.
                    if let Some(info) = self.shared_types.get(enum_name) {
                        let ptr = self.emit_rc_alloc(info.heap_type);
                        // Tag at heap index 1.
                        let tag_ptr = self
                            .builder
                            .build_struct_gep(info.heap_type, ptr, 1, "sh_tag")
                            .unwrap();
                        self.builder
                            .build_store(tag_ptr, i64_t.const_int(tag, false))
                            .unwrap();
                        found = Some(ptr.into());
                        break;
                    }

                    let mut agg = layout.llvm_type.get_undef();
                    // Only need to set the tag; remaining fields are undef/zeroed.
                    agg = self
                        .builder
                        .build_insert_value(agg, i64_t.const_int(tag, false), 0, "tag")
                        .unwrap()
                        .into_struct_value();
                    found = Some(agg.into());
                    break;
                }
            }
        }
        found
    }

    // ── For loop ─────────────────────────────────────────────────

    /// Compile `for pattern in iterable { body }`.
    /// Currently supports ranges (`start..end`, `start..=end`) and array literals.
    fn compile_for(
        &mut self,
        pattern: &Pattern,
        iterable: &Expr,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match &iterable.kind {
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => self.compile_for_range(pattern, start, end, *inclusive, body),
            ExprKind::ArrayLiteral(elems) => {
                // Compile each element eagerly and iterate by index
                let elems: Vec<BasicValueEnum<'ctx>> = elems
                    .iter()
                    .map(|e| self.compile_expr(e))
                    .collect::<Result<_, _>>()?;
                self.compile_for_array_values(pattern, &elems, body)
            }
            ExprKind::Identifier(name) => {
                if let Some(slot) = self.variables.get(name.as_str()).copied() {
                    // Owned array
                    if let BasicTypeEnum::ArrayType(at) = slot.ty {
                        return self.compile_for_array_var(pattern, slot.ptr, at, body);
                    }
                    // Ref array
                    if let Some(&BasicTypeEnum::ArrayType(at)) = self.ref_params.get(name.as_str())
                    {
                        let arr_ptr = self.get_data_ptr(name).unwrap();
                        return self.compile_for_array_var(pattern, arr_ptr, at, body);
                    }
                    // Vec/String iteration (owned or ref)
                    if self.vec_elem_types.contains_key(name.as_str()) {
                        return self.compile_for_vec_var(pattern, name, body);
                    }
                    // Slice iteration: `{ptr, len}` struct alloca.
                    if self.slice_elem_types.contains_key(name.as_str()) {
                        return self.compile_for_slice_var(pattern, name, body);
                    }
                    // Map iteration: for (k, v) in map { }
                    if self.map_key_types.contains_key(name.as_str()) {
                        return self.compile_for_map_var(pattern, name, body);
                    }
                    // Set iteration: for x in set { }
                    if self.set_elem_types.contains_key(name.as_str()) {
                        return self.compile_for_set_var(pattern, name, body);
                    }
                }
                Ok(self.context.i64_type().const_int(0, false).into())
            }
            _ => {
                // Unknown iterable — skip body, return unit
                Ok(self.context.i64_type().const_int(0, false).into())
            }
        }
    }

    fn compile_for_range(
        &mut self,
        pattern: &Pattern,
        start: &Option<Box<Expr>>,
        end: &Option<Box<Expr>>,
        inclusive: bool,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();

        let start_val = if let Some(s) = start {
            self.compile_expr(s)?.into_int_value()
        } else {
            i64_t.const_int(0, false)
        };
        let end_val = if let Some(e) = end {
            self.compile_expr(e)?.into_int_value()
        } else {
            return Err("for-range loop requires an end bound".to_string());
        };

        // Allocate loop counter
        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder.build_store(counter, start_val).unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // Condition: i < end (or i <= end for inclusive)
        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let pred = if inclusive {
            IntPredicate::SLE
        } else {
            IntPredicate::SLT
        };
        let cond = self
            .builder
            .build_int_compare(pred, cur, end_val, "for.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        // Body: bind pattern, compile block
        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap();
        self.bind_pattern(pattern, cur)?;
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        // Increment
        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let one = i64_t.const_int(1, false);
        let next = self.builder.build_int_add(cur, one, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_for_slice_var(
        &mut self,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let slice_ty = self.slice_struct_type();
        let elem_ty = *self.slice_elem_types.get(var_name).unwrap();
        let slice_ptr = self.get_data_ptr(var_name).unwrap();

        let data_pp = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 0, "for.s.data.pp")
            .unwrap();
        let len_p = self
            .builder
            .build_struct_gep(slice_ty, slice_ptr, 1, "for.s.len.p")
            .unwrap();
        let data = self
            .builder
            .build_load(ptr_ty, data_pp, "for.s.data")
            .unwrap()
            .into_pointer_value();
        let len = self
            .builder
            .build_load(i64_t, len_p, "for.s.len")
            .unwrap()
            .into_int_value();

        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_int(0, false))
            .unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.s.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.s.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.s.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.s.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, cur, len, "for.s.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[cur], "for.s.elem.ptr")
                .unwrap()
        };
        let elem_val = self
            .builder
            .build_load(elem_ty, elem_ptr, "for.s.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        self.register_for_loop_bindings(pattern, var_name);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let one = i64_t.const_int(1, false);
        let next = self.builder.build_int_add(cur, one, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_for_vec_var(
        &mut self,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let vec_ty = self.vec_struct_type();
        let elem_ty = self.vec_elem_type_for_var(var_name);
        let vec_ptr = self.get_data_ptr(var_name).unwrap();

        // Load len and data pointer.
        let len_ptr = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 1, "for.v.len.ptr")
            .unwrap();
        let data_ptr_ptr = self
            .builder
            .build_struct_gep(vec_ty, vec_ptr, 0, "for.v.data.ptr")
            .unwrap();
        let len = self
            .builder
            .build_load(i64_t, len_ptr, "for.v.len")
            .unwrap()
            .into_int_value();
        let data = self
            .builder
            .build_load(ptr_ty, data_ptr_ptr, "for.v.data")
            .unwrap()
            .into_pointer_value();

        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_int(0, false))
            .unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // Condition: i < len
        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, cur, len, "for.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        // Body: load data[i], bind, execute
        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let elem_ptr = unsafe {
            self.builder
                .build_gep(elem_ty, data, &[cur], "for.v.elem.ptr")
                .unwrap()
        };
        let elem_val = self
            .builder
            .build_load(elem_ty, elem_ptr, "for.v.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        self.register_for_loop_bindings(pattern, var_name);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        // Increment
        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let one = i64_t.const_int(1, false);
        let next = self.builder.build_int_add(cur, one, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    /// Compile `for <pattern> in <map_var> { body }`.
    ///
    /// Uses the `karac_map_iter_*` runtime functions:
    /// - `karac_map_iter_new` creates the iterator before the loop.
    /// - `karac_map_iter_next` drives the loop; returns `false` when exhausted.
    /// - `karac_map_iter_free` runs unconditionally in the exit block so it fires
    ///   on both normal exit and `break`.
    ///
    /// The `(K, V)` pair delivered to `bind_pattern` is a two-field struct so
    /// tuple patterns like `for (k, v) in m` work via the existing struct-extract
    /// path in `bind_pattern`.
    fn compile_for_map_var(
        &mut self,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

        let slot = self
            .variables
            .get(var_name)
            .copied()
            .ok_or_else(|| format!("unknown map variable '{var_name}'"))?;
        let map_handle = self
            .builder
            .build_load(ptr_ty, slot.ptr, "map.handle")
            .unwrap()
            .into_pointer_value();

        let key_ty = self
            .map_key_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());
        let val_ty = self
            .map_val_types
            .get(var_name)
            .copied()
            .unwrap_or(i64_t.into());

        // Create the iterator (opaque ptr, lives for the duration of the loop).
        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[map_handle.into()], "map.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        // Persistent allocas for out_key / out_val — overwritten each iteration.
        let out_key = self.create_entry_alloca(fn_val, "map.iter.key", key_ty);
        let out_val = self.create_entry_alloca(fn_val, "map.iter.val", val_ty);

        let loop_bb = self.context.append_basic_block(fn_val, "map.for.loop");
        let body_bb = self.context.append_basic_block(fn_val, "map.for.body");
        let exit_bb = self.context.append_basic_block(fn_val, "map.for.exit");

        self.builder.build_unconditional_branch(loop_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            continue_bb: loop_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // loop_bb: advance iterator; branch on result.
        self.builder.position_at_end(loop_bb);
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_ptr.into(), out_key.into(), out_val.into()],
                "map.iter.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, body_bb, exit_bb)
            .unwrap();

        // body_bb: load key/val, build {K,V} struct, bind pattern, compile body.
        self.builder.position_at_end(body_bb);
        let key_val = self.builder.build_load(key_ty, out_key, "map.k").unwrap();
        let val_val = self.builder.build_load(val_ty, out_val, "map.v").unwrap();
        let kv_ty = self.context.struct_type(&[key_ty, val_ty], false);
        let mut kv = kv_ty.get_undef();
        kv = self
            .builder
            .build_insert_value(kv, key_val, 0, "kv.k")
            .unwrap()
            .into_struct_value();
        kv = self
            .builder
            .build_insert_value(kv, val_val, 1, "kv.v")
            .unwrap()
            .into_struct_value();
        self.bind_pattern(pattern, kv.into())?;
        self.register_for_loop_bindings(pattern, var_name);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(loop_bb).unwrap();
        }

        self.loop_stack.pop();

        // exit_bb: free iterator — runs on both normal exhaustion and break.
        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_ptr.into()], "")
            .unwrap();

        Ok(i64_t.const_int(0, false).into())
    }

    /// Compile `for x in s { ... }` for a `Set[T]` variable. Mirror of
    /// `compile_for_map_var` — Set lowers to `Map[T, ()]` so the runtime
    /// iterator is the same; the value out-slot is sized 0 (a single
    /// shared `i8` alloca) and discarded since Set iteration produces only
    /// the element. The element pattern is bound directly (no `(k, v)`
    /// destructuring like Map's tuple-shaped iteration delivery).
    fn compile_for_set_var(
        &mut self,
        pattern: &Pattern,
        var_name: &str,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let i8_t = self.context.i8_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());

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

        let iter_ptr = self
            .builder
            .build_call(self.karac_map_iter_new_fn, &[set_handle.into()], "set.iter")
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();

        let out_elem = self.create_entry_alloca(fn_val, "set.iter.elem", elem_ty);
        // val_size = 0 in the runtime; the val out-slot is overwritten
        // with zero bytes per iteration so a single `i8` is sufficient.
        let dummy_val = self.create_entry_alloca(fn_val, "set.iter.dummy", i8_t.into());

        let loop_bb = self.context.append_basic_block(fn_val, "set.for.loop");
        let body_bb = self.context.append_basic_block(fn_val, "set.for.body");
        let exit_bb = self.context.append_basic_block(fn_val, "set.for.exit");

        self.builder.build_unconditional_branch(loop_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            continue_bb: loop_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        self.builder.position_at_end(loop_bb);
        let has_next = self
            .builder
            .build_call(
                self.karac_map_iter_next_fn,
                &[iter_ptr.into(), out_elem.into(), dummy_val.into()],
                "set.iter.next",
            )
            .unwrap()
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.builder
            .build_conditional_branch(has_next, body_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let elem_val = self
            .builder
            .build_load(elem_ty, out_elem, "set.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        // Re-derive collection side-tables for the bound element so
        // `for x in s.union(t) { x.len() }` etc. dispatch correctly when
        // the element type itself is a Vec/Slice/Map (currently a no-op
        // for scalar Set elements; cheap insurance for the future).
        if let PatternKind::Binding(elem_name) = &pattern.kind {
            if let Some(elem_te) = self.set_elem_type_exprs.get(var_name).cloned() {
                self.register_var_from_type_expr(elem_name, &elem_te);
            }
        }
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(loop_bb).unwrap();
        }

        self.loop_stack.pop();

        self.builder.position_at_end(exit_bb);
        self.builder
            .build_call(self.karac_map_iter_free_fn, &[iter_ptr.into()], "")
            .unwrap();

        Ok(i64_t.const_int(0, false).into())
    }

    fn compile_for_array_var(
        &mut self,
        pattern: &Pattern,
        arr_ptr: PointerValue<'ctx>,
        arr_ty: inkwell::types::ArrayType<'ctx>,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let i64_t = self.context.i64_type();
        let len = arr_ty.len() as u64;
        let elem_ty = arr_ty.get_element_type();

        let counter = self.create_entry_alloca(fn_val, "for.i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_int(0, false))
            .unwrap();

        let cond_bb = self.context.append_basic_block(fn_val, "for.cond");
        let body_bb = self.context.append_basic_block(fn_val, "for.body");
        let incr_bb = self.context.append_basic_block(fn_val, "for.incr");
        let exit_bb = self.context.append_basic_block(fn_val, "for.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            continue_bb: incr_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        // Condition: i < N
        self.builder.position_at_end(cond_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let end_val = i64_t.const_int(len, false);
        let cond = self
            .builder
            .build_int_compare(IntPredicate::ULT, cur, end_val, "for.cond")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, exit_bb)
            .unwrap();

        // Body: load arr[i], bind to pattern, compile block
        self.builder.position_at_end(body_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let zero = i64_t.const_int(0, false);
        let elem_ptr = unsafe {
            self.builder
                .build_gep(
                    BasicTypeEnum::ArrayType(arr_ty),
                    arr_ptr,
                    &[zero, cur],
                    "for.elem.ptr",
                )
                .unwrap()
        };
        let elem_val = self
            .builder
            .build_load(elem_ty, elem_ptr, "for.elem")
            .unwrap();
        self.bind_pattern(pattern, elem_val)?;
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(incr_bb).unwrap();
        }

        // Increment
        self.builder.position_at_end(incr_bb);
        let cur = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(i64_t.into(), counter, "i")
            .unwrap()
            .into_int_value();
        let one = i64_t.const_int(1, false);
        let next = self.builder.build_int_add(cur, one, "incr").unwrap();
        self.builder.build_store(counter, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_for_array_values(
        &mut self,
        pattern: &Pattern,
        elems: &[BasicValueEnum<'ctx>],
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        for &elem in elems {
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_some()
            {
                break;
            }
            self.bind_pattern(pattern, elem)?;
            self.compile_block(body)?;
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    // ── IfLet ────────────────────────────────────────────────────

    fn compile_if_let(
        &mut self,
        pattern: &Pattern,
        value: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let val = self.compile_expr(value)?;
        let cond = self.compile_pattern_condition(pattern, val)?;
        // Reuse if-else codegen
        let fn_val = self.current_fn.unwrap();
        let then_bb = self.context.append_basic_block(fn_val, "iflet.then");
        let else_bb = self.context.append_basic_block(fn_val, "iflet.else");
        let merge_bb = self.context.append_basic_block(fn_val, "iflet.merge");

        self.builder
            .build_conditional_branch(cond.into_int_value(), then_bb, else_bb)
            .unwrap();

        self.builder.position_at_end(then_bb);
        self.bind_pattern_values(pattern, val)?;
        let then_val = self.compile_block(then_block)?;
        let then_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let then_end = self.builder.get_insert_block().unwrap();
        if !then_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(else_bb);
        let else_val = if let Some(eb) = else_branch {
            match &eb.kind {
                ExprKind::Block(blk) => self.compile_block(blk)?,
                _ => Some(self.compile_expr(eb)?),
            }
        } else {
            None
        };
        let else_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let else_end = self.builder.get_insert_block().unwrap();
        if !else_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(merge_bb);
        if let (Some(tv), Some(ev)) = (&then_val, &else_val) {
            if !then_terminated && !else_terminated && tv.get_type() == ev.get_type() {
                let phi = self.builder.build_phi(tv.get_type(), "ifletval").unwrap();
                phi.add_incoming(&[(tv, then_end), (ev, else_end)]);
                return Ok(phi.as_basic_value());
            }
        }
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_print(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let zero = self.context.i64_type().const_int(0, false);
        if args.is_empty() {
            let fmt = self.builder.build_global_string_ptr("\n", "nl").unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[BasicMetadataValueEnum::from(fmt.as_pointer_value())],
                    "printf",
                )
                .unwrap();
            return Ok(zero.into());
        }

        let nl = if name == "println" { "\n" } else { "" };

        // Collection dispatch: when the print arg is a bare identifier that
        // we've registered as a Vec or Map variable, emit a call to the
        // per-type Display fn against the variable's alloca. This is the
        // primary path for `println(v)` on collections; it produces the same
        // formatted output the interpreter prints. Bare Vec/Map values appear
        // as struct/pointer values in the legacy `is_struct_value` /
        // `is_pointer_value` arms below — that path is wrong for collections
        // (Vec gets treated as String; Map gets printed as a raw address) —
        // but those arms are still reachable for non-identifier expressions
        // (function returns, fresh literals) where the source-level type is
        // not in the side-tables, so we leave them in place as fallbacks.
        if let ExprKind::Identifier(var_name) = &args[0].value.kind {
            // Vec[T]: side-table both `vec_elem_types` and `var_elem_type_exprs`
            // are set (the latter is what distinguishes a Vec variable from a
            // String variable, which only sets `vec_elem_types`).
            if self.vec_elem_types.contains_key(var_name)
                && self.var_elem_type_exprs.contains_key(var_name)
            {
                let elem_te = self.var_elem_type_exprs[var_name].clone();
                let slot = self
                    .variables
                    .get(var_name)
                    .copied()
                    .ok_or_else(|| format!("compile_print: '{var_name}' not bound"))?;
                let display_fn = self.emit_vec_display_fn_te(&elem_te);
                self.builder
                    .build_call(display_fn, &[slot.ptr.into()], "vd")
                    .unwrap();
                if !nl.is_empty() {
                    let nl_str = self.builder.build_global_string_ptr("\n", "vd.nl").unwrap();
                    self.builder
                        .build_call(self.printf_fn, &[nl_str.as_pointer_value().into()], "p")
                        .unwrap();
                }
                return Ok(zero.into());
            }
            // Map[K, V]: side-tables hold both K and V `TypeExpr`s.
            if self.map_key_type_exprs.contains_key(var_name)
                && self.var_elem_type_exprs.contains_key(var_name)
            {
                let k_te = self.map_key_type_exprs[var_name].clone();
                let v_te = self.var_elem_type_exprs[var_name].clone();
                let slot = self
                    .variables
                    .get(var_name)
                    .copied()
                    .ok_or_else(|| format!("compile_print: '{var_name}' not bound"))?;
                let display_fn = self.emit_map_display_fn(&k_te, &v_te);
                self.builder
                    .build_call(display_fn, &[slot.ptr.into()], "md")
                    .unwrap();
                if !nl.is_empty() {
                    let nl_str = self.builder.build_global_string_ptr("\n", "md.nl").unwrap();
                    self.builder
                        .build_call(self.printf_fn, &[nl_str.as_pointer_value().into()], "p")
                        .unwrap();
                }
                return Ok(zero.into());
            }
            // Set[T]: side-table holds the element `TypeExpr`.
            if self.set_elem_type_exprs.contains_key(var_name) {
                let elem_te = self.set_elem_type_exprs[var_name].clone();
                let slot = self
                    .variables
                    .get(var_name)
                    .copied()
                    .ok_or_else(|| format!("compile_print: '{var_name}' not bound"))?;
                let display_fn = self.emit_set_display_fn(&elem_te);
                self.builder
                    .build_call(display_fn, &[slot.ptr.into()], "sd")
                    .unwrap();
                if !nl.is_empty() {
                    let nl_str = self.builder.build_global_string_ptr("\n", "sd.nl").unwrap();
                    self.builder
                        .build_call(self.printf_fn, &[nl_str.as_pointer_value().into()], "p")
                        .unwrap();
                }
                return Ok(zero.into());
            }
        }

        let val = self.compile_expr(&args[0].value)?;

        if val.is_int_value() {
            let bits = val.into_int_value().get_type().get_bit_width();
            if bits == 1 {
                let true_s = self
                    .builder
                    .build_global_string_ptr(&format!("true{nl}"), "ts")
                    .unwrap();
                let false_s = self
                    .builder
                    .build_global_string_ptr(&format!("false{nl}"), "fs")
                    .unwrap();
                let sel = self
                    .builder
                    .build_select(
                        val.into_int_value(),
                        true_s.as_pointer_value(),
                        false_s.as_pointer_value(),
                        "bstr",
                    )
                    .unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[BasicMetadataValueEnum::from(sel.into_pointer_value())],
                        "printf",
                    )
                    .unwrap();
            } else {
                let fmt = self
                    .builder
                    .build_global_string_ptr(&format!("%lld{nl}"), "fi")
                    .unwrap();
                self.builder
                    .build_call(
                        self.printf_fn,
                        &[
                            BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                            BasicMetadataValueEnum::from(val.into_int_value()),
                        ],
                        "printf",
                    )
                    .unwrap();
            }
        } else if val.is_struct_value() {
            // String struct { ptr, i64, i64 } — extract the data pointer for printf %s.
            let str_ptr = self
                .builder
                .build_extract_value(val.into_struct_value(), 0, "str.ptr")
                .unwrap()
                .into_pointer_value();
            let fmt = self
                .builder
                .build_global_string_ptr(&format!("%s{nl}"), "fsp")
                .unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[
                        BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                        BasicMetadataValueEnum::from(str_ptr),
                    ],
                    "printf",
                )
                .unwrap();
        } else if val.is_pointer_value() {
            // Raw pointer (shared types, etc.) — pass directly to %s.
            let fmt = self
                .builder
                .build_global_string_ptr(&format!("%s{nl}"), "fsp")
                .unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[
                        BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                        BasicMetadataValueEnum::from(val.into_pointer_value()),
                    ],
                    "printf",
                )
                .unwrap();
        } else if val.is_float_value() {
            let fmt = self
                .builder
                .build_global_string_ptr(&format!("%g{nl}"), "ff")
                .unwrap();
            self.builder
                .build_call(
                    self.printf_fn,
                    &[
                        BasicMetadataValueEnum::from(fmt.as_pointer_value()),
                        BasicMetadataValueEnum::from(val.into_float_value()),
                    ],
                    "printf",
                )
                .unwrap();
        }
        Ok(zero.into())
    }

    // ── Control flow ──────────────────────────────────────────────

    fn compile_if(
        &mut self,
        condition: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let cond_val = self.compile_expr(condition)?.into_int_value();
        let fn_val = self.current_fn.unwrap();
        let then_bb = self.context.append_basic_block(fn_val, "then");
        let else_bb = self.context.append_basic_block(fn_val, "else");
        let merge_bb = self.context.append_basic_block(fn_val, "ifmerge");

        self.builder
            .build_conditional_branch(cond_val, then_bb, else_bb)
            .unwrap();

        self.builder.position_at_end(then_bb);
        let then_val = self.compile_block(then_block)?;
        let then_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let then_end_bb = self.builder.get_insert_block().unwrap();
        if !then_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(else_bb);
        let else_val = if let Some(else_expr) = else_branch {
            match &else_expr.kind {
                ExprKind::Block(blk) => self.compile_block(blk)?,
                ExprKind::If {
                    condition: c,
                    then_block: tb,
                    else_branch: eb,
                } => {
                    let v = self.compile_if(c, tb, eb.as_deref())?;
                    Some(v)
                }
                _ => {
                    let v = self.compile_expr(else_expr)?;
                    Some(v)
                }
            }
        } else {
            None
        };
        let else_terminated = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_some();
        let else_end_bb = self.builder.get_insert_block().unwrap();
        if !else_terminated {
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }

        self.builder.position_at_end(merge_bb);

        if let (Some(tv), Some(ev)) = (&then_val, &else_val) {
            if !then_terminated && !else_terminated && tv.get_type() == ev.get_type() {
                let phi = self.builder.build_phi(tv.get_type(), "ifval").unwrap();
                phi.add_incoming(&[(tv, then_end_bb), (ev, else_end_bb)]);
                return Ok(phi.as_basic_value());
            }
        }

        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_while(
        &mut self,
        condition: &Expr,
        body: &Block,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let cond_bb = self.context.append_basic_block(fn_val, "while.cond");
        let body_bb = self.context.append_basic_block(fn_val, "while.body");
        let exit_bb = self.context.append_basic_block(fn_val, "while.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        self.loop_stack.push(LoopFrame {
            continue_bb: cond_bb,
            break_bb: exit_bb,
            result_slot: None,
        });

        self.builder.position_at_end(cond_bb);
        let cond_val = self.compile_expr(condition)?.into_int_value();
        self.builder
            .build_conditional_branch(cond_val, body_bb, exit_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(cond_bb).unwrap();
        }

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        Ok(self.context.i64_type().const_int(0, false).into())
    }

    fn compile_loop(&mut self, body: &Block) -> Result<BasicValueEnum<'ctx>, String> {
        let fn_val = self.current_fn.unwrap();
        let loop_bb = self.context.append_basic_block(fn_val, "loop.body");
        let exit_bb = self.context.append_basic_block(fn_val, "loop.exit");

        // Allocate a slot for `break value` (i64 by default; refined if used)
        let result_slot =
            self.create_entry_alloca(fn_val, "loop.result", self.context.i64_type().into());

        self.loop_stack.push(LoopFrame {
            continue_bb: loop_bb,
            break_bb: exit_bb,
            result_slot: Some(result_slot),
        });

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        self.compile_block(body)?;
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_unconditional_branch(loop_bb).unwrap();
        }

        self.loop_stack.pop();
        self.builder.position_at_end(exit_bb);
        // Load result (may be zero if no break-with-value was hit)
        let result = self
            .builder
            .build_load::<BasicTypeEnum<'ctx>>(
                self.context.i64_type().into(),
                result_slot,
                "loop.val",
            )
            .unwrap();
        Ok(result)
    }

    fn compile_break(&mut self, value: Option<&Expr>) -> Result<BasicValueEnum<'ctx>, String> {
        let zero = self.context.i64_type().const_int(0, false);
        if let Some(frame) = self.loop_stack.last().copied() {
            if let Some(slot) = frame.result_slot {
                let val = if let Some(v) = value {
                    self.compile_expr(v)?
                } else {
                    zero.into()
                };
                // Store break value (only works when types match i64)
                if val.is_int_value() {
                    self.builder.build_store(slot, val).unwrap();
                }
            }
            self.builder
                .build_unconditional_branch(frame.break_bb)
                .unwrap();
        }
        Ok(zero.into())
    }

    fn compile_continue(&mut self) -> Result<BasicValueEnum<'ctx>, String> {
        let zero = self.context.i64_type().const_int(0, false);
        if let Some(frame) = self.loop_stack.last().copied() {
            self.builder
                .build_unconditional_branch(frame.continue_bb)
                .unwrap();
        }
        Ok(zero.into())
    }

    // ── Match ─────────────────────────────────────────────────────

    fn compile_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let scrut = self.compile_expr(scrutinee)?;
        let fn_val = self.current_fn.unwrap();
        let merge_bb = self.context.append_basic_block(fn_val, "match.merge");

        let mut arm_results: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();

        let mut next_bb = self.context.append_basic_block(fn_val, "match.arm0");
        self.builder.build_unconditional_branch(next_bb).unwrap();

        for (i, arm) in arms.iter().enumerate() {
            let arm_bb = next_bb;
            // Always create a fresh fail_bb — never reuse merge_bb directly.
            // If the last pattern condition is false (non-exhaustive match or
            // missed case), we emit `unreachable` to satisfy LLVM's requirement
            // that every basic block has a terminator and every phi predecessor
            // is accounted for.
            let is_last = i + 1 == arms.len();
            let fail_bb = if !is_last {
                self.context
                    .append_basic_block(fn_val, &format!("match.arm{}", i + 1))
            } else {
                self.context.append_basic_block(fn_val, "match.nofall")
            };
            next_bb = fail_bb;

            self.builder.position_at_end(arm_bb);

            // Generate pattern condition
            let cond = self.compile_pattern_condition(&arm.pattern, scrut)?;

            let body_bb = self
                .context
                .append_basic_block(fn_val, &format!("match.body{}", i));

            self.builder
                .build_conditional_branch(cond.into_int_value(), body_bb, fail_bb)
                .unwrap();

            self.builder.position_at_end(body_bb);

            // Bind pattern variables
            self.bind_pattern_values(&arm.pattern, scrut)?;

            let arm_val = self.compile_expr(&arm.body)?;
            let arm_end = self.builder.get_insert_block().unwrap();
            if arm_end.get_terminator().is_none() {
                arm_results.push((arm_val, arm_end));
                self.builder.build_unconditional_branch(merge_bb).unwrap();
            }
        }

        // Terminate the last fail_bb (match.nofall) — exhaustive matches never
        // reach here; emit `unreachable` so LLVM doesn't require a phi entry.
        self.builder.position_at_end(next_bb);
        if next_bb.get_terminator().is_none() {
            self.builder.build_unreachable().unwrap();
        }

        self.builder.position_at_end(merge_bb);

        // Build phi if all arms produce a value of the same type
        if !arm_results.is_empty() {
            let first_ty = arm_results[0].0.get_type();
            if arm_results.iter().all(|(v, _)| v.get_type() == first_ty) {
                let phi = self.builder.build_phi(first_ty, "matchval").unwrap();
                for (val, bb) in &arm_results {
                    phi.add_incoming(&[(val, *bb)]);
                }
                return Ok(phi.as_basic_value());
            }
        }

        Ok(self.context.i64_type().const_int(0, false).into())
    }

    /// Returns an i1 (bool) value: 1 if the scrutinee matches the pattern.
    fn compile_pattern_condition(
        &mut self,
        pattern: &Pattern,
        scrut: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let tru = self.context.bool_type().const_int(1, false);
        match &pattern.kind {
            PatternKind::Wildcard => Ok(tru.into()),
            PatternKind::Binding(name) => {
                // Check if this binding name is actually a unit enum variant.
                // The parser produces Binding("Color.Red") or Binding("Red") for
                // unit variants in match arms; detect and compare tags.
                let variant_name = name.rsplit('.').next().unwrap_or(name);
                if let Some(tag) = self.enum_tag_for_variant(variant_name) {
                    let actual_tag = self.extract_enum_tag(scrut, variant_name)?;
                    let expected_tag = self.context.i64_type().const_int(tag, false);
                    return Ok(self
                        .builder
                        .build_int_compare(IntPredicate::EQ, actual_tag, expected_tag, "tag_eq")
                        .unwrap()
                        .into());
                }
                // Not a variant — true binding, always matches.
                Ok(tru.into())
            }
            PatternKind::Literal(lit) => {
                let lit_val = match lit {
                    LiteralPattern::Integer(n, sfx) => self.const_int_for_suffix(*n, *sfx).into(),
                    LiteralPattern::Bool(b) => self
                        .context
                        .bool_type()
                        .const_int(u64::from(*b), false)
                        .into(),
                    LiteralPattern::Float(f, sfx) => self.const_float_for_suffix(*f, *sfx).into(),
                    LiteralPattern::Char(c) => {
                        self.context.i32_type().const_int(*c as u64, false).into()
                    }
                    LiteralPattern::String(s) => self
                        .builder
                        .build_global_string_ptr(s, "spat")
                        .unwrap()
                        .as_pointer_value()
                        .into(),
                };
                self.compile_binop(&BinOp::Eq, scrut, lit_val)
            }
            PatternKind::Or(pats) => {
                let mut result: BasicValueEnum<'ctx> =
                    self.context.bool_type().const_int(0, false).into();
                for p in pats {
                    let cond = self.compile_pattern_condition(p, scrut)?;
                    result = self
                        .builder
                        .build_or(result.into_int_value(), cond.into_int_value(), "orcond")
                        .unwrap()
                        .into();
                }
                Ok(result)
            }
            // Tuple enum variant: check tag matches
            PatternKind::TupleVariant { path, .. } => {
                let variant_name = path.last().map(|s| s.as_str()).unwrap_or("");
                if let Some(tag) = self.enum_tag_for_variant(variant_name) {
                    let actual_tag = self.extract_enum_tag(scrut, variant_name)?;
                    let expected_tag = self.context.i64_type().const_int(tag, false);
                    return Ok(self
                        .builder
                        .build_int_compare(IntPredicate::EQ, actual_tag, expected_tag, "tag_eq")
                        .unwrap()
                        .into());
                }
                Ok(tru.into())
            }
            // Struct enum variant: check tag matches
            PatternKind::Struct { path, .. }
                if path.len() > 1
                    || self
                        .enum_tag_for_variant(path.last().map(|s| s.as_str()).unwrap_or(""))
                        .is_some() =>
            {
                let variant_name = path.last().map(|s| s.as_str()).unwrap_or("");
                if let Some(tag) = self.enum_tag_for_variant(variant_name) {
                    let actual_tag = self.extract_enum_tag(scrut, variant_name)?;
                    let expected_tag = self.context.i64_type().const_int(tag, false);
                    return Ok(self
                        .builder
                        .build_int_compare(IntPredicate::EQ, actual_tag, expected_tag, "tag_eq")
                        .unwrap()
                        .into());
                }
                Ok(tru.into())
            }
            // Plain struct pattern or anything else — always matches
            _ => Ok(tru.into()),
        }
    }

    /// Extract the tag integer from an enum scrutinee.
    /// Handles both shared enums (pointer — GEP to tag at index 1) and
    /// non-shared enums (struct value — extractvalue at index 0).
    fn extract_enum_tag(
        &self,
        scrut: BasicValueEnum<'ctx>,
        variant_name: &str,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        let i64_t = self.context.i64_type();
        // Check if this variant belongs to a shared enum.
        if let BasicValueEnum::PointerValue(ptr) = scrut {
            for (enum_name, layout) in &self.enum_layouts {
                if layout.tags.contains_key(variant_name) {
                    if let Some(info) = self.shared_types.get(enum_name) {
                        // Shared enum: tag is at heap index 1.
                        let tag_ptr = self
                            .builder
                            .build_struct_gep(info.heap_type, ptr, 1, "sh_tag_ptr")
                            .unwrap();
                        let tag = self
                            .builder
                            .build_load(i64_t, tag_ptr, "actual_tag")
                            .unwrap()
                            .into_int_value();
                        return Ok(tag);
                    }
                }
            }
        }
        // Non-shared enum: extractvalue at index 0.
        if let BasicValueEnum::StructValue(sv) = scrut {
            let tag = self
                .builder
                .build_extract_value(sv, 0, "actual_tag")
                .unwrap()
                .into_int_value();
            return Ok(tag);
        }
        Ok(i64_t.const_int(0, false))
    }

    /// Find the discriminant tag for a variant name across all registered enums.
    fn enum_tag_for_variant(&self, variant_name: &str) -> Option<u64> {
        for layout in self.enum_layouts.values() {
            if let Some(&tag) = layout.tags.get(variant_name) {
                return Some(tag);
            }
        }
        None
    }

    /// Find the LLVM struct type for the enum containing a given variant.
    #[allow(dead_code)]
    fn enum_type_for_variant(&self, variant_name: &str) -> Option<StructType<'ctx>> {
        for layout in self.enum_layouts.values() {
            if layout.tags.contains_key(variant_name) {
                return Some(layout.llvm_type);
            }
        }
        None
    }

    /// Bind variable names from a pattern against the scrutinee value.
    fn bind_pattern_values(
        &mut self,
        pattern: &Pattern,
        scrut: BasicValueEnum<'ctx>,
    ) -> Result<(), String> {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                // Skip binding if this is a unit enum variant pattern.
                let variant_name = name.rsplit('.').next().unwrap_or(name);
                if self.enum_tag_for_variant(variant_name).is_some() {
                    return Ok(());
                }
                let fn_val = self.current_fn.unwrap();

                // Struct-payload reconstruction: when the typechecker
                // recorded a struct surface type for this binding, the
                // enum-payload codegen has handed us the i64 word that
                // held the (single-field) struct. Wrap it back into the
                // struct shape so subsequent `.field` access dispatches
                // through the right LLVM struct type. Limited to the
                // single-i64-field case for now — wider error wrappers
                // can't survive the i64-payload-word lowering anyway, so
                // there's nothing to reconstitute beyond this shape.
                let key = (pattern.span.offset, pattern.span.length);
                if let Some(type_name) = self.pattern_binding_types.get(&key).cloned() {
                    if let Some(&st) = self.struct_types.get(&type_name) {
                        if let BasicValueEnum::IntValue(iv) = scrut {
                            if st.count_fields() == 1
                                && matches!(
                                    st.get_field_type_at_index(0),
                                    Some(BasicTypeEnum::IntType(t))
                                        if t.get_bit_width() == iv.get_type().get_bit_width()
                                )
                            {
                                let undef = st.get_undef();
                                let struct_val = self
                                    .builder
                                    .build_insert_value(undef, iv, 0, "pat.struct")
                                    .unwrap()
                                    .into_struct_value();
                                let alloca = self.create_entry_alloca(fn_val, name, st.into());
                                self.builder.build_store(alloca, struct_val).unwrap();
                                self.variables.insert(
                                    name.clone(),
                                    VarSlot {
                                        ptr: alloca,
                                        ty: st.into(),
                                    },
                                );
                                self.var_type_names.insert(name.clone(), type_name);
                                return Ok(());
                            }
                        }
                    }
                }

                let alloca = self.create_entry_alloca(fn_val, name, scrut.get_type());
                self.builder.build_store(alloca, scrut).unwrap();
                self.variables.insert(
                    name.clone(),
                    VarSlot {
                        ptr: alloca,
                        ty: scrut.get_type(),
                    },
                );
                Ok(())
            }
            PatternKind::TupleVariant { path, patterns } => {
                let variant_name = path.last().map(|s| s.as_str()).unwrap_or("");
                // Shared enum: extract payload via GEP (words at heap index 2+).
                if let BasicValueEnum::PointerValue(ptr) = scrut {
                    for (enum_name, layout) in &self.enum_layouts.clone() {
                        if layout.tags.contains_key(variant_name) {
                            if let Some(info) = self.shared_types.get(enum_name).cloned() {
                                for (i, sub_pat) in patterns.iter().enumerate() {
                                    let word_ptr = self
                                        .builder
                                        .build_struct_gep(
                                            info.heap_type,
                                            ptr,
                                            (i + 2) as u32,
                                            "sh_payload",
                                        )
                                        .unwrap();
                                    let word = self
                                        .builder
                                        .build_load(self.context.i64_type(), word_ptr, "payload")
                                        .unwrap();
                                    self.bind_pattern_values(sub_pat, word)?;
                                }
                                return Ok(());
                            }
                        }
                    }
                }
                // Non-shared enum: extract payload words from the struct value.
                if let BasicValueEnum::StructValue(sv) = scrut {
                    for (i, sub_pat) in patterns.iter().enumerate() {
                        let word = self
                            .builder
                            .build_extract_value(sv, (i + 1) as u32, "payload")
                            .unwrap();
                        self.bind_pattern_values(sub_pat, word)?;
                    }
                }
                Ok(())
            }
            PatternKind::Or(pats) => {
                // Bind variables from first sub-pattern (all alternatives must bind same names)
                if let Some(first) = pats.first() {
                    self.bind_pattern_values(first, scrut)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    // ── Par block compilation ─────────────────────────────────────

    /// Lower a `par {}` block into a call into the runtime.
    ///
    /// Each top-level statement becomes a branch function with signature
    /// `void(*)(ptr ctx)`. A single shared environment struct is allocated on
    /// the caller's stack (the runtime joins all branches before returning,
    /// so the stack frame is live throughout). At the call site we build an
    /// array of `KaracBranch { func, ctx }` entries and hand it to
    /// `karac_par_run`.
    ///
    /// First cut: no cancellation, no return values, no error propagation.
    /// The par block always evaluates to `unit` (i64 0 in this codegen's
    /// convention). Cancellation and error propagation land in later phases
    /// alongside the work-stealing scheduler.
    #[allow(clippy::result_large_err)]
    fn compile_par_block(&mut self, block: &Block) -> Result<BasicValueEnum<'ctx>, String> {
        self.emit_par_run(&block.stmts, &block.span)
    }

    /// Lower a list of statements to a `karac_par_run` runtime dispatch.
    ///
    /// Shared between the explicit-`par`-block lowering (`compile_par_block`)
    /// and slice 2's auto-par lowering on inferred parallel groups
    /// (`compile_function_body`). Both call sites pass a slice of stmts that
    /// should run concurrently and a span used for capture-set scoping —
    /// for the explicit path the span is the par-block's own span; for the
    /// inferred path it is best-effort the function-body span (per-stmt
    /// span resolution is slice 3's concern). Trivial fan-outs (zero or
    /// one statement) compile sequentially without invoking the runtime.
    #[allow(clippy::result_large_err)]
    fn emit_par_run(
        &mut self,
        stmts: &[Stmt],
        span: &Span,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let unit = self.context.i64_type().const_int(0, false).into();

        // Zero statements: nothing to do. Single statement: no parallelism
        // needed — compile in place to avoid the runtime call overhead.
        if stmts.is_empty() {
            return Ok(unit);
        }
        if stmts.len() == 1 {
            self.compile_stmt(&stmts[0])?;
            return Ok(unit);
        }

        // 1. Collect the union of captured variables across all branch statements.
        //    Intersection with self.variables filters out non-locals (top-level
        //    functions, struct names, etc.) that refs_in_block doesn't distinguish.
        let mut refs: HashSet<String> = HashSet::new();
        let mut inner_defs: HashSet<String> = HashSet::new();
        for stmt in stmts {
            let mini = Block {
                stmts: vec![stmt.clone()],
                final_expr: None,
                span: span.clone(),
            };
            self.refs_in_block(&mini, &mut refs, &mut inner_defs);
        }
        let mut captures: Vec<String> = refs
            .into_iter()
            .filter(|n| !inner_defs.contains(n))
            .filter(|n| self.variables.contains_key(n.as_str()))
            .collect();
        captures.sort(); // deterministic order

        // 2. Build the shared env struct. Use a dummy i8 when there are no
        //    captures so we always have a valid struct type to point at.
        let env_field_types: Vec<BasicTypeEnum<'ctx>> = if captures.is_empty() {
            vec![self.context.i8_type().into()]
        } else {
            captures.iter().map(|n| self.variables[n].ty).collect()
        };
        let env_struct_ty = self.context.struct_type(&env_field_types, false);

        // 3. Allocate and populate the env struct in the outer function. We
        //    capture by value (copy semantics) — sufficient for the types the
        //    rest of the codegen already supports (integers, floats, pointers).
        let outer_fn = self.current_fn.unwrap();
        let env_alloca = self.create_entry_alloca(outer_fn, "__par_env", env_struct_ty.into());
        if !captures.is_empty() {
            let mut env_agg = env_struct_ty.get_undef();
            for (i, name) in captures.iter().enumerate() {
                let slot = self.variables[name];
                let val = self.builder.build_load(slot.ty, slot.ptr, name).unwrap();
                env_agg = self
                    .builder
                    .build_insert_value(env_agg, val, i as u32, "__par_env_field")
                    .unwrap()
                    .into_struct_value();
            }
            self.builder.build_store(env_alloca, env_agg).unwrap();
        }

        // 4. Generate one branch function per statement.
        //    Mint the SpawnSiteId via `record_spawn_site` (Debugger
        //    Contract slice 3): the helper bumps `par_counter`, returns
        //    the assigned ID, and pushes a metadata record carrying the
        //    par-block's `(file, line, col)` and static branch count for
        //    `KARAC_SPAWN_SITES` to emit at the end of compilation.
        //    Both call sites of `emit_par_run` (explicit `par {}` via
        //    `compile_par_block`, inferred groups via
        //    `compile_function_body`) flow through this single
        //    recording site.
        let par_id = self.record_spawn_site(span, Some(stmts.len() as u32));
        let mut branch_fn_ptrs: Vec<PointerValue<'ctx>> = Vec::with_capacity(stmts.len());
        for (i, stmt) in stmts.iter().enumerate() {
            let fn_ptr = self.emit_par_branch_fn(
                par_id,
                i,
                stmt,
                &captures,
                &env_field_types,
                env_struct_ty,
            )?;
            branch_fn_ptrs.push(fn_ptr);
        }

        // 5. Build the KaracBranch array on the stack, one entry per branch.
        let i64_type = self.context.i64_type();
        let branches_ty = self.karac_branch_ty.array_type(stmts.len() as u32);
        let branches_alloca =
            self.create_entry_alloca(outer_fn, "__par_branches", branches_ty.into());
        for (i, fn_ptr) in branch_fn_ptrs.iter().enumerate() {
            let mut entry = self.karac_branch_ty.get_undef();
            entry = self
                .builder
                .build_insert_value(entry, *fn_ptr, 0, "__par_branch_fn")
                .unwrap()
                .into_struct_value();
            entry = self
                .builder
                .build_insert_value(entry, env_alloca, 1, "__par_branch_ctx")
                .unwrap()
                .into_struct_value();
            let idx = [
                i64_type.const_int(0, false),
                i64_type.const_int(i as u64, false),
            ];
            let elem_ptr = unsafe {
                self.builder
                    .build_in_bounds_gep(branches_ty, branches_alloca, &idx, "__par_branch_slot")
                    .unwrap()
            };
            self.builder.build_store(elem_ptr, entry).unwrap();
        }

        // 6. Call karac_par_run(branches, count, par_id).
        //    `par_id` (Debugger Contract slice 4) was minted via
        //    `record_spawn_site` above; the runtime uses it to populate
        //    `KaracFrame::spawn_site_id` for slice 5's enumeration surface.
        let count = i64_type.const_int(stmts.len() as u64, false);
        let par_id_val = self.context.i32_type().const_int(par_id as u64, false);
        self.builder
            .build_call(
                self.karac_par_run_fn,
                &[branches_alloca.into(), count.into(), par_id_val.into()],
                "__par_run",
            )
            .unwrap();

        Ok(unit)
    }

    /// Generate the branch function for a single par-block statement.
    /// Signature: `void __par_branch_<par_id>_<i>(ptr ctx)`.
    ///
    /// The function unpacks captured locals from the shared env struct,
    /// compiles the statement, and returns. Captures are loaded as fresh
    /// allocas so the statement body sees them as ordinary locals.
    #[allow(clippy::result_large_err)]
    fn emit_par_branch_fn(
        &mut self,
        par_id: u32,
        index: usize,
        stmt: &Stmt,
        captures: &[String],
        env_field_types: &[BasicTypeEnum<'ctx>],
        env_struct_ty: StructType<'ctx>,
    ) -> Result<PointerValue<'ctx>, String> {
        let fn_name = format!("__par_branch_{}_{}", par_id, index);
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        // Branch function signature: void fn(ptr ctx, ptr cancel_flag)
        let fn_type = self.context.void_type().fn_type(
            &[
                BasicMetadataTypeEnum::from(ptr_ty),
                BasicMetadataTypeEnum::from(ptr_ty),
            ],
            false,
        );
        let branch_fn = self.module.add_function(&fn_name, fn_type, None);

        // Save outer codegen state — we're about to compile a fresh function.
        let saved_bb = self.builder.get_insert_block();
        let saved_fn = self.current_fn;
        let saved_vars = std::mem::take(&mut self.variables);
        let saved_var_types = std::mem::take(&mut self.var_type_names);
        let saved_loop_stack = std::mem::take(&mut self.loop_stack);
        let saved_cleanup = std::mem::take(&mut self.scope_cleanup_actions);
        let saved_cancel_ptr = self.branch_cancel_ptr.take();

        self.current_fn = Some(branch_fn);
        let entry = self.context.append_basic_block(branch_fn, "entry");
        self.builder.position_at_end(entry);

        // Cancel check at branch start: if *cancel_flag != 0, return immediately.
        let cancel_ptr = branch_fn.get_nth_param(1).unwrap().into_pointer_value();
        // Stash the cancel pointer so subsequent `compile_call` invocations
        // can emit mid-branch cooperative cancel checks before each callee.
        self.branch_cancel_ptr = Some(cancel_ptr);
        let i8_ty = self.context.i8_type();
        let cancel_val = self
            .builder
            .build_load(i8_ty, cancel_ptr, "cancel")
            .unwrap()
            .into_int_value();
        let is_cancelled = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                cancel_val,
                i8_ty.const_int(0, false),
                "is_cancelled",
            )
            .unwrap();
        let body_bb = self.context.append_basic_block(branch_fn, "body");
        let cancel_bb = self.context.append_basic_block(branch_fn, "cancelled");
        self.builder
            .build_conditional_branch(is_cancelled, cancel_bb, body_bb)
            .unwrap();
        self.builder.position_at_end(cancel_bb);
        self.builder.build_return(None).unwrap();
        self.builder.position_at_end(body_bb);

        // Unpack captures from the env struct into fresh allocas.
        if !captures.is_empty() {
            let env_ptr = branch_fn.get_nth_param(0).unwrap().into_pointer_value();
            let env_val = self
                .builder
                .build_load::<BasicTypeEnum<'ctx>>(env_struct_ty.into(), env_ptr, "__env")
                .unwrap();
            for (i, var_name) in captures.iter().enumerate() {
                let cap_ty = env_field_types[i];
                let field_val = self
                    .builder
                    .build_extract_value(env_val.into_struct_value(), i as u32, var_name)
                    .unwrap();
                let alloca = self.create_entry_alloca(branch_fn, var_name, cap_ty);
                self.builder.build_store(alloca, field_val).unwrap();
                self.variables.insert(
                    var_name.clone(),
                    VarSlot {
                        ptr: alloca,
                        ty: cap_ty,
                    },
                );
                // Propagate the outer scope's struct/enum type binding so
                // method dispatch can route `var.method()` through the
                // user impl-block path inside the par branch.
                if let Some(type_name) = saved_var_types.get(var_name) {
                    self.var_type_names
                        .insert(var_name.clone(), type_name.clone());
                }
            }
        }

        // Compile the statement body. Any errors surface to the outer context.
        let stmt_result = self.compile_stmt(stmt);

        // Terminate the branch function. The par-block API discards branch
        // return values in this first cut.
        if self
            .builder
            .get_insert_block()
            .unwrap()
            .get_terminator()
            .is_none()
        {
            self.builder.build_return(None).unwrap();
        }

        // Restore outer state.
        self.branch_cancel_ptr = saved_cancel_ptr;
        self.scope_cleanup_actions = saved_cleanup;
        self.loop_stack = saved_loop_stack;
        self.var_type_names = saved_var_types;
        self.variables = saved_vars;
        self.current_fn = saved_fn;
        if let Some(bb) = saved_bb {
            self.builder.position_at_end(bb);
        }

        stmt_result?;
        Ok(branch_fn.as_global_value().as_pointer_value())
    }

    /// If we are currently compiling a par-branch function body, emit a
    /// cooperative cancel check at the current insertion point: load the
    /// runtime's `AtomicBool` cancel flag, branch to a fresh "cancelled"
    /// block when set, otherwise fall through to a "continue" block. The
    /// cancelled block drains scope cleanup actions and `return`s void
    /// from the branch function, mirroring the entry-time check shape.
    /// No-op outside par branches.
    ///
    /// `callee` is the canonical name of the call about to be emitted (free
    /// fn `name` or `Type.method`). When `Some(name)` and
    /// `callee_effectful[name] == false`, the check is skipped — the
    /// callee carries no `reads`/`writes`/`sends`/`receives`, so a mid-branch
    /// cancellation cannot observe a partial side effect via this call.
    /// `None` (or an unknown name) preserves the conservative MVP behavior.
    fn emit_branch_cancel_check(&mut self, label: &str, callee: Option<&str>) {
        let Some(cancel_ptr) = self.branch_cancel_ptr else {
            return;
        };
        if let Some(name) = callee {
            if let Some(false) = self.callee_effectful.get(name) {
                return;
            }
        }
        let i8_ty = self.context.i8_type();
        let cancel_val = self
            .builder
            .build_load(i8_ty, cancel_ptr, &format!("{label}.cancel.flag"))
            .unwrap()
            .into_int_value();
        let is_cancelled = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                cancel_val,
                i8_ty.const_int(0, false),
                &format!("{label}.cancelled"),
            )
            .unwrap();
        let fn_val = self.current_fn.unwrap();
        let cancel_bb = self
            .context
            .append_basic_block(fn_val, &format!("{label}.cancel.bb"));
        let cont_bb = self
            .context
            .append_basic_block(fn_val, &format!("{label}.cont.bb"));
        self.builder
            .build_conditional_branch(is_cancelled, cancel_bb, cont_bb)
            .unwrap();
        self.builder.position_at_end(cancel_bb);
        self.emit_scope_cleanup();
        self.builder.build_return(None).unwrap();
        self.builder.position_at_end(cont_bb);
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

        // 3. Determine param types from annotations (default to i64).
        let param_llvm_types: Vec<BasicTypeEnum<'ctx>> = params
            .iter()
            .map(|p| {
                p.ty.as_ref()
                    .map(|te| self.llvm_type_for_type_expr(te))
                    .unwrap_or_else(|| self.context.i64_type().into())
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
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let generic_fn = self.generic_fns[name].clone();

        // Compile argument values so we can infer concrete types.
        let arg_vals: Vec<BasicValueEnum<'ctx>> = args
            .iter()
            .map(|a| self.compile_expr(&a.value))
            .collect::<Result<_, _>>()?;

        // Infer type arguments from the argument value types.
        let subst = self.infer_type_args(&generic_fn, &arg_vals);

        // Mangle a unique name for this specialization (e.g. `max$i64`).
        let mangled = self.mangle_mono_name(name, &generic_fn, &subst);

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

            // Declare then compile the specialization.
            self.declare_mono_function(&generic_fn, &mangled)?;
            self.compile_mono_function(&generic_fn, &mangled)?;

            // Restore state.
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
    ) -> String {
        let params = match &func.generic_params {
            Some(gp) => &gp.params,
            None => return base.to_string(),
        };

        let mut mangled = base.to_string();
        for param in params {
            if let Some(ty) = subst.get(&param.name) {
                mangled.push('$');
                mangled.push_str(&self.llvm_type_to_mangle_str(*ty));
            }
        }
        mangled
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

    // ── Helpers ─────────────────────────────────────────────────

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

/// Pull the element `TypeExpr` out of `Vec[T]` — returns `None` for
/// non-Vec shapes or when generic args aren't a single type.
fn vec_inner_type_expr(te: &TypeExpr) -> Option<TypeExpr> {
    if let TypeKind::Path(path) = &te.kind {
        if path.segments.first().map(|s| s.as_str()) == Some("Vec") {
            if let Some(args) = &path.generic_args {
                if let Some(GenericArg::Type(elem)) = args.first() {
                    return Some(elem.clone());
                }
            }
        }
    }
    None
}

/// Pull the element `TypeExpr` out of `Slice[T]` or `mut Slice[T]`.
fn slice_inner_type_expr(te: &TypeExpr) -> Option<TypeExpr> {
    match &te.kind {
        TypeKind::Path(path) => {
            if path.segments.first().map(|s| s.as_str()) != Some("Slice") {
                return None;
            }
            let args = path.generic_args.as_ref()?;
            if args.len() != 1 {
                return None;
            }
            match &args[0] {
                GenericArg::Type(t) => Some(t.clone()),
                _ => None,
            }
        }
        TypeKind::MutSlice(inner) => Some((**inner).clone()),
        _ => None,
    }
}

/// Pull the element `TypeExpr` out of `Set[T]`.
fn set_inner_type_expr(te: &TypeExpr) -> Option<TypeExpr> {
    if let TypeKind::Path(path) = &te.kind {
        if path.segments.first().map(|s| s.as_str()) == Some("Set") {
            if let Some(args) = &path.generic_args {
                if let Some(GenericArg::Type(elem)) = args.first() {
                    return Some(elem.clone());
                }
            }
        }
    }
    None
}

/// Pull the (key, value) `TypeExpr`s out of `Map[K, V]`.
fn map_kv_type_exprs(te: &TypeExpr) -> Option<(TypeExpr, TypeExpr)> {
    if let TypeKind::Path(path) = &te.kind {
        if path.segments.first().map(|s| s.as_str()) != Some("Map") {
            return None;
        }
        let args = path.generic_args.as_ref()?;
        if args.len() != 2 {
            return None;
        }
        let k = match &args[0] {
            GenericArg::Type(t) => t.clone(),
            _ => return None,
        };
        let v = match &args[1] {
            GenericArg::Type(t) => t.clone(),
            _ => return None,
        };
        Some((k, v))
    } else {
        None
    }
}

/// Extract the type name from an impl block's target TypeExpr.
/// Returns `None` for non-path targets (slice/array/etc.) since those
/// can't carry user-defined impl methods in v1.
fn impl_target_name(target: &TypeExpr) -> Option<String> {
    match &target.kind {
        TypeKind::Path(p) => p.segments.last().cloned(),
        _ => None,
    }
}

/// Recognize the `with_provider[R](provider, closure)` call shape at AST
/// level. Mirror of `Interpreter::match_with_provider` and
/// `provider_escape::match_with_provider`. Returns `(R, provider_expr,
/// closure_expr)` when the callee is `Index(Ident("with_provider") |
/// Path(["with_provider"]), R)` with exactly two unlabeled args, else `None`.
fn match_with_provider_call<'e>(
    callee: &'e Expr,
    args: &'e [CallArg],
) -> Option<(String, &'e Expr, &'e Expr)> {
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
    let resource = match &index.kind {
        ExprKind::Identifier(n) => n.clone(),
        ExprKind::Path { segments, .. } => segments.last().cloned()?,
        _ => return None,
    };
    if args.len() != 2 {
        return None;
    }
    Some((resource, &args[0].value, &args[1].value))
}

/// `true` when the method has no `ref T` / `mut ref T` parameters, so its
/// signature matches what the operator-lowering pass emits — every binop
/// rewrite at `lowering.rs` passes operands by value through
/// `Path(Type, method)(a, b)`. Used to break ties between duplicate impls
/// of the same method (e.g. `impl PartialEq for Point { fn eq(ref self,
/// ref Point) }` and `impl Eq for Point { fn eq(self, Point) }`): the
/// value-form impl is the one whose function signature lines up with
/// what the call site actually passes. `make_impl_method_function`
/// already synthesizes `self` as value-typed regardless of the source
/// `ref self`, so this check focuses on the non-self params.
fn method_self_is_value(method: &Function) -> bool {
    !method
        .params
        .iter()
        .any(|p| matches!(&p.ty.kind, TypeKind::Ref(_) | TypeKind::MutRef(_)))
}

/// Build a synthetic `Function` node for an impl-block method so the
/// existing `declare_function` / `compile_function` machinery can emit it
/// as an LLVM function named `Type.method`. If the method has a receiver,
/// prepend a `self` parameter whose type mirrors the source self mode:
/// `self` → `Type` (owned), `ref self` → `ref Type`, `mut ref self` →
/// `mut ref Type`. The existing ref-param plumbing in `compile_function`
/// (`ref_params`, `get_data_ptr`, `load_variable` deref) handles each
/// case from there; ref-self mutations write back to the caller's
/// storage via the pointer-typed self param.
fn make_impl_method_function(type_name: &str, method: &Function) -> Function {
    let mut f = method.clone();
    f.name = format!("{}.{}", type_name, method.name);
    if let Some(self_kind) = method.self_param.as_ref() {
        let span = method.span.clone();
        let base = TypeExpr {
            kind: TypeKind::Path(PathExpr {
                segments: vec![type_name.to_string()],
                generic_args: None,
                span: span.clone(),
            }),
            span: span.clone(),
        };
        let ty = match self_kind {
            SelfParam::Owned => base,
            SelfParam::Ref => TypeExpr {
                kind: TypeKind::Ref(Box::new(base)),
                span: span.clone(),
            },
            SelfParam::MutRef => TypeExpr {
                kind: TypeKind::MutRef(Box::new(base)),
                span: span.clone(),
            },
        };
        let self_param = Param {
            span: span.clone(),
            pattern: Pattern {
                kind: PatternKind::Binding("self".to_string()),
                span,
            },
            ty,
            default_value: None,
            doc_comment: None,
        };
        f.params.insert(0, self_param);
    }
    f.self_param = None;
    f
}
