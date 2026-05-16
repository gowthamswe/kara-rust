//! HTTP handler ABI shim and Request method codegen.
//!
//! Houses the three methods that bridge Kāra `Server.serve(handler)`
//! to the FFI extern `void (*)(*const KaracHttpRequest, *mut
//! KaracHttpResponse)` slot: `resolve_free_fn_for_handler_arg`
//! (validates and dereferences the user fn pointer),
//! `emit_http_handler_shim` (synthesizes the per-handler extern "C"
//! shim function), and `compile_request_string_method` (lowers
//! `Request.path()` / `Request.method()` to the runtime externs).

use crate::ast::*;

use inkwell::module::Linkage;
use inkwell::values::BasicValueEnum;
use inkwell::AddressSpace;

impl<'ctx> super::Codegen<'ctx> {
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
    pub(super) fn resolve_free_fn_for_handler_arg(
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
    pub(super) fn emit_http_handler_shim(
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
    pub(super) fn compile_request_string_method(
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
}
