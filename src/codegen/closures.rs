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
use std::collections::{HashMap, HashSet};

use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, StructType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum};
use inkwell::AddressSpace;

use super::state::VarSlot;

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
    pub(super) fn compile_closure(
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
