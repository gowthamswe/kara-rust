#[cfg(test)]
mod once_function_carrier_tests {
    use super::super::*;

    fn fn_i32_to_i32() -> Type {
        Type::Function {
            params: vec![Type::Int(IntSize::I32)],
            return_type: Box::new(Type::Int(IntSize::I32)),
        }
    }

    fn once_fn_i32_to_i32() -> Type {
        Type::OnceFunction {
            params: vec![Type::Int(IntSize::I32)],
            return_type: Box::new(Type::Int(IntSize::I32)),
        }
    }

    #[test]
    fn type_display_prints_oncefn() {
        assert_eq!(type_display(&once_fn_i32_to_i32()), "OnceFn(i32) -> i32");
        assert_eq!(type_display(&fn_i32_to_i32()), "Fn(i32) -> i32");
    }

    #[test]
    fn type_display_oncefn_unit_return_omits_arrow() {
        let no_arg_unit = Type::OnceFunction {
            params: vec![],
            return_type: Box::new(Type::Unit),
        };
        assert_eq!(type_display(&no_arg_unit), "OnceFn()");
    }

    #[test]
    fn type_display_oncefn_multi_param() {
        let multi = Type::OnceFunction {
            params: vec![Type::Int(IntSize::I32), Type::Bool],
            return_type: Box::new(Type::Float(FloatSize::F64)),
        };
        assert_eq!(type_display(&multi), "OnceFn(i32, bool) -> f64");
    }

    #[test]
    fn types_compatible_oncefn_identity() {
        assert!(types_compatible(
            &once_fn_i32_to_i32(),
            &once_fn_i32_to_i32()
        ));
    }

    #[test]
    fn types_compatible_oncefn_rejects_fn_in_either_direction() {
        assert!(!types_compatible(&once_fn_i32_to_i32(), &fn_i32_to_i32()));
        assert!(!types_compatible(&fn_i32_to_i32(), &once_fn_i32_to_i32()));
    }

    #[test]
    fn types_compatible_oncefn_param_arity_mismatch() {
        let one = once_fn_i32_to_i32();
        let two = Type::OnceFunction {
            params: vec![Type::Int(IntSize::I32), Type::Int(IntSize::I32)],
            return_type: Box::new(Type::Int(IntSize::I32)),
        };
        assert!(!types_compatible(&one, &two));
    }

    #[test]
    fn numeric_trait_arms_reject_oncefn() {
        // The trait-bound queries (`type_supports_*`) live on `TypeChecker`, so
        // we build a minimal one against an empty parsed program. With no impls
        // registered, the function-shape arms (now extended with `OnceFunction`)
        // are the ones exercised — verifying we widened the catch-all "false"
        // patterns rather than silently letting `OnceFunction` fall through to
        // permissive arms.
        let parsed = crate::parse("");
        let resolved = crate::resolve(&parsed.program);
        let tc = TypeChecker::new(&parsed.program, &resolved);
        let oncefn = once_fn_i32_to_i32();
        assert!(!tc.type_supports_partial_eq(&oncefn));
        assert!(!tc.type_supports_eq(&oncefn));
        assert!(!tc.type_supports_hash(&oncefn));
        assert!(!tc.type_supports_ord(&oncefn));
        assert!(!tc.type_supports_display(&oncefn));
        assert!(!tc.type_supports_partial_ord(&oncefn));
    }

    #[test]
    fn substitute_type_params_preserves_once() {
        let t_to_t = Type::OnceFunction {
            params: vec![Type::TypeParam("T".to_string())],
            return_type: Box::new(Type::TypeParam("T".to_string())),
        };
        let mut subs = HashMap::new();
        subs.insert("T".to_string(), SubstValue::Type(Type::Bool));
        let resolved = substitute_type_params(&t_to_t, &subs);
        assert_eq!(
            resolved,
            Type::OnceFunction {
                params: vec![Type::Bool],
                return_type: Box::new(Type::Bool),
            }
        );
    }

    #[test]
    fn contains_type_param_handles_oncefn() {
        let with_param = Type::OnceFunction {
            params: vec![Type::TypeParam("T".to_string())],
            return_type: Box::new(Type::Int(IntSize::I32)),
        };
        assert!(contains_type_param(&with_param));

        let no_param = once_fn_i32_to_i32();
        assert!(!contains_type_param(&no_param));
    }

    // ── Type::Shared / Type::Rc / Type::Arc variants ──

    #[test]
    fn test_type_display_shared_rc_arc_variants() {
        let shared = Type::Shared("S".to_string());
        assert_eq!(type_display(&shared), "S");

        let rc_i64 = Type::Rc(Box::new(Type::Int(IntSize::I64)));
        assert_eq!(type_display(&rc_i64), "Rc[i64]");

        let arc_str = Type::Arc(Box::new(Type::Str));
        assert_eq!(type_display(&arc_str), "Arc[String]");
    }

    #[test]
    fn test_types_compatible_rc_not_assignable_to_arc() {
        let rc_i64 = Type::Rc(Box::new(Type::Int(IntSize::I64)));
        let arc_i64 = Type::Arc(Box::new(Type::Int(IntSize::I64)));
        assert!(!types_compatible(&rc_i64, &arc_i64));
        assert!(!types_compatible(&arc_i64, &rc_i64));

        // The legacy structural form `Type::Named { name: "Rc", … }` is
        // a different type now — variants are distinct, even though
        // sub-item 2 hasn't yet migrated callers to construct them.
        let legacy_rc = Type::Named {
            name: "Rc".to_string(),
            args: vec![Type::Int(IntSize::I64)],
        };
        assert!(!types_compatible(&rc_i64, &legacy_rc));
        assert!(!types_compatible(&legacy_rc, &rc_i64));
    }

    #[test]
    fn test_types_compatible_shared_struct_name_match() {
        let shared_s = Type::Shared("S".to_string());
        let shared_s2 = Type::Shared("S".to_string());
        assert!(types_compatible(&shared_s, &shared_s2));

        let shared_t = Type::Shared("T".to_string());
        assert!(!types_compatible(&shared_s, &shared_t));

        // Distinct from the legacy `Type::Named { name: "S", args: [] }`.
        let legacy_s = Type::Named {
            name: "S".to_string(),
            args: vec![],
        };
        assert!(!types_compatible(&shared_s, &legacy_s));
        assert!(!types_compatible(&legacy_s, &shared_s));
    }

    // ── lower_path_type produces Rc / Arc / Shared variants (sub-item 2) ──

    fn build_typechecker(src: &str) -> TypeChecker<'static> {
        // Leak the parsed/resolved data so the TypeChecker borrow is 'static
        // for the duration of the test — fine; the lifetime ends with the
        // test process.
        let parsed: &'static _ = Box::leak(Box::new(crate::parse(src)));
        let resolved: &'static _ = Box::leak(Box::new(crate::resolve(&parsed.program)));
        let mut tc = TypeChecker::new(&parsed.program, resolved);
        tc.build_type_env();
        tc
    }

    fn path_with_args(name: &str, args: Vec<crate::ast::TypeExpr>) -> crate::ast::PathExpr {
        use crate::ast::GenericArg;
        crate::ast::PathExpr {
            segments: vec![name.to_string()],
            generic_args: if args.is_empty() {
                None
            } else {
                Some(args.into_iter().map(GenericArg::Type).collect())
            },
            span: Span::default(),
        }
    }

    fn type_path(name: &str) -> crate::ast::TypeExpr {
        crate::ast::TypeExpr {
            kind: crate::ast::TypeKind::Path(path_with_args(name, vec![])),
            span: Span::default(),
        }
    }

    #[test]
    fn test_lower_rc_path_type_produces_rc_variant() {
        let mut tc = build_typechecker("");
        let path = path_with_args("Rc", vec![type_path("i64")]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(lowered, Type::Rc(Box::new(Type::Int(IntSize::I64))));
    }

    #[test]
    fn test_lower_arc_path_type_produces_arc_variant() {
        let mut tc = build_typechecker("");
        let path = path_with_args("Arc", vec![type_path("String")]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(lowered, Type::Arc(Box::new(Type::Str)));
    }

    #[test]
    fn test_lower_shared_struct_path_type_produces_shared_variant() {
        let mut tc = build_typechecker("shared struct S { val: i64 }");
        let path = path_with_args("S", vec![]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(lowered, Type::Shared("S".to_string()));
    }

    #[test]
    fn test_lower_nonshared_struct_path_type_stays_named() {
        // Cross-check: the shared-struct intercept must not fire for plain
        // structs — sub-item 2's behavior-preserving promise hinges on this.
        let mut tc = build_typechecker("struct P { val: i64 }");
        let path = path_with_args("P", vec![]);
        let lowered = tc.lower_path_type(&path, &[]);
        assert_eq!(
            lowered,
            Type::Named {
                name: "P".to_string(),
                args: vec![],
            }
        );
    }

    // ── Method resolution: receiver_for_method_lookup deref step (sub-item 3a) ──

    #[test]
    fn test_receiver_for_lookup_strips_ref_wrappers() {
        let foo = Type::Named {
            name: "Foo".to_string(),
            args: vec![],
        };
        // `ref Foo` and `mut ref Foo` deref to `Foo` per design.md
        // § Method Resolution Step 1 — same as before sub-item 3a.
        assert_eq!(
            receiver_for_method_lookup(&Type::Ref(Box::new(foo.clone()))),
            foo
        );
        assert_eq!(
            receiver_for_method_lookup(&Type::MutRef(Box::new(foo.clone()))),
            foo
        );
    }

    #[test]
    fn test_receiver_for_lookup_shared_lowers_to_named() {
        // `Type::Shared(S)` lowers to `Type::Named { name: "S", args: [] }`
        // so the candidate-list lookup feeds into the existing
        // user-defined-struct method-resolution path verbatim.
        let shared = Type::Shared("S".to_string());
        assert_eq!(
            receiver_for_method_lookup(&shared),
            Type::Named {
                name: "S".to_string(),
                args: vec![],
            }
        );
    }

    #[test]
    fn test_receiver_for_lookup_rc_arc_deref_to_inner() {
        // `Rc[Foo]` and `Arc[Foo]` strip the wrapper so the inner type's
        // methods become reachable. Args carry through (e.g.
        // `Rc[Vec[i64]]` → `Vec[i64]`).
        let foo = Type::Named {
            name: "Foo".to_string(),
            args: vec![],
        };
        assert_eq!(
            receiver_for_method_lookup(&Type::Rc(Box::new(foo.clone()))),
            foo
        );
        assert_eq!(
            receiver_for_method_lookup(&Type::Arc(Box::new(foo.clone()))),
            foo
        );

        let vec_i64 = Type::Named {
            name: "Vec".to_string(),
            args: vec![Type::Int(IntSize::I64)],
        };
        assert_eq!(
            receiver_for_method_lookup(&Type::Rc(Box::new(vec_i64.clone()))),
            vec_i64
        );
    }

    #[test]
    fn test_receiver_for_lookup_passthrough_for_other_types() {
        // No-op for types without an outer wrapper — TypeParam, primitive,
        // etc. — so the existing arms in `infer_method_call` (TypeParam
        // dispatch, fallthrough) still receive the original shape.
        let tp = Type::TypeParam("T".to_string());
        assert_eq!(receiver_for_method_lookup(&tp), tp);

        let prim = Type::Int(IntSize::I64);
        assert_eq!(receiver_for_method_lookup(&prim), prim);
    }
}

#[cfg(test)]
mod closure_once_callability_inference_tests {
    //! Round 12.44 (Step 2) — closure-expression once-callability
    //! inference at construction. Verifies the typechecker assigns
    //! `Type::OnceFunction` to closures whose body consumes a captured
    //! outer non-Copy binding, `Type::Function` otherwise (capture-free
    //! / read-only-capture / explicit `ref ||` / `mut ref ||` prefix).
    use super::super::*;

    /// Type-check `src`, then return the inferred type of the first
    /// `Function` or `OnceFunction` value in `expr_types` — i.e., the
    /// closure expression's recorded type. Closure expressions are the
    /// only places these variants appear in user programs (no surface
    /// `Fn(...)` / `OnceFn(...)` annotation lower path yet).
    fn first_closure_type(src: &str) -> Type {
        let parsed = crate::parse(src);
        let resolved = crate::resolve(&parsed.program);
        let tc = crate::typecheck(&parsed.program, &resolved);
        for ty in tc.expr_types.values() {
            if matches!(ty, Type::Function { .. } | Type::OnceFunction { .. }) {
                return ty.clone();
            }
        }
        panic!(
            "expected a Function/OnceFunction-typed closure expression in `expr_types`; \
             expr_types: {:?}",
            tc.expr_types
        );
    }

    #[test]
    fn closure_captures_and_consumes_infers_oncefn() {
        // `apply(cfg)`: `apply` takes owned non-Copy `Cfg`, so the
        // capture-position `cfg` is in Consuming mode → outer non-Copy
        // → closure is once-callable → `Type::OnceFunction`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = || apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::OnceFunction { .. }),
            "expected OnceFunction; got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_only_reads_capture_infers_fn() {
        // `cfg.name` is a FieldAccess walked in Reading mode at the
        // closure body's top level → the `cfg` identifier-leaf inside
        // is Reading → no consume → repeatable closure → `Function`.
        let src = "struct Cfg { name: i64 }\n\
                   fn make(cfg: Cfg) -> i64 {\n\
                       let h = || cfg.name;\n\
                       cfg.name\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function; got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn capture_free_closure_infers_fn() {
        // No outer references → no captures → trivially repeatable.
        let src = "fn main() {\n\
                       let h = || 42;\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function; got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn explicit_ref_prefix_forces_fn_even_when_body_would_consume() {
        // `ref ||` declares the captures as borrows; the round-12.6
        // repeatable-closure rule says these are NOT once-callable
        // regardless of body shape. The body here would otherwise look
        // consume-y to the walker (call with own param slot), but the
        // explicit prefix short-circuits the walk to `Function`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = ref || apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (ref prefix forces repeatable); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn explicit_mut_ref_prefix_forces_fn_even_when_body_would_consume() {
        // `mut ref ||` declares the captures as mutable borrows; same
        // round-12.6 rule. Body shape that would otherwise infer
        // OnceFn must produce `Function` here.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = mut ref || apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (mut ref prefix forces repeatable); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_consuming_copy_capture_infers_fn() {
        // `apply(n)` where `n` is `i64` (Copy). Even though `n` is in
        // Consuming mode, Copy types never trigger once-callability —
        // a Copy capture is duplicated, not moved, on every invocation.
        let src = "fn apply(x: i64) { }\n\
                   fn make() {\n\
                       let n: i64 = 42;\n\
                       let h = || apply(n);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (Copy capture, not once-callable); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_param_shadowing_outer_non_copy_does_not_capture() {
        // The closure's `cfg` parameter shadows the outer `cfg`, so
        // the body's `apply(cfg)` consumes the PARAM, not a capture.
        // No outer non-Copy is consumed → repeatable.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = |cfg: Cfg| apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (param shadows outer); got {}",
            type_display(&ty)
        );
    }

    #[test]
    fn closure_body_local_let_shadows_outer_non_copy_capture() {
        // A `let cfg = ...` inside the closure body shadows the outer
        // `cfg`. Subsequent `apply(cfg)` inside the body consumes the
        // body-local, not the capture → repeatable.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn make(cfg: Cfg) {\n\
                       let h = || {\n\
                           let cfg = Cfg { name: 7 };\n\
                           apply(cfg)\n\
                       };\n\
                       let _ = h;\n\
                   }";
        let ty = first_closure_type(src);
        assert!(
            matches!(ty, Type::Function { .. }),
            "expected Function (body let shadows capture); got {}",
            type_display(&ty)
        );
    }
}

#[cfg(test)]
mod once_fn_slot_rejection_tests {
    //! Round 12.45 (Step 3) — caller-side rejection of `OnceFn` arguments at
    //! `Fn(...)` and `ref Fn(...)` parameter slots. The slot promises
    //! repeatable invocation; an `OnceFn` value violates that promise. The
    //! diagnostic kind is `OnceFnIntoFnSlot` (E0235); when the argument is
    //! a closure literal that the typechecker has already classified as
    //! once-callable (Step 2), the message also names the consumed capture.
    use super::super::*;

    fn typecheck_src(src: &str) -> TypeCheckResult {
        let parsed = crate::parse(src);
        let resolved = crate::resolve(&parsed.program);
        crate::typecheck(&parsed.program, &resolved)
    }

    fn errors_of_kind(result: &TypeCheckResult, kind: &TypeErrorKind) -> Vec<TypeError> {
        result
            .errors
            .iter()
            .filter(|e| std::mem::discriminant(&e.kind) == std::mem::discriminant(kind))
            .cloned()
            .collect()
    }

    #[test]
    fn own_fn_slot_rejects_oncefn_closure_literal() {
        // `take(f: Fn())`: owned `Fn()` slot — promises the callee can call
        // `f` any number of times. The closure `|| apply(cfg)` is once-
        // callable (consumes captured non-Copy `cfg`). Step 3 must reject.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one OnceFnIntoFnSlot error; all errors: {:?}",
            result.errors
        );
        assert!(
            hits[0].message.contains("once-callable"),
            "expected message to mention 'once-callable'; got '{}'",
            hits[0].message
        );
        assert!(
            hits[0].message.contains("'cfg'") || hits[0].message.contains("captured binding"),
            "expected message to name the consumed capture 'cfg'; got '{}'",
            hits[0].message
        );
    }

    #[test]
    fn own_fn_slot_accepts_repeatable_closure() {
        // Capture-free closure → `Type::Function` → fits an own `Fn()` slot.
        let src = "fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       take(|| { });\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot error for repeatable closure; got: {:?}",
            hits
        );
    }

    #[test]
    fn own_fn_slot_accepts_explicit_ref_prefix_closure_via_binding() {
        // `ref ||` forces repeatable per round 12.6 even when the body
        // would otherwise look consume-y. `ref` is not legal at call
        // sites (parser rejects), so the closure must be let-bound first;
        // the binding gets `Type::Function`, which the own-Fn slot accepts.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let h = ref || apply(cfg);\n\
                       take(h);\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot error for ref-prefix-bound closure; got: {:?}",
            hits
        );
    }

    #[test]
    fn ref_fn_slot_rejects_oncefn_closure_literal() {
        // `ref Fn()` slot — same once-callability constraint; the callee
        // can dispatch through the ref repeatedly, so a once-callable
        // closure value must be rejected. The closure literal types as
        // bare `OnceFn()`, the slot is `ref Fn()`; the unwrapped shape
        // (Fn vs OnceFn) flags the once-callability violation rather than
        // the ref-vs-bare regular mismatch.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: ref Fn()) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected OnceFnIntoFnSlot error for ref-Fn slot rejection; all errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn cross_call_oncefn_through_fn_slot_rejects_at_inner_site() {
        // Inner `inner(cb: Fn())` — a Fn slot. Outer `forward(cb: Fn())`
        // forwards `cb` to inner. Caller passes a once-callable closure to
        // forward — already a Step-3 violation at the OUTER call site. The
        // test pins that the diagnostic kind fires at the user-visible
        // call site (forward(...)), regardless of how many forwarding
        // hops the typechecker would chase.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn inner(cb: Fn()) { cb() }\n\
                   fn forward(cb: Fn()) { inner(cb) }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       forward(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected at least one OnceFnIntoFnSlot error in cross-call forwarding; \
             all errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn method_call_fn_slot_rejects_oncefn_closure_literal() {
        // Method-call slot rejection — the same `Fn()` rule applies to
        // method parameter slots, since the dispatch site routes through
        // `check_call_args_with_substitution` and ultimately
        // `check_assignable`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   struct Runner { }\n\
                   impl Runner {\n\
                       fn drive(self, f: Fn()) { f() }\n\
                   }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let r = Runner { };\n\
                       r.drive(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected OnceFnIntoFnSlot error for method-call Fn-slot rejection; \
             all errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn no_typemismatch_double_report_when_oncefn_slot_violation_fires() {
        // The OnceFnIntoFnSlot kind replaces the generic TypeMismatch for
        // this specific shape — emitting both would double-report. The
        // single-error invariant is what makes the new diagnostic useful;
        // this test pins it.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let once_hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        let mismatch_hits = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert_eq!(once_hits.len(), 1);
        // The TypeMismatch kind may still appear for unrelated reasons,
        // but not for the same span as the OnceFn slot violation.
        let once_span = once_hits[0].span.clone();
        for tm in &mismatch_hits {
            assert!(
                tm.span != once_span,
                "TypeMismatch double-reported at OnceFn slot violation span: {:?}",
                tm
            );
        }
    }

    #[test]
    fn diagnostic_includes_three_concrete_fix_hints() {
        // Round 12.47 (Step 5a) — diagnostic polish. The OnceFnIntoFnSlot
        // message must offer the three concrete fixes documented in the
        // implementation checklist: clone the consumed capture, restructure
        // to keep the closure local, or change the slot type to `OnceFn`.
        // Pin each phrase so future edits to the message body don't silently
        // drop a fix hint.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn take(f: Fn()) { f() }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       take(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert_eq!(hits.len(), 1, "all errors: {:?}", result.errors);
        let msg = &hits[0].message;
        assert!(
            msg.contains("clone the captured value"),
            "missing clone hint; got '{}'",
            msg
        );
        assert!(
            msg.contains("invoke the closure locally") || msg.contains("restructure"),
            "missing restructure-locally hint; got '{}'",
            msg
        );
        assert!(
            msg.contains("`OnceFn(...)`") || msg.contains("OnceFn(...)"),
            "missing OnceFn-slot-change hint; got '{}'",
            msg
        );
    }
}

#[cfg(test)]
mod once_fn_container_slot_tests {
    //! Round 12.46 (Step 4) — once-callability rejection at container element
    //! slots, plus surface `OnceFn(...)` annotation acceptance and for-loop
    //! iteration parity over `Vec[Fn]` and `Vec[OnceFn]`. The active rejection
    //! is at the *insert* (`.push`); iteration falls out for free because
    //! `for f in vec` types `f` as the element type, and Step 1's `Call`
    //! dispatch already accepts both `Function` and `OnceFunction` callees.
    use super::super::*;

    fn typecheck_src(src: &str) -> TypeCheckResult {
        let parsed = crate::parse(src);
        let resolved = crate::resolve(&parsed.program);
        crate::typecheck(&parsed.program, &resolved)
    }

    fn errors_of_kind(result: &TypeCheckResult, kind: &TypeErrorKind) -> Vec<TypeError> {
        result
            .errors
            .iter()
            .filter(|e| std::mem::discriminant(&e.kind) == std::mem::discriminant(kind))
            .cloned()
            .collect()
    }

    #[test]
    fn vec_fn_push_rejects_oncefn_closure_literal() {
        // `Vec[Fn()]` element slot — pushing a once-callable closure must
        // reject at the call site of `.push` because the slot promises
        // repeatable invocation. Routes through the new Vec.push slot
        // dispatch into `check_assignable`, which fires `OnceFnIntoFnSlot`
        // (E0235) via Step 3's logic.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one OnceFnIntoFnSlot error at Vec[Fn].push site; \
             all errors: {:?}",
            result.errors
        );
        assert!(
            hits[0].message.contains("once-callable"),
            "expected 'once-callable' in message; got '{}'",
            hits[0].message
        );
        assert!(
            hits[0].message.contains("'cfg'") || hits[0].message.contains("captured binding"),
            "expected consumed-capture name 'cfg' in message; got '{}'",
            hits[0].message
        );
    }

    #[test]
    fn vec_fn_push_accepts_repeatable_closure() {
        // Capture-free closure → `Type::Function` → fits `Vec[Fn()]` element.
        let src = "fn main() {\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(|| { });\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot for repeatable closure push; got: {:?}",
            hits
        );
        // Also confirm no TypeMismatch crept in for the push arg.
        let mismatch = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert!(
            mismatch.is_empty(),
            "expected no TypeMismatch errors; got: {:?}",
            mismatch
        );
    }

    #[test]
    fn vec_oncefn_push_accepts_once_callable_closure() {
        // Surface `OnceFn(...)` annotation (round 12.46 Step 4) lets the
        // user opt into a Vec whose element slot accepts once-callable
        // closures. Pushing a closure that consumes a captured non-Copy
        // binding now fits the slot — `OnceFunction` ⇄ `OnceFunction`.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let mut v: Vec[OnceFn()] = Vec.new();\n\
                       v.push(|| apply(cfg));\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            hits.is_empty(),
            "expected no OnceFnIntoFnSlot for OnceFn-into-OnceFn slot; got: {:?}",
            hits
        );
        let mismatch = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert!(
            mismatch.is_empty(),
            "expected no TypeMismatch for OnceFn-into-OnceFn slot; got: {:?}",
            mismatch
        );
    }

    #[test]
    fn vec_oncefn_slot_accepts_function_closure_via_subsumption() {
        // Item 131 sub-step 3 (bidirectional subsumption): a Function-typed
        // closure (repeatable) flows into a Vec[OnceFn] slot. Fn is a subtype
        // of OnceFn — a repeatable callable trivially satisfies the
        // callable-once contract. `is_subtype(OnceFunction, Function)` returns
        // true at check_assignable, so neither TypeMismatch nor
        // OnceFnIntoFnSlot fires.
        //
        // Pre-sub-step-3 this fired TypeMismatch (the old test name was
        // `vec_oncefn_annotation_lowers_to_once_function_type` — which
        // observed the rejection as a side effect of the symmetric
        // types_compatible cross-pair rejection). The annotation is still
        // correctly lowered to OnceFunction; what changed is that the upward
        // direction is now admitted at the slot.
        let src = "fn main() {\n\
                       let mut v: Vec[OnceFn() -> i64] = Vec.new();\n\
                       v.push(|| 7);\n\
                   }";
        let result = typecheck_src(src);
        let mismatch = errors_of_kind(&result, &TypeErrorKind::TypeMismatch);
        assert!(
            mismatch.is_empty(),
            "Function → OnceFn slot is admitted by sub-step 3 subsumption; \
             expected no TypeMismatch but got: {:?}",
            mismatch
        );
        let once_hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            once_hits.is_empty(),
            "OnceFnIntoFnSlot must not fire for Function → OnceFn (only the \
             reverse direction is the round-12.45 case); got: {:?}",
            once_hits
        );
    }

    #[test]
    fn for_loop_over_vec_fn_invokes_repeatedly() {
        // Iteration over `Vec[Fn()]` yields `f: Fn()` per iteration. The
        // body's `f()` call dispatches against `Type::Function`, which
        // Step 1 made first-class for callee dispatch. No OnceFn ever
        // appears in this path because the slot at insert time was Fn.
        let src = "fn main() {\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(|| { });\n\
                       v.push(|| { });\n\
                       for f in v {\n\
                           f();\n\
                       }\n\
                   }";
        let result = typecheck_src(src);
        assert!(
            result.errors.is_empty(),
            "expected clean typecheck; got errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn for_loop_over_vec_oncefn_invokes_each_element() {
        // Iteration over `Vec[OnceFn()]` yields `f: OnceFn()` per
        // iteration. The typechecker's Call dispatch matches
        // `Function | OnceFunction`, so the body's `f()` succeeds. Each
        // iteration owns its element (move semantics) so calling once is
        // fine; the body invokes f exactly once.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg1 = Cfg { name: 1 };\n\
                       let cfg2 = Cfg { name: 2 };\n\
                       let mut v: Vec[OnceFn()] = Vec.new();\n\
                       v.push(|| apply(cfg1));\n\
                       v.push(|| apply(cfg2));\n\
                       for f in v {\n\
                           f();\n\
                       }\n\
                   }";
        let result = typecheck_src(src);
        assert!(
            result.errors.is_empty(),
            "expected clean typecheck for Vec[OnceFn] iter+invoke; got: {:?}",
            result.errors
        );
    }

    #[test]
    fn vec_fn_push_oncefn_through_intermediate_binding_still_rejects() {
        // The closure is bound to a let first, then pushed. The let's
        // binding type infers to OnceFunction (Step 2) and the push slot
        // check sees OnceFunction → Function and fires E0235. This pins
        // that the Vec.push slot check does not depend on the argument
        // being a closure literal — any once-callable value flowing into
        // the slot rejects.
        let src = "struct Cfg { name: i64 }\n\
                   fn apply(c: Cfg) { }\n\
                   fn main() {\n\
                       let cfg = Cfg { name: 7 };\n\
                       let h = || apply(cfg);\n\
                       let mut v: Vec[Fn()] = Vec.new();\n\
                       v.push(h);\n\
                   }";
        let result = typecheck_src(src);
        let hits = errors_of_kind(&result, &TypeErrorKind::OnceFnIntoFnSlot);
        assert!(
            !hits.is_empty(),
            "expected OnceFnIntoFnSlot when pushing a let-bound once-callable \
             closure into Vec[Fn]; all errors: {:?}",
            result.errors
        );
    }
}
