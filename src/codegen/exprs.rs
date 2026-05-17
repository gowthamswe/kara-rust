//! Expression compilation.
//!
//! Houses `compile_expr` (the big per-expression-kind dispatch),
//! `compile_offset_of` (the `offset_of!` intrinsic), `compile_path_expr`
//! (multi-segment-path-as-value lowering), `compile_question` (`?`
//! propagation), `emit_error_trace_push` /
//! `ensure_source_filename_global` (error-return-trace runtime
//! integration), `compile_struct_init` (struct literal), and the SOA
//! collection constructors `compile_soa_new` / `compile_soa_method`.

use crate::ast::*;

use inkwell::types::BasicTypeEnum;
use inkwell::values::{BasicValueEnum, PointerValue};
use inkwell::AddressSpace;

use super::state::{SoaGroup, SoaLayout, VarSlot};

impl<'ctx> super::Codegen<'ctx> {
    pub(super) fn compile_expr(&mut self, expr: &Expr) -> Result<BasicValueEnum<'ctx>, String> {
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
                // vec_ty is the same struct type used for Vec/String. The
                // f-string accumulator is always `String` (Vec[u8]); element
                // type u8 has no heap content, so no recursive drop needed.
                let u8_ty: BasicTypeEnum<'ctx> = self.context.i8_type().into();
                self.track_vec_var(acc, Some(u8_ty));

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
                // Stage the acc pointer so a consuming Let / Assign whose
                // LHS is a tracked Vec/String slot can suppress the acc's
                // scope-exit cleanup (the LHS now owns the buffer; without
                // suppression both cleanups fire on the same pointer and
                // double-free hangs in macOS malloc_printf). Discard / pass-
                // through consumers (println(f"..."), function args, etc.)
                // leave the staged value in place — the next compile_expr
                // overwrites it, or the surrounding scope's cleanup walk
                // ignores it.
                self.last_fstr_acc = Some(acc);
                Ok(result)
            }
            ExprKind::Identifier(name) => {
                // Resolution order: const-generic param (slice 4 — when
                // an active monomorphization's `const_subst` binds the
                // name, lower to the LLVM constant of the matching
                // width); local variable (may shadow a const), then
                // unit enum variant, then top-level `const` (re-compile
                // the stored value expression at this use site so LLVM
                // folds it), then free-fn-name-as-value (Slice B
                // follow-up 2026-05-09 — `let f = my_free_fn;` lowers
                // to the fn's global pointer; consumers that take
                // fn-pointer slots use it as a typed indirect-call
                // target), and finally `load_variable` so the existing
                // "Undefined variable" diagnostic still fires for
                // genuinely unbound names.
                if let Some(cv) = self.const_subst.get(name) {
                    let cv = cv.clone();
                    return Ok(self.compile_primitive_const(&cv));
                }
                if self.variables.contains_key(name.as_str()) {
                    self.load_variable(name)
                } else if let Some(ev) = self.try_unit_enum_variant(name) {
                    Ok(ev)
                } else if let Some(const_value) = self.consts.get(name).cloned() {
                    self.compile_expr(&const_value)
                } else if let Some(fv) = self.module.get_function(name) {
                    // Free fn name → fn pointer. The LLVM type is
                    // `ptr` at this layer; downstream consumers (FFI
                    // dispatchers like `Server.serve`) use it as a
                    // typed indirect-call target. v1 doesn't yet track
                    // the fn's source-level signature on the resulting
                    // value — direct calls through such a binding (e.g.
                    // `let f = target; f()`) are not supported and
                    // would fall through to the generic call path's
                    // unknown-callee branch. The intended consumer is
                    // free-fn-as-`Fn`-arg dispatch (Server.serve and
                    // similar FFI extern hookups).
                    Ok(fv.as_global_value().as_pointer_value().into())
                } else {
                    self.load_variable(name)
                }
            }
            ExprKind::SelfValue => {
                // `self` is bound as an ordinary local by `compile_function`'s
                // parameter loop (impl methods prepend a `self: Type` param).
                self.load_variable("self")
            }
            ExprKind::Binary { op, left, right } => match op {
                // Short-circuit `and`/`or` per documented design
                // (roadmap.md:425, 429): RHS only evaluates when the
                // LHS doesn't already determine the result, so RHS
                // side-effects (panicking index, dropped fn call)
                // don't fire when short-circuited.
                BinOp::And | BinOp::Or => self.compile_short_circuit(op, left, right),
                _ => {
                    let lhs = self.compile_expr(left)?;
                    let rhs = self.compile_expr(right)?;
                    self.compile_binop(op, lhs, rhs)
                }
            },
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
                label,
                condition,
                body,
            } => self.compile_while(label.as_deref(), condition, body),
            ExprKind::Loop { label, body } => self.compile_loop(label.as_deref(), body),
            ExprKind::Break { label, value } => {
                self.compile_break(label.as_deref(), value.as_deref())
            }
            ExprKind::Continue { label } => self.compile_continue(label.as_deref()),
            ExprKind::Closure { params, body, .. } => {
                self.compile_closure(params, body, &expr.span)
            }
            ExprKind::Return(val) => {
                // Early-return cleanup parity with the function-end path
                // at `compile_function`: walk the full `scope_cleanup_actions`
                // stack and emit cleanup IR for every tracked binding before
                // building the return. Without this, heap-owning locals
                // (Vec data buffers, Map handles, RC-fallback heap boxes)
                // leak per-call whenever control exits via `return expr`
                // instead of falling through to the function body's tail.
                // Move-aware suppression mirrors the tail-return path: when
                // the return value is an Identifier naming a tracked
                // Vec / String, the caller now owns that buffer, so zeroing
                // the source's `cap` before cleanup skips the free for the
                // moved-out binding while still freeing every other tracked
                // local. Closes the 2026-05-13 bfs_sieve residual leak —
                // `return d` from inside a loop's match-arm body was
                // bypassing cleanup for `factors` / `bucket` / `visited` /
                // `queue`, leaking the entire per-call working set.
                if let Some(e) = val {
                    self.suppress_source_vec_cleanup_for_arg(e);
                    let v = self.compile_expr(e)?;
                    self.emit_scope_cleanup();
                    self.builder.build_return(Some(&v)).unwrap();
                } else {
                    self.emit_scope_cleanup();
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
                label,
                pattern,
                iterable,
                body,
            } => self.compile_for(label.as_deref(), pattern, iterable, body),
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
            ExprKind::LabeledBlock { label, body, .. } => self.compile_labeled_block(label, body),
            ExprKind::OffsetOf { ty, field_path } => self.compile_offset_of(ty, field_path),
            _ => Ok(self.context.i64_type().const_int(0, false).into()),
        }
    }

    /// Lower `offset_of[T](field.path)` to a compile-time `usize` constant.
    /// The typechecker has already validated that `T` is a struct and the
    /// path is well-typed; here we walk the lowered LLVM struct types to
    /// chain `TargetData::offset_of_element` across nested-path segments.
    /// Returns the byte offset as an `i64` constant.
    pub(super) fn compile_offset_of(
        &mut self,
        ty: &TypeExpr,
        field_path: &[String],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_ty = self.context.i64_type();
        // Recover the initial struct name from the type expression.
        let TypeKind::Path(path) = &ty.kind else {
            return Err("offset_of: target must be a path-named struct".to_string());
        };
        let mut current_struct_name = path
            .segments
            .last()
            .ok_or_else(|| "offset_of: empty type path".to_string())?
            .clone();
        let mut total_offset: u64 = 0;
        for segment_name in field_path {
            let struct_ty = self
                .struct_types
                .get(&current_struct_name)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "offset_of: struct '{current_struct_name}' has no LLVM \
                         type registration"
                    )
                })?;
            let field_names = self
                .struct_field_names
                .get(&current_struct_name)
                .ok_or_else(|| {
                    format!(
                        "offset_of: struct '{current_struct_name}' has no field-name \
                         table"
                    )
                })?
                .clone();
            let field_idx = field_names
                .iter()
                .position(|n| n == segment_name)
                .ok_or_else(|| {
                    format!(
                        "offset_of: field '{segment_name}' not found on struct \
                         '{current_struct_name}'"
                    )
                })?;
            let target_data = self.ensure_target_data()?;
            let segment_offset = target_data
                .offset_of_element(&struct_ty, field_idx as u32)
                .ok_or_else(|| {
                    format!(
                        "offset_of: TargetData rejected element index {field_idx} \
                         on struct '{current_struct_name}'"
                    )
                })?;
            total_offset += segment_offset;
            // Chase the field's type for the next segment.
            let field_type_names = self
                .struct_field_type_names
                .get(&current_struct_name)
                .cloned();
            if let Some(ftns) = field_type_names {
                if let Some(Some(next_name)) = ftns.get(field_idx) {
                    current_struct_name = next_name.clone();
                }
            }
        }
        Ok(i64_ty.const_int(total_offset, false).into())
    }

    /// Compile a `Type.Variant` path expression. The parser emits `Color.Red`
    /// as `ExprKind::Path(["Color", "Red"])` (any dotted ident sequence whose
    /// segments start with an uppercase letter). The only case currently
    /// reaching this arm is unit-variant construction — payload-bearing
    /// variants go through `ExprKind::Call { callee: Path(...), args }` and
    /// are dispatched by `compile_assoc_call`.
    pub(super) fn compile_path_expr(
        &mut self,
        segments: &[String],
    ) -> Result<BasicValueEnum<'ctx>, String> {
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
    pub(super) fn compile_question(
        &mut self,
        inner: &Expr,
        outer_span: &crate::token::Span,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let val = self.compile_expr(inner)?;
        let i64_t = self.context.i64_type();
        // The early-return struct must match the enclosing function's
        // declared LLVM return type. Result keeps the legacy
        // `{i64, i64}` layout; Option was widened to
        // `{i64, i64, i64, i64}` to fit multi-word payloads (tuple,
        // Vec, String). Pull the actual return type from the function
        // declaration instead of hardcoding a narrow shape.
        let fn_val = self.current_fn.unwrap();
        let enum_ty = match fn_val.get_type().get_return_type() {
            Some(BasicTypeEnum::StructType(s)) => s,
            _ => self.context.struct_type(
                &[BasicTypeEnum::IntType(i64_t), BasicTypeEnum::IntType(i64_t)],
                false,
            ),
        };

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
    pub(super) fn emit_error_trace_push(&mut self, outer_span: &crate::token::Span) {
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
    pub(super) fn ensure_source_filename_global(&mut self) -> Option<(PointerValue<'ctx>, u64)> {
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

    pub(super) fn compile_struct_init(
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
                    // Move-aware suppression for `Foo { ..., body: body }`
                    // when the field expr is an Identifier naming a
                    // tracked Vec / String. The struct field now owns
                    // the heap buffer; without this, the source's
                    // scope-exit `FreeVecBuffer` frees the buffer the
                    // struct value carries downstream, producing UAF
                    // when the consumer reads through the struct.
                    // Mirrors the enum-variant constructor pattern
                    // already wired at `try_compile_enum_variant`.
                    self.suppress_source_vec_cleanup_for_arg(&field_init.value);
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
                // Move-aware suppression — same shape as the shared-
                // struct branch above. The new struct aggregate carries
                // the source's data pointer; suppress the source's
                // scope-exit free so the consumer can read through.
                self.suppress_source_vec_cleanup_for_arg(&field_init.value);
            }
            Ok(agg.into())
        } else {
            Ok(self.context.i64_type().const_int(0, false).into())
        }
    }

    /// Compile `let <name>: Vec[T] = Vec::new()` for a SoA-laid-out collection.
    /// Produces `{ null, ..., [null_cold,] 0, 0 }` (one null ptr per group plus optional cold, len=0, cap=0).
    pub(super) fn compile_soa_new(
        &mut self,
        var_name: &str,
        soa: &SoaLayout,
    ) -> Result<(), String> {
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
        // Track for scope cleanup (need to free each group buffer). SoA's
        // multi-group cleanup is its own shape; the recursive-element drop
        // path doesn't apply here, so pass `None` to use the legacy
        // outer-buffer-only free.
        self.track_vec_var(alloca, None);
        Ok(())
    }

    pub(super) fn compile_soa_method(
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
}
