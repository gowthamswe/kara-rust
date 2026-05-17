//! Runtime intrinsic emission: refcounting, scope cleanup, var-tracking
//! registration, and the string-build helpers used by f-strings.
//!
//! Houses `emit_panic`, the RC/Arc alloc/inc/dec primitives
//! (`emit_rc_alloc`, `emit_rc_inc`, `emit_rc_dec`, `emit_arc_inc`,
//! `emit_arc_dec`, `emit_refcount_inc`, `emit_refcount_dec`), the
//! per-variable cleanup-registration helpers
//! (`track_rc_var`, `track_vec_var`, `track_map_var`, `track_enum_var`,
//! `track_struct_var`, `enum_name_for_binding`), the scope-cleanup
//! emission (`emit_scope_cleanup`, `drain_top_frame_with_emit`,
//! `emit_cleanup_action`), and the f-string raw-builder helpers
//! (`emit_string_append_raw`, `compile_fstr_part_to_cstr`).

use crate::ast::*;

use inkwell::types::{BasicTypeEnum, StructType};
use inkwell::values::{BasicValueEnum, FunctionValue, PointerValue};
use inkwell::{AddressSpace, AtomicOrdering, AtomicRMWBinOp, IntPredicate};

use super::state::CleanupAction;

impl<'ctx> super::Codegen<'ctx> {
    /// Allocate a new RC heap object: `malloc(sizeof(heap_type))`, store refcount = 1.
    /// Returns a pointer to the heap object.
    pub(super) fn emit_panic(&self, message: &str) {
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

    pub(super) fn emit_rc_alloc(&self, heap_type: StructType<'ctx>) -> PointerValue<'ctx> {
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
    pub(super) fn emit_rc_inc(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
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
    pub(super) fn emit_rc_dec(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
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
    pub(super) fn emit_arc_inc(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
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
    pub(super) fn emit_arc_dec(&self, heap_type: StructType<'ctx>, ptr: PointerValue<'ctx>) {
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
    pub(super) fn emit_refcount_inc(
        &self,
        name: &str,
        heap_type: StructType<'ctx>,
        ptr: PointerValue<'ctx>,
    ) {
        if self.is_arc_binding(name) {
            self.emit_arc_inc(heap_type, ptr);
        } else {
            self.emit_rc_inc(heap_type, ptr);
        }
    }

    /// Dispatch a dec on `name`'s refcount: atomic path when `name` is in
    /// `arc_fallback_fns` for the current function, plain non-atomic otherwise.
    pub(super) fn emit_refcount_dec(
        &self,
        name: &str,
        heap_type: StructType<'ctx>,
        ptr: PointerValue<'ctx>,
    ) {
        if self.is_arc_binding(name) {
            self.emit_arc_dec(heap_type, ptr);
        } else {
            self.emit_rc_dec(heap_type, ptr);
        }
    }

    /// Track a shared-type variable for scope-exit rc_dec.
    pub(super) fn track_rc_var(
        &mut self,
        name: &str,
        ptr: PointerValue<'ctx>,
        heap_type: StructType<'ctx>,
    ) {
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::RcDec {
                name: name.to_string(),
                ptr,
                heap_type,
            });
        }
    }

    /// Track a Vec/String alloca for scope-exit buffer free. Pass the
    /// element LLVM type (`vec_elem_types[var_name]`) so the cleanup loop
    /// can recursively drop nested heap-owning element types — critical
    /// for `Vec[Vec[T]]`, `Vec[String]`, `Vec[Map[K, V]]`, etc., where the
    /// outer buffer's free does not reach the inner allocations.
    pub(super) fn track_vec_var(
        &mut self,
        vec_alloca: PointerValue<'ctx>,
        elem_ty: Option<BasicTypeEnum<'ctx>>,
    ) {
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::FreeVecBuffer {
                vec_alloca,
                elem_ty,
            });
        }
    }

    /// Emit a runtime zero-store to a Vec/String alloca's `cap` field
    /// (slot index 2 of the `{data, len, cap}` struct). Used to suppress
    /// a queued `FreeVecBuffer` whose buffer ownership has moved to a
    /// different slot — the `cap > 0` guard in `emit_scope_cleanup`'s
    /// `FreeVecBuffer` walker turns the free into a no-op, leaving the
    /// new owner's own cleanup to run once.
    pub(super) fn zero_vec_alloca_cap(&self, vec_alloca: PointerValue<'ctx>) {
        let vec_ty = self.vec_struct_type();
        let i64_t = self.context.i64_type();
        if let Ok(cap_ptr) =
            self.builder
                .build_struct_gep(vec_ty, vec_alloca, 2, "fstr.acc.cap.suppress")
        {
            let _ = self.builder.build_store(cap_ptr, i64_t.const_int(0, false));
        }
    }

    /// Emit an eager free of a Vec/String slot's heap buffer, guarded on
    /// `cap > 0`. Used at move-overwrite sites where the slot is about to
    /// be reassigned to a new heap buffer — without this, the prior
    /// buffer leaks (the slot loses its only reference before scope-exit
    /// cleanup can reach it). Mirrors the runtime shape of `FreeVecBuffer`
    /// for the eager-free position. `cap = 0` slots (string literals,
    /// already-transferred sources) skip the free, preserving the static-
    /// vs-heap invariant the scope walker also relies on.
    pub(super) fn emit_free_vec_buffer_if_owned(&mut self, vec_alloca: PointerValue<'ctx>) {
        let vec_ty = self.vec_struct_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let fn_val = match self.current_fn {
            Some(f) => f,
            None => return,
        };
        let data_ptr = match self
            .builder
            .build_struct_gep(vec_ty, vec_alloca, 0, "ov.data.pp")
        {
            Ok(p) => p,
            Err(_) => return,
        };
        let cap_ptr = match self
            .builder
            .build_struct_gep(vec_ty, vec_alloca, 2, "ov.cap.pp")
        {
            Ok(p) => p,
            Err(_) => return,
        };
        let data = self
            .builder
            .build_load(ptr_ty, data_ptr, "ov.data")
            .unwrap()
            .into_pointer_value();
        let cap = self
            .builder
            .build_load(i64_t, cap_ptr, "ov.cap")
            .unwrap()
            .into_int_value();
        let zero = i64_t.const_int(0, false);
        let owned = self
            .builder
            .build_int_compare(IntPredicate::UGT, cap, zero, "ov.owned")
            .unwrap();
        let free_bb = self.context.append_basic_block(fn_val, "ov.free");
        let after_bb = self.context.append_basic_block(fn_val, "ov.after");
        self.builder
            .build_conditional_branch(owned, free_bb, after_bb)
            .unwrap();
        self.builder.position_at_end(free_bb);
        self.builder
            .build_call(self.free_fn, &[data.into()], "")
            .unwrap();
        self.builder.build_unconditional_branch(after_bb).unwrap();
        self.builder.position_at_end(after_bb);
    }

    /// Track a Map / Set alloca for scope-exit free. `key_is_vec` /
    /// `val_is_vec` tell the cleanup whether each side follows the
    /// Vec/String `{ptr, len, cap}` layout and therefore needs per-entry
    /// buffer release before the bucket storage is deallocated. Both
    /// false → plain `karac_map_free`. Either true → routes through
    /// `karac_map_free_with_drop_vec(handle, key_is_vec, val_is_vec)`
    /// so the per-entry walk runs.
    ///
    /// `val_shared_heap_type = Some(heap_ty)` triggers the codegen-side
    /// per-bucket rc_dec walk for shared-struct / shared-enum values
    /// (the runtime helper can't decrement refcounts itself — it's
    /// type-erased and doesn't know V's heap layout). Closes the
    /// `Map[K, shared T]` leak (2026-05-16): values previously
    /// stranded their refcount when the Map went out of scope.
    /// `key_shared_heap_type` is the symmetric K-side gate — fires
    /// the same walk against the key half of each occupied bucket
    /// (`Map[shared K, V]` / `Set[shared T]`).
    pub(super) fn track_map_var(
        &mut self,
        map_alloca: PointerValue<'ctx>,
        key_is_vec: bool,
        val_is_vec: bool,
        val_shared_heap_type: Option<StructType<'ctx>>,
        key_shared_heap_type: Option<StructType<'ctx>>,
    ) {
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::FreeMapHandle {
                map_alloca,
                key_is_vec,
                val_is_vec,
                val_shared_heap_type,
                key_shared_heap_type,
            });
        }
    }

    /// Phase 7.2 Slice DP — resolve a let-binding's surface enum name
    /// from the let-statement's annotation and RHS shape, for the
    /// `track_enum_var` registration site. Tries in order:
    ///
    /// 1. Existing `var_type_names` entry — populated by the upstream
    ///    type-hint pass when an explicit `let e: E = ...;` annotation
    ///    is present, or when an Identifier-RHS aliases a previously-
    ///    typed binding.
    /// 2. RHS = bare `Variant(args)` (`ExprKind::Call` with an Identifier
    ///    callee whose name matches a known variant) — walk `enum_layouts`
    ///    for the enum that owns that variant. Single-variant collisions
    ///    across enums are rare in practice and are tolerated by taking
    ///    the first match.
    /// 3. RHS = qualified `Enum.Variant(args)` (`ExprKind::Call` with a
    ///    Path-based callee whose first segment matches a known enum) —
    ///    use the first-segment name directly.
    /// 4. RHS = qualified `Enum.assoc_fn(args)` returning a value of the
    ///    enum's LLVM struct type — match by LLVM-struct-identity reverse-
    ///    lookup against `enum_layouts` (the same shape the existing
    ///    user-struct fallback at the let-site uses for structs).
    ///
    /// Returns `None` when the binding's surface type isn't a known
    /// value-type enum; the cleanup hook then becomes a no-op for that
    /// binding (matches v1 conservative behavior — no spurious cleanup).
    pub(super) fn enum_name_for_binding(
        &self,
        var_name: &str,
        value: &Expr,
        ty: Option<&TypeExpr>,
    ) -> Option<String> {
        // (1) Existing var_type_names entry pointing at a known enum.
        if let Some(n) = self.var_type_names.get(var_name) {
            if self.enum_layouts.contains_key(n) {
                return Some(n.clone());
            }
        }
        // Explicit annotation.
        if let Some(t) = ty {
            if let TypeKind::Path(p) = &t.kind {
                if let Some(seg) = p.segments.last() {
                    if self.enum_layouts.contains_key(seg) {
                        return Some(seg.clone());
                    }
                }
            }
        }
        // (2) / (3) Inspect the RHS Call shape.
        if let ExprKind::Call { callee, .. } = &value.kind {
            match &callee.kind {
                ExprKind::Identifier(n) => {
                    // Bare-name variant constructor.
                    for (en, layout) in &self.enum_layouts {
                        if layout.tags.contains_key(n) {
                            return Some(en.clone());
                        }
                    }
                }
                ExprKind::Path { segments, .. } => {
                    if let Some(first) = segments.first() {
                        if self.enum_layouts.contains_key(first) {
                            return Some(first.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Phase 7.2 Slice DP — register a value-type enum alloca for
    /// scope-exit drop-function invocation. Per design lock DP1, the
    /// registration site is at let-binding time (not inside
    /// `try_compile_enum_variant` — the variant constructor returns a
    /// `BasicValueEnum` aggregate before the alloca exists; the alloca
    /// is created by `bind_pattern_values`). Per DP3, `is_shared` enums
    /// are filtered upstream — RC inc/dec via `track_rc_var` handles
    /// their cleanup through refcount semantics. Per DP4, the
    /// scope-exit drain emits a single `call drop_fn(alloca)` for the
    /// `EnumDrop` action; move-suppression for caller→callee passing
    /// is implicit in the existing convention that function parameters
    /// don't register `track_enum_var` (mirrors how Vec/String params
    /// don't register `track_vec_var` — only the let-binding site
    /// owns cleanup, so the param is a stranded view of the same
    /// payload words and no double-free can occur).
    pub(super) fn track_enum_var(&mut self, enum_name: &str, enum_alloca: PointerValue<'ctx>) {
        // DP3 carve-out: shared enums use the RC-pointer cleanup path
        // (refcount-driven free in `emit_rc_dec`). The drop-switch
        // machinery is for value-type enums only.
        let is_shared = self
            .enum_layouts
            .get(enum_name)
            .map(|l| l.is_shared)
            .unwrap_or(false);
        if is_shared {
            return;
        }
        // Skip enums with no heap-bearing payload anywhere — emitting
        // a no-op drop call would just bloat IR. The drop-fn helper
        // returns `None` when every variant's `field_drop_kinds` is
        // entirely `EnumDropKind::None`.
        let drop_fn = match self.emit_enum_drop_switch(enum_name) {
            Some(f) => f,
            None => return,
        };
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::EnumDrop {
                enum_alloca,
                drop_fn,
            });
        }
    }

    /// Track a non-shared struct alloca for scope-exit drop-fn invocation.
    /// Mirrors `track_enum_var` but for struct types. The per-struct drop
    /// fn is lazily synthesized by `emit_struct_drop_synthesis`; if the
    /// struct has no heap-owning fields (every field is primitive / Slice
    /// / Ref / etc.) the synthesis returns `None` and we skip registration
    /// — there's nothing to drop. Shared structs use the RC machinery
    /// (`track_rc_var` / `emit_refcount_dec`) and are also filtered out by
    /// `emit_struct_drop_synthesis`.
    ///
    /// Closes the 2026-05-14 leak class for `struct Holder { v: Vec[i64] }`
    /// / `struct Cache { entries: Map[String, V] }` / `Vec[Container]`
    /// (slice γ of the recursive-drop work). Without this, a let-binding
    /// of a struct value never drops its Vec/Map/Set field contents on
    /// scope exit — only the struct's own inline storage (the
    /// `{ptr, len, cap}` field for a Vec field) was released, the actual
    /// heap-allocated backing buffer leaked.
    pub(super) fn track_struct_var(
        &mut self,
        struct_name: &str,
        struct_alloca: PointerValue<'ctx>,
    ) {
        let drop_fn = match self.emit_struct_drop_synthesis(struct_name) {
            Some(f) => f,
            None => return,
        };
        if let Some(frame) = self.scope_cleanup_actions.last_mut() {
            frame.push(CleanupAction::StructDrop {
                struct_alloca,
                drop_fn,
            });
        }
    }

    /// Emit all cleanup actions registered across all scope frames (for function exit).
    /// Iterates frames in reverse (innermost first) and within each frame in push order
    /// (consistent with how RAII destruction works in block-structured languages).
    pub(super) fn emit_scope_cleanup(&self) {
        let vec_ty = self.vec_struct_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();

        for frame in self.scope_cleanup_actions.iter().rev() {
            for action in frame {
                self.emit_cleanup_action(action, fn_val, vec_ty, ptr_ty, i64_t);
            }
        }
    }

    /// Drain the topmost `scope_cleanup_actions` frame: emit cleanup IR for
    /// every action it holds (in push order), then pop the frame. Used by
    /// `compile_match` to fire match-arm-scoped cleanups (let-bindings inside
    /// the arm body, plus the match-arm pattern binding itself) at end-of-arm
    /// instead of end-of-function — without this the alloca reuse across
    /// match-arm iterations leaks all but the last bound value.
    ///
    /// Caller is responsible for ensuring the basic-block insertion point is
    /// somewhere meaningful (i.e. the arm-body's end before the merge branch).
    /// No-op if the cleanup stack is empty.
    pub(super) fn drain_top_frame_with_emit(&mut self) {
        let Some(frame) = self.scope_cleanup_actions.pop() else {
            return;
        };
        let vec_ty = self.vec_struct_type();
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();
        for action in &frame {
            self.emit_cleanup_action(action, fn_val, vec_ty, ptr_ty, i64_t);
        }
    }

    /// Per-action cleanup IR emitter. Extracted from `emit_scope_cleanup` so
    /// the same code path serves both whole-stack drain (function-end /
    /// early-return cleanup) and top-frame drain (per-match-arm cleanup at
    /// `drain_top_frame_with_emit`). Signature takes pre-computed type
    /// handles so the caller hoists them out of inner loops.
    pub(super) fn emit_cleanup_action(
        &self,
        action: &CleanupAction<'ctx>,
        fn_val: FunctionValue<'ctx>,
        vec_ty: StructType<'ctx>,
        ptr_ty: inkwell::types::PointerType<'ctx>,
        i64_t: inkwell::types::IntType<'ctx>,
    ) {
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
            CleanupAction::FreeVecBuffer {
                vec_alloca,
                elem_ty,
            } => {
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

                // Recursive-drop fast path: when the element type is
                // itself a Vec/String struct, each live element owns
                // a separate data buffer. Iterate `len` elements and
                // free each one's `data` pointer before releasing
                // the outer buffer; otherwise those inner buffers
                // leak. Closes the 2026-05-13 cumulative-retention
                // bug measured on LeetCode #3629 bfs_sieve, where
                // `Vec[Vec[i64]]` leaked ~32 MB per `min_jumps`
                // call. One-level recursion handles the bench
                // workloads and the documented common case
                // (`Vec[Vec[T]]`, `Vec[String]`); deeper nesting
                // (`Vec[Vec[Vec[T]]]`) still leaks the innermost
                // buffers — tracked as a follow-up in `deferred.md`
                // § *Recursive Drop for Heap-Owned Collection
                // Elements > deeper-nesting limitation*.
                if let Some(et) = elem_ty {
                    if self.llvm_ty_is_vec_struct(*et) {
                        let len_ptr = self
                            .builder
                            .build_struct_gep(vec_ty, *vec_alloca, 1, "cleanup.len.ptr")
                            .unwrap();
                        let len = self
                            .builder
                            .build_load(i64_t, len_ptr, "cleanup.len")
                            .unwrap()
                            .into_int_value();
                        let counter =
                            self.create_entry_alloca(fn_val, "cleanup.drop.i", i64_t.into());
                        self.builder.build_store(counter, zero).unwrap();
                        let drop_cond_bb =
                            self.context.append_basic_block(fn_val, "cleanup.drop.cond");
                        let drop_body_bb =
                            self.context.append_basic_block(fn_val, "cleanup.drop.body");
                        let drop_after_bb = self
                            .context
                            .append_basic_block(fn_val, "cleanup.drop.after");
                        self.builder
                            .build_unconditional_branch(drop_cond_bb)
                            .unwrap();

                        self.builder.position_at_end(drop_cond_bb);
                        let cur = self
                            .builder
                            .build_load(i64_t, counter, "cleanup.drop.cur")
                            .unwrap()
                            .into_int_value();
                        let lt = self
                            .builder
                            .build_int_compare(IntPredicate::ULT, cur, len, "cleanup.drop.lt")
                            .unwrap();
                        self.builder
                            .build_conditional_branch(lt, drop_body_bb, drop_after_bb)
                            .unwrap();

                        self.builder.position_at_end(drop_body_bb);
                        // Each element is a Vec struct `{ptr, len,
                        // cap}` at `data + i * sizeof(VecStruct)`.
                        // Check inner cap > 0, then free inner ptr.
                        let inner_struct_ptr = unsafe {
                            self.builder
                                .build_gep(
                                    self.vec_struct_type(),
                                    data,
                                    &[cur],
                                    "cleanup.drop.elem",
                                )
                                .unwrap()
                        };
                        let inner_cap_ptr = self
                            .builder
                            .build_struct_gep(
                                self.vec_struct_type(),
                                inner_struct_ptr,
                                2,
                                "cleanup.drop.inner.cap.ptr",
                            )
                            .unwrap();
                        let inner_cap = self
                            .builder
                            .build_load(i64_t, inner_cap_ptr, "cleanup.drop.inner.cap")
                            .unwrap()
                            .into_int_value();
                        let inner_is_heap = self
                            .builder
                            .build_int_compare(
                                IntPredicate::UGT,
                                inner_cap,
                                zero,
                                "cleanup.drop.inner.is_heap",
                            )
                            .unwrap();
                        let inner_free_bb = self
                            .context
                            .append_basic_block(fn_val, "cleanup.drop.inner.free");
                        let inner_skip_bb = self
                            .context
                            .append_basic_block(fn_val, "cleanup.drop.inner.skip");
                        self.builder
                            .build_conditional_branch(inner_is_heap, inner_free_bb, inner_skip_bb)
                            .unwrap();

                        self.builder.position_at_end(inner_free_bb);
                        let inner_data_ptr = self
                            .builder
                            .build_struct_gep(
                                self.vec_struct_type(),
                                inner_struct_ptr,
                                0,
                                "cleanup.drop.inner.data.ptr",
                            )
                            .unwrap();
                        let inner_data = self
                            .builder
                            .build_load(ptr_ty, inner_data_ptr, "cleanup.drop.inner.data")
                            .unwrap()
                            .into_pointer_value();
                        self.builder
                            .build_call(self.free_fn, &[inner_data.into()], "")
                            .unwrap();
                        self.builder
                            .build_unconditional_branch(inner_skip_bb)
                            .unwrap();

                        self.builder.position_at_end(inner_skip_bb);
                        let one = i64_t.const_int(1, false);
                        let next = self
                            .builder
                            .build_int_add(cur, one, "cleanup.drop.next")
                            .unwrap();
                        self.builder.build_store(counter, next).unwrap();
                        self.builder
                            .build_unconditional_branch(drop_cond_bb)
                            .unwrap();

                        self.builder.position_at_end(drop_after_bb);
                    }
                }

                self.builder
                    .build_call(self.free_fn, &[data.into()], "")
                    .unwrap();
                self.builder.build_unconditional_branch(skip_bb).unwrap();
                self.builder.position_at_end(skip_bb);
            }
            CleanupAction::FreeMapHandle {
                map_alloca,
                key_is_vec,
                val_is_vec,
                val_shared_heap_type,
                key_shared_heap_type,
            } => {
                let handle = self
                    .builder
                    .build_load(ptr_ty, *map_alloca, "cleanup.map.handle")
                    .unwrap()
                    .into_pointer_value();
                // Shared-half rc_dec walks MUST run before the runtime
                // helper releases the bucket storage — they read each
                // live slot's bytes from `kv[]`. Closes the `Map[K,
                // shared T]` leak (2026-05-16) on the value side, and
                // the `Map[shared K, V]` / `Set[shared T]` leak on the
                // key side. Both fire when both K and V are shared.
                // Type-erased runtime can't decrement refcounts itself
                // because it doesn't know each half's heap layout;
                // codegen does, so the dec is open-coded per-
                // instantiation against the matching
                // `SharedTypeInfo.heap_type`.
                if let Some(heap_ty) = val_shared_heap_type {
                    self.emit_map_shared_half_rc_dec_walk(handle, *heap_ty, true);
                }
                if let Some(heap_ty) = key_shared_heap_type {
                    self.emit_map_shared_half_rc_dec_walk(handle, *heap_ty, false);
                }
                // When either the key or value type follows the Vec/String
                // `{ptr, len, cap}` layout, route through the recursive-
                // drop runtime helper so each live entry's heap content
                // is freed before the bucket array is deallocated. Plain
                // `karac_map_free` is correct only when both sides own
                // no heap. Closes the 2026-05-13 bucket leak (LeetCode
                // #3629 `Map[i64, Vec[i64]]`) and the 2026-05-14
                // `Set[String]` / `Map[String, V]` leaks (slice α /
                // β of the recursive-drop work).
                if *key_is_vec || *val_is_vec {
                    let i32_t = self.context.i32_type();
                    let key_flag = i32_t.const_int(if *key_is_vec { 1 } else { 0 }, false);
                    let val_flag = i32_t.const_int(if *val_is_vec { 1 } else { 0 }, false);
                    self.builder
                        .build_call(
                            self.karac_map_free_with_drop_vec_fn,
                            &[handle.into(), key_flag.into(), val_flag.into()],
                            "",
                        )
                        .unwrap();
                } else {
                    self.builder
                        .build_call(self.karac_map_free_fn, &[handle.into()], "")
                        .unwrap();
                }
            }
            // Phase 7.2 Slice DP — invoke the per-enum drop
            // function on the alloca. The drop fn takes a
            // pointer to the enum struct and walks the tag-
            // switch / per-variant cleanup BBs internally.
            CleanupAction::EnumDrop {
                enum_alloca,
                drop_fn,
            } => {
                self.builder
                    .build_call(*drop_fn, &[(*enum_alloca).into()], "")
                    .unwrap();
            }
            CleanupAction::StructDrop {
                struct_alloca,
                drop_fn,
            } => {
                self.builder
                    .build_call(*drop_fn, &[(*struct_alloca).into()], "")
                    .unwrap();
            }
        }
    }

    /// Walk every live bucket of `map_handle` and emit `rc_dec` on
    /// one half of the slot — value when `is_val == true`, key when
    /// `is_val == false`. Used by `FreeMapHandle` cleanup when the
    /// corresponding side is a shared struct / shared enum — the
    /// type-erased runtime (`karac_map_free_with_drop_vec`) only
    /// knows the Vec/String `{ptr, len, cap}` layout, so per-K / per-V
    /// refcount decrements have to be open-coded at the cleanup site
    /// against the matching `SharedTypeInfo.heap_type`. Mirrors the
    /// bucket-walk shape in `karac_map_free_with_drop_vec`
    /// (`runtime/src/map.rs`): for each `slot in 0..capacity`, check
    /// `status[slot] == OCCUPIED`, then load the half's pointer from
    /// `kv[slot*stride + offset]` (`offset = 0` for key, `key_size`
    /// for val) and rc_dec it.
    ///
    /// **Layout dependence.** Reads `capacity`, `status`, `kv`,
    /// `key_size`, `val_size` from the runtime's `#[repr(C)]`
    /// `KaracMap` at the offsets pinned by the runtime-side
    /// `karac_map_field_offsets_match_codegen` unit test. `key_size`
    /// and `val_size` are loaded at runtime (not const-folded from
    /// K/V LLVM widths) so the walk stays agnostic of K's / V's
    /// exact representation — the `kv` byte array's stride is
    /// `(key_size + val_size)` bytes, with the val half starting
    /// at `+key_size` and the key half at `+0`.
    ///
    /// **Concurrency.** The walk uses `emit_rc_dec` (non-atomic)
    /// rather than `emit_arc_dec`. Maps are local to a single thread
    /// (`unsafe impl Send for KaracMap`), and the cleanup runs on
    /// the thread that owns the Map, so non-atomic is correct here.
    /// If a future change shares Maps across threads via Arc, this
    /// callsite needs the atomic dispatch — same shape as the
    /// `emit_refcount_dec` decision in `RcDec` cleanup, but the
    /// map's keys / values aren't named bindings, so the
    /// `is_arc_binding` check has no anchor; an explicit `is_arc`
    /// flag on `FreeMapHandle` would be the path then.
    pub(super) fn emit_map_shared_half_rc_dec_walk(
        &self,
        map_handle: PointerValue<'ctx>,
        heap_type: StructType<'ctx>,
        is_val: bool,
    ) {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let i8_t = self.context.i8_type();
        let i64_t = self.context.i64_type();
        let fn_val = self.current_fn.unwrap();

        // Runtime layout offsets (pinned by
        // `karac_map_field_offsets_match_codegen`):
        //   0..8   status   *u8
        //   8..16  kv       *u8
        //   16..24 capacity usize
        //   24..32 len      usize
        //   32..40 tombstones usize
        //   40..48 key_size usize
        //   48..56 val_size usize
        const STATUS_OFFSET: u64 = 0;
        const KV_OFFSET: u64 = 8;
        const CAPACITY_OFFSET: u64 = 16;
        const KEY_SIZE_OFFSET: u64 = 40;
        const VAL_SIZE_OFFSET: u64 = 48;
        const BUCKET_OCCUPIED: u64 = 1;

        // Null guard — the registration site stores a fresh
        // `karac_map_new` handle which is non-null, but defensive
        // null-skip matches the runtime helper's first check
        // (`if map.is_null() { return; }`) so the cleanup is
        // robust against any future code path that might leave
        // the alloca uninitialized.
        let is_null = self
            .builder
            .build_is_null(map_handle, "cleanup.map.shared.is_null")
            .unwrap();
        let null_skip_bb = self
            .context
            .append_basic_block(fn_val, "cleanup.map.shared.null.skip");
        let walk_entry_bb = self
            .context
            .append_basic_block(fn_val, "cleanup.map.shared.walk.entry");
        self.builder
            .build_conditional_branch(is_null, null_skip_bb, walk_entry_bb)
            .unwrap();

        // ── walk.entry: load capacity, status, kv, key_size ─────
        self.builder.position_at_end(walk_entry_bb);
        let cap_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_handle,
                    &[i64_t.const_int(CAPACITY_OFFSET, false)],
                    "cleanup.map.shared.cap.p",
                )
                .unwrap()
        };
        let capacity = self
            .builder
            .build_load(i64_t, cap_p, "cleanup.map.shared.cap")
            .unwrap()
            .into_int_value();
        let status_pp = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_handle,
                    &[i64_t.const_int(STATUS_OFFSET, false)],
                    "cleanup.map.shared.status.pp",
                )
                .unwrap()
        };
        let status_ptr = self
            .builder
            .build_load(ptr_ty, status_pp, "cleanup.map.shared.status")
            .unwrap()
            .into_pointer_value();
        let kv_pp = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_handle,
                    &[i64_t.const_int(KV_OFFSET, false)],
                    "cleanup.map.shared.kv.pp",
                )
                .unwrap()
        };
        let kv_ptr = self
            .builder
            .build_load(ptr_ty, kv_pp, "cleanup.map.shared.kv")
            .unwrap()
            .into_pointer_value();
        let key_size_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_handle,
                    &[i64_t.const_int(KEY_SIZE_OFFSET, false)],
                    "cleanup.map.shared.ks.p",
                )
                .unwrap()
        };
        let key_size = self
            .builder
            .build_load(i64_t, key_size_p, "cleanup.map.shared.ks")
            .unwrap()
            .into_int_value();
        let val_size_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    map_handle,
                    &[i64_t.const_int(VAL_SIZE_OFFSET, false)],
                    "cleanup.map.shared.vs.p",
                )
                .unwrap()
        };
        let val_size = self
            .builder
            .build_load(i64_t, val_size_p, "cleanup.map.shared.vs")
            .unwrap()
            .into_int_value();
        let stride = self
            .builder
            .build_int_add(key_size, val_size, "cleanup.map.shared.stride")
            .unwrap();

        // Loop counter alloca'd in entry block.
        let counter = self.create_entry_alloca(fn_val, "cleanup.map.shared.i", i64_t.into());
        self.builder
            .build_store(counter, i64_t.const_zero())
            .unwrap();

        let cond_bb = self
            .context
            .append_basic_block(fn_val, "cleanup.map.shared.loop.cond");
        let body_bb = self
            .context
            .append_basic_block(fn_val, "cleanup.map.shared.loop.body");
        let occupied_bb = self
            .context
            .append_basic_block(fn_val, "cleanup.map.shared.loop.occupied");
        let next_bb = self
            .context
            .append_basic_block(fn_val, "cleanup.map.shared.loop.next");
        let exit_bb = self
            .context
            .append_basic_block(fn_val, "cleanup.map.shared.loop.exit");

        self.builder.build_unconditional_branch(cond_bb).unwrap();

        // ── loop.cond: i < capacity? ──────────────────────────────
        self.builder.position_at_end(cond_bb);
        let i_val = self
            .builder
            .build_load(i64_t, counter, "cleanup.map.shared.i.cur")
            .unwrap()
            .into_int_value();
        let cont = self
            .builder
            .build_int_compare(
                IntPredicate::ULT,
                i_val,
                capacity,
                "cleanup.map.shared.cont",
            )
            .unwrap();
        self.builder
            .build_conditional_branch(cont, body_bb, exit_bb)
            .unwrap();

        // ── loop.body: load status[i], occupied? ──────────────────
        self.builder.position_at_end(body_bb);
        let status_slot_p = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    status_ptr,
                    &[i_val],
                    "cleanup.map.shared.status.slot.p",
                )
                .unwrap()
        };
        let status_byte = self
            .builder
            .build_load(i8_t, status_slot_p, "cleanup.map.shared.status.byte")
            .unwrap()
            .into_int_value();
        let is_occupied = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                status_byte,
                i8_t.const_int(BUCKET_OCCUPIED, false),
                "cleanup.map.shared.is_occupied",
            )
            .unwrap();
        self.builder
            .build_conditional_branch(is_occupied, occupied_bb, next_bb)
            .unwrap();

        // ── loop.occupied: rc_dec value pointer ───────────────────
        self.builder.position_at_end(occupied_bb);
        let slot_off = self
            .builder
            .build_int_mul(i_val, stride, "cleanup.map.shared.slot.off")
            .unwrap();
        let slot_kv_p = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, kv_ptr, &[slot_off], "cleanup.map.shared.slot.kv.p")
                .unwrap()
        };
        // Key half lives at offset 0 within the bucket (`slot_kv_p`);
        // value half lives at `+key_size`. Both are pointer-sized on
        // shared types (rc-managed heap-pointer values are 8 bytes
        // on 64-bit).
        let half_ptr_p = if is_val {
            unsafe {
                self.builder
                    .build_in_bounds_gep(
                        i8_t,
                        slot_kv_p,
                        &[key_size],
                        "cleanup.map.shared.slot.val.p",
                    )
                    .unwrap()
            }
        } else {
            slot_kv_p
        };
        let half_ptr = self
            .builder
            .build_load(
                ptr_ty,
                half_ptr_p,
                if is_val {
                    "cleanup.map.shared.val.ptr"
                } else {
                    "cleanup.map.shared.key.ptr"
                },
            )
            .unwrap()
            .into_pointer_value();
        self.emit_rc_dec(heap_type, half_ptr);
        self.builder.build_unconditional_branch(next_bb).unwrap();

        // ── loop.next: i++, branch back to cond ──────────────────
        self.builder.position_at_end(next_bb);
        let i_next = self
            .builder
            .build_int_add(
                i_val,
                i64_t.const_int(1, false),
                "cleanup.map.shared.i.next",
            )
            .unwrap();
        self.builder.build_store(counter, i_next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();

        // ── loop.exit: fall through to null.skip via uncond jump ─
        self.builder.position_at_end(exit_bb);
        self.builder
            .build_unconditional_branch(null_skip_bb)
            .unwrap();

        // Continuation point — both the null-guard and the loop
        // funnel here so the caller can continue emitting the
        // `karac_map_free*` runtime call after this helper returns.
        self.builder.position_at_end(null_skip_bb);
    }

    // ── F-string helpers ──────────────────────────────────────────

    /// Append `src_len` bytes from `src_ptr` to the String (Vec<u8>) alloca at
    /// `dest_alloca`, growing the buffer if necessary.  Mirrors the inline
    /// `push_str` logic in `compile_vec_method`.
    pub(super) fn emit_string_append_raw(
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
    pub(super) fn compile_fstr_part_to_cstr(
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
}
