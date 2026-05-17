//! Closure compilation: literal capture, env-struct emission, indirect
//! calls, and the free-variable scan helpers.
//!
//! Houses `closure_value_type` (the `{fn_ptr, env_ptr}` fat-pointer
//! struct), `compile_closure` (the synthesized closure-body fn +
//! caller-side env capture), `compile_closure_call` (indirect call
//! through a closure binding), `infer_closure_return_type`, and the
//! `collect_closure_free_vars` / `refs_in_expr` / `refs_in_block`
//! free-variable scan helpers consumed by both closure capture and
//! par-block capture sets.

use crate::ast::*;
use crate::ownership::CapturePath;
use crate::resolver::SpanKey;
use crate::token::Span;
use std::collections::{HashMap, HashSet};

use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, StructType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum};
use inkwell::AddressSpace;

use super::state::VarSlot;

/// Per-root unpack plan for the disjoint-capture slice-4 per-path env
/// layout. Records how a captured root binding is rebuilt inside the
/// synthesized closure body from one or more env-struct slots.
///
/// `whole_root_slot = Some(idx)` means the env's slot at `idx` holds the
/// entire root value (matches today's per-name layout); the body unpack
/// loads the slot and stores into a root-named alloca, and field accesses
/// in the body walk it normally.
///
/// `whole_root_slot = None` means the root was captured *path-precisely*:
/// `sub_slots` lists the env slots that hold leaf values at non-empty
/// projection chains under this root. The body unpack allocates a fresh
/// root-typed alloca (uninit'd in the unread fields — the ownership pass
/// guarantees the body never reads them) and writes each sub-slot leaf
/// into its GEP chain. The body's field accesses then walk the stitched
/// root as if it were a whole-root capture.
struct RootUnpackPlan<'ctx> {
    /// LLVM type of the root in the outer scope (matches `VarSlot.ty`).
    root_ty: BasicTypeEnum<'ctx>,
    /// Source type-name of the root, if `var_type_names` has an entry.
    /// Propagated into the closure body's `var_type_names` so method
    /// dispatch on the captured root resolves through the user impl-block.
    type_name: Option<String>,
    /// `Some(env_slot_idx)` → whole-root capture; `None` → per-path.
    whole_root_slot: Option<usize>,
    /// Per-sub-path entries when `whole_root_slot` is None. Each tuple
    /// is `(env_slot_idx, gep_chain, leaf_ty)` — load env[idx] of type
    /// `leaf_ty`, then GEP into the root alloca via `gep_chain` and store.
    sub_slots: Vec<(usize, Vec<u32>, BasicTypeEnum<'ctx>)>,
}

/// Full per-closure capture layout — slot list (env struct field order)
/// plus the per-root unpack plans. Produced by
/// `Codegen::build_capture_path_layout` when ownership data is available
/// for the closure's `SpanKey` and every captured root resolves cleanly
/// through `struct_field_names` / `struct_field_type_names`. `None` →
/// fall back to the legacy `collect_closure_free_vars` per-name layout.
struct CapturePathLayout<'ctx> {
    /// Env-struct field types in slot order. Empty when no captures.
    slot_tys: Vec<BasicTypeEnum<'ctx>>,
    /// `slot_idx → (root_name, gep_chain)` — drives capture-site loads:
    /// for slot i, load `outer.variables[root]` via the gep chain and
    /// store into env field i. Empty `gep_chain` → store the whole-root
    /// value.
    slot_sources: Vec<(String, Vec<u32>)>,
    /// Per-root unpack plans, in deterministic root-name order. Drives
    /// the closure body's prelude.
    root_plans: Vec<(String, RootUnpackPlan<'ctx>)>,
}

impl<'ctx> super::Codegen<'ctx> {
    // ── Closure compilation ────────────────────────────────────────

    /// The LLVM struct type used to represent a closure fat-pointer: `{ ptr fn_ptr, ptr env_ptr }`.
    pub(super) fn closure_value_type(&self) -> StructType<'ctx> {
        let ptr = self.context.ptr_type(AddressSpace::default());
        self.context.struct_type(&[ptr.into(), ptr.into()], false)
    }

    /// Compile `|params| body` into a fat-pointer value `{ fn_ptr, env_ptr }`.
    ///
    /// Sets `pending_closure_fn_type` so the surrounding `let` binding can register the
    /// function type for later indirect calls.
    ///
    /// `closure_span` is the `ExprKind::Closure` expression's own span — used
    /// as the lookup key into `Codegen::closure_capture_paths` (sourced from
    /// `OwnershipCheckResult::closure_capture_path_modes`). When the ownership
    /// pass supplied per-path mode data for this closure and every captured
    /// root resolves cleanly through `struct_field_names`, the env struct is
    /// laid out with one field per captured path (disjoint-capture slice 4);
    /// otherwise the legacy per-captured-name layout from
    /// `collect_closure_free_vars` is used.
    pub(super) fn compile_closure(
        &mut self,
        params: &[ClosureParam],
        body: &Expr,
        closure_span: &Span,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let id = self.closure_counter;
        self.closure_counter += 1;
        let fn_name = format!("__closure_{}", id);

        // 1. Collect free variables (names referenced in body, not in
        //    params, present in scope). Always run the per-name walker —
        //    it doubles as the fallback when no per-path layout is
        //    available, and the per-path layout consults it indirectly
        //    via `self.variables` for the root types.
        let free_vars = self.collect_closure_free_vars(params, body);

        // 1b. Disjoint-capture slice 4: per-path env layout when the
        //     ownership pass supplied modes for this closure and every
        //     captured root resolves cleanly. Falls back to per-name
        //     layout when the data is missing (e.g., `compile_to_ir`
        //     called without ownership) or any captured root has a
        //     projection step that can't be resolved (treated as a
        //     whole-root capture for that root inside the path layout
        //     builder).
        let path_layout = self.build_capture_path_layout(closure_span, &free_vars);

        // 2. Build the env struct type: { T0_cap, T1_cap, ... }.
        //    Use a dummy i8 when there are no captures so we always have
        //    a valid struct type.
        let env_field_types: Vec<BasicTypeEnum<'ctx>> = if let Some(layout) = path_layout.as_ref() {
            if layout.slot_tys.is_empty() {
                vec![self.context.i8_type().into()]
            } else {
                layout.slot_tys.clone()
            }
        } else if free_vars.is_empty() {
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

        if let Some(layout) = path_layout.as_ref() {
            // Per-path unpack: one env slot per captured CapturePath.
            // For whole-root entries the slot holds the root value as-is;
            // for path-precise entries we allocate a root-typed alloca
            // and stitch each leaf into its GEP chain, then register the
            // root alloca in `self.variables` so the body's `u.f` reads
            // walk it normally.
            let env_struct = env_val.into_struct_value();
            for (root_name, plan) in &layout.root_plans {
                if let Some(slot_idx) = plan.whole_root_slot {
                    let field_val = self
                        .builder
                        .build_extract_value(env_struct, slot_idx as u32, root_name)
                        .unwrap();
                    let alloca = self.create_entry_alloca(closure_fn, root_name, plan.root_ty);
                    self.builder.build_store(alloca, field_val).unwrap();
                    self.variables.insert(
                        root_name.clone(),
                        VarSlot {
                            ptr: alloca,
                            ty: plan.root_ty,
                        },
                    );
                } else {
                    // Stitch: allocate the root, write each captured leaf
                    // into its GEP chain. Other leaves stay undef — the
                    // ownership pass guarantees the body never reads them.
                    let alloca = self.create_entry_alloca(closure_fn, root_name, plan.root_ty);
                    for (slot_idx, gep_chain, leaf_ty) in &plan.sub_slots {
                        let leaf_val = self
                            .builder
                            .build_extract_value(
                                env_struct,
                                *slot_idx as u32,
                                &format!("{}.cap", root_name),
                            )
                            .unwrap();
                        let leaf_ptr = self.gep_root_chain(plan.root_ty, alloca, gep_chain);
                        self.builder.build_store(leaf_ptr, leaf_val).unwrap();
                        let _ = leaf_ty; // typed read at capture site; store inherits type from value.
                    }
                    self.variables.insert(
                        root_name.clone(),
                        VarSlot {
                            ptr: alloca,
                            ty: plan.root_ty,
                        },
                    );
                }
                if let Some(type_name) = &plan.type_name {
                    self.var_type_names
                        .insert(root_name.clone(), type_name.clone());
                }
            }
        } else if !free_vars.is_empty() {
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
        if let Some(layout) = path_layout.as_ref() {
            // Per-path capture: for each env slot, walk the source root's
            // GEP chain and store the leaf value into the slot.
            if !layout.slot_sources.is_empty() {
                let mut env_agg = env_struct_ty.get_undef();
                for (i, (root, gep_chain)) in layout.slot_sources.iter().enumerate() {
                    let slot = self.variables[root];
                    let val = if gep_chain.is_empty() {
                        // Whole-root: load the root binding directly.
                        self.builder.build_load(slot.ty, slot.ptr, root).unwrap()
                    } else {
                        // Path-precise: GEP into the root's alloca, load
                        // the leaf. `slot.ptr` is the alloca holding the
                        // root struct value (root captures gated to
                        // non-RC, non-ref-param roots in
                        // `build_capture_path_layout` so this is always
                        // a direct struct alloca).
                        let leaf_ptr = self.gep_root_chain(slot.ty, slot.ptr, gep_chain);
                        let leaf_ty = self.leaf_type_for_chain(slot.ty, gep_chain);
                        self.builder
                            .build_load(leaf_ty, leaf_ptr, &format!("{}.cap.read", root))
                            .unwrap()
                    };
                    env_agg = self
                        .builder
                        .build_insert_value(env_agg, val, i as u32, "__env_field")
                        .unwrap()
                        .into_struct_value();
                }
                self.builder.build_store(env_alloca, env_agg).unwrap();
            }
        } else if !free_vars.is_empty() {
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
    pub(super) fn compile_closure_call(
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
    pub(super) fn infer_closure_return_type(
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

    // ── Disjoint-capture slice 4 helpers ───────────────────────────

    /// Build a per-path env layout for the closure at `closure_span`.
    /// Returns `None` when the ownership pass did not supply path-mode
    /// data for this closure (caller falls back to per-name layout).
    /// Roots that aren't safe for path-precise stitching (RC-fallback
    /// promoted, `ref`-param-shaped, or any projection step the resolver
    /// can't walk through `struct_field_names`) are collapsed to a single
    /// whole-root slot for that root — other roots in the same layout
    /// still get path-precise slots.
    fn build_capture_path_layout(
        &self,
        closure_span: &Span,
        free_vars: &[String],
    ) -> Option<CapturePathLayout<'ctx>> {
        let key = SpanKey::from_span(closure_span);
        let path_modes = self.closure_capture_paths.get(&key)?;

        // Group paths by root, preserving the slice-2 list order so
        // multiple paths under the same root keep deterministic ordering.
        let mut roots_in_order: Vec<String> = Vec::new();
        let mut by_root: HashMap<String, Vec<&CapturePath>> = HashMap::new();
        for (path, _mode) in path_modes {
            if !self.variables.contains_key(path.root.as_str()) {
                // Path references a binding the codegen scope doesn't
                // know about (e.g. captured by a nested closure but
                // shadowed before reaching this point) — skip; the
                // legacy per-name walker mirrors the same filter.
                continue;
            }
            if !by_root.contains_key(&path.root) {
                roots_in_order.push(path.root.clone());
            }
            by_root.entry(path.root.clone()).or_default().push(path);
        }
        // The slice-2 path set is keyed off the closure's free-variable
        // scan, which records roots even when the body only reaches them
        // through stopping constructs. Cross-check with `free_vars` so
        // any root the per-name walker found but slice 2 missed (and
        // vice-versa) doesn't silently drop from the env — fall back to
        // per-name layout if the two sets disagree.
        let path_root_set: HashSet<&String> = by_root.keys().collect();
        let free_var_set: HashSet<&String> = free_vars.iter().collect();
        if path_root_set != free_var_set {
            return None;
        }

        let mut slot_tys: Vec<BasicTypeEnum<'ctx>> = Vec::new();
        let mut slot_sources: Vec<(String, Vec<u32>)> = Vec::new();
        let mut root_plans: Vec<(String, RootUnpackPlan<'ctx>)> = Vec::new();

        for root in roots_in_order {
            let slot = *self.variables.get(root.as_str())?;
            let type_name = self.var_type_names.get(root.as_str()).cloned();
            let paths = by_root.get(&root).unwrap();

            // Conservative force-whole-root triggers: RC-fallback root
            // (slot.ty is `ptr`, body field-access goes through the
            // heap-deref path), ref-param root (alloca holds a pointer,
            // not a struct value), or any path under this root has a
            // projection chain that can't be resolved through
            // `struct_field_names`.
            let force_whole_root = self.is_rc_fallback_binding(&root)
                || self.ref_params.contains_key(root.as_str())
                || paths.iter().any(|p| {
                    !p.projection.is_empty()
                        && self
                            .resolve_gep_chain(slot.ty, type_name.as_deref(), &p.projection)
                            .is_none()
                });

            let any_whole = paths.iter().any(|p| p.projection.is_empty());

            if force_whole_root || any_whole {
                // One whole-root slot for this root. Drop sub-paths —
                // the body walks the whole root and field reads work
                // through normal compile_field_access dispatch.
                let slot_idx = slot_tys.len();
                slot_tys.push(slot.ty);
                slot_sources.push((root.clone(), Vec::new()));
                root_plans.push((
                    root.clone(),
                    RootUnpackPlan {
                        root_ty: slot.ty,
                        type_name,
                        whole_root_slot: Some(slot_idx),
                        sub_slots: Vec::new(),
                    },
                ));
            } else {
                // Per-path: one slot per non-empty projection. The slice-2
                // set guarantees every path here has non-empty projection
                // (`any_whole` is false in this branch).
                let mut sub_slots: Vec<(usize, Vec<u32>, BasicTypeEnum<'ctx>)> = Vec::new();
                for p in paths {
                    let gep_chain = self
                        .resolve_gep_chain(slot.ty, type_name.as_deref(), &p.projection)
                        .unwrap();
                    let leaf_ty = self.leaf_type_for_chain(slot.ty, &gep_chain);
                    let slot_idx = slot_tys.len();
                    slot_tys.push(leaf_ty);
                    slot_sources.push((root.clone(), gep_chain.clone()));
                    sub_slots.push((slot_idx, gep_chain, leaf_ty));
                }
                root_plans.push((
                    root.clone(),
                    RootUnpackPlan {
                        root_ty: slot.ty,
                        type_name,
                        whole_root_slot: None,
                        sub_slots,
                    },
                ));
            }
        }

        Some(CapturePathLayout {
            slot_tys,
            slot_sources,
            root_plans,
        })
    }

    /// Walk a projection chain (root-to-leaf field names, possibly mixed
    /// with numeric tuple indices) into a sequence of LLVM struct GEP
    /// indices. Returns `None` if any step can't be resolved — the
    /// caller treats that root as a whole-root capture. `type_name` is
    /// the source-level type of the root, looked up in
    /// `struct_field_names` to translate field-name → index.
    fn resolve_gep_chain(
        &self,
        root_ty: BasicTypeEnum<'ctx>,
        type_name: Option<&str>,
        projection: &[String],
    ) -> Option<Vec<u32>> {
        let mut current_ty = root_ty;
        let mut current_type_name: Option<String> = type_name.map(|s| s.to_string());
        let mut chain: Vec<u32> = Vec::with_capacity(projection.len());
        for step in projection {
            let struct_ty = match current_ty {
                BasicTypeEnum::StructType(st) => st,
                _ => return None,
            };
            // Try struct-field-name → index lookup first.
            let idx = if let Some(name) = current_type_name.as_deref() {
                if let Some(names) = self.struct_field_names.get(name) {
                    names.iter().position(|f| f == step).map(|p| p as u32)
                } else {
                    None
                }
            } else {
                None
            };
            // Fall back to numeric tuple-index parse.
            let idx = idx.or_else(|| step.parse::<u32>().ok())?;
            // Advance the LLVM and source type-name pointers.
            current_ty = struct_ty.get_field_type_at_index(idx)?;
            current_type_name = current_type_name
                .as_deref()
                .and_then(|name| self.struct_field_type_names.get(name))
                .and_then(|tys| tys.get(idx as usize).cloned())
                .flatten();
            chain.push(idx);
        }
        Some(chain)
    }

    /// Resolve the LLVM type at the end of a GEP chain rooted at
    /// `root_ty`. Used by both the capture-site loader (to type the load
    /// from the source root) and the unpack-site stitcher (to type the
    /// store into the stitched root).
    fn leaf_type_for_chain(
        &self,
        root_ty: BasicTypeEnum<'ctx>,
        chain: &[u32],
    ) -> BasicTypeEnum<'ctx> {
        let mut current = root_ty;
        for &idx in chain {
            if let BasicTypeEnum::StructType(st) = current {
                current = st.get_field_type_at_index(idx).unwrap();
            } else {
                // Builder guarantees the chain is resolvable; this branch
                // is only reached if a non-struct sneaks in, which would
                // be a bug — return the i64 fallback rather than panic.
                return self.context.i64_type().into();
            }
        }
        current
    }

    /// GEP into a struct alloca via a chain of field indices. Used by
    /// both the capture site (to read a leaf from the outer-scope root)
    /// and the unpack site (to write a leaf into the stitched-back
    /// root). The chain is rooted at field index 0 conceptually — every
    /// `struct_gep` step walks down one level from the current pointer.
    fn gep_root_chain(
        &self,
        root_ty: BasicTypeEnum<'ctx>,
        root_ptr: inkwell::values::PointerValue<'ctx>,
        chain: &[u32],
    ) -> inkwell::values::PointerValue<'ctx> {
        let mut current_ptr = root_ptr;
        let mut current_ty = root_ty;
        for (i, &idx) in chain.iter().enumerate() {
            let struct_ty = match current_ty {
                BasicTypeEnum::StructType(st) => st,
                _ => return current_ptr,
            };
            current_ptr = self
                .builder
                .build_struct_gep(struct_ty, current_ptr, idx, &format!("cap.gep.{}", i))
                .unwrap();
            current_ty = struct_ty.get_field_type_at_index(idx).unwrap();
        }
        current_ptr
    }

    /// Collect the names of variables captured by a closure (free variables from outer scope).
    ///
    /// A variable is captured if:
    /// 1. It is referenced in `body`.
    /// 2. It is NOT one of the closure's own parameters.
    /// 3. It is NOT defined by a `let` inside the closure body.
    /// 4. It IS present in the current outer scope (`self.variables`).
    pub(super) fn collect_closure_free_vars(
        &self,
        params: &[ClosureParam],
        body: &Expr,
    ) -> Vec<String> {
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
    pub(super) fn refs_in_expr(
        &self,
        expr: &Expr,
        refs: &mut HashSet<String>,
        defs: &mut HashSet<String>,
    ) {
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

    pub(super) fn refs_in_block(
        &self,
        block: &Block,
        refs: &mut HashSet<String>,
        defs: &mut HashSet<String>,
    ) {
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
}
