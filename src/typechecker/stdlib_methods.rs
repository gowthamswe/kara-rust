//! Method-inference dispatch for stdlib types.
//!
//! Houses the per-stdlib-type method-resolution arms invoked from
//! `infer_method_call` (in typechecker.rs) when the receiver is a
//! known stdlib shape: `String`, `Slice[T]` / `Vec[T]` / `Array[T,N]`,
//! `Map[K,V]`, `Map.Entry[K,V]`, `SortedSet[T]`, `Set[T]`, every
//! `Iterator` adapter, `Regex`, the `http.Client` / `http.Response` /
//! `http.Error` triple, and `Sender[T]` / `Receiver[T]` channel ends.
//!
//! Each `infer_X_method` arm returns the inferred return `Type`
//! (synthesizing from receiver type-args plus argument types), records
//! `method_callee_types` for the codegen lowering pass, and emits
//! per-method diagnostics for arity / type mismatches.

use crate::ast::*;
use crate::token::Span;

use super::types::{type_display, IntSize, Type};
use super::TypeErrorKind;

impl<'a> super::TypeChecker<'a> {
    /// Validate a `sort_by` / `sorted_by` comparator argument against the
    /// `Fn(elem, elem) -> Ordering` shape. Pushes the expected function
    /// type down into the closure via `check_expr` so closure-parameter
    /// types are inferred from the element type rather than left as fresh
    /// metavars (today's silent-fall-through path) — a wrong-shape
    /// comparator (`xs.sort_by(|a| a)`, `xs.sort_by(|a, b| a)`, or a
    /// `Fn` value of the wrong arity / return type) now produces a
    /// TypeMismatch at the closure expression instead of runtime-panicking
    /// when the interpreter invokes it with two args / consumes the
    /// non-Ordering return.
    pub(super) fn check_sort_comparator(
        &mut self,
        elem: &Type,
        arg: &CallArg,
        method: &str,
        span: &Span,
    ) {
        let expected = Type::Function {
            params: vec![elem.clone(), elem.clone()],
            return_type: Box::new(Type::Named {
                name: "Ordering".to_string(),
                args: Vec::new(),
            }),
        };
        let _ = (method, span); // method / span carried for future diagnostic refinement
        self.check_expr(&arg.value, &expected);
    }

    /// Validate a `sort_by_key` / `sorted_by_key` key-function argument
    /// against `Fn(elem) -> K` and verify the inferred `K` satisfies `Ord`.
    /// `K` is a fresh metavar pushed down through `check_expr`; once the
    /// closure body unifies it to a concrete type, an Ord bound check
    /// rejects key types (raw floats, function values, etc.) that lack
    /// total ordering. Generic `K` (still a TypeVar after resolution)
    /// flows through without an Ord assertion — the bound will be
    /// rechecked at monomorphization.
    pub(super) fn check_sort_key_closure(
        &mut self,
        elem: &Type,
        arg: &CallArg,
        method: &str,
        span: &Span,
    ) {
        // `Fn(elem) -> K` where K is a placeholder the closure body solves.
        // Use `Type::TypeParam` not `Type::TypeVar`: `types_compatible` treats
        // TypeParam permissively so the `check_assignable` step doesn't fire
        // a spurious "expected K, found <body_ty>" diagnostic. After
        // `check_expr` returns the inferred closure type, read the resolved
        // body type out of the Function shape and check the Ord bound on it.
        // Pattern lifted from `Iterator.map`'s pushdown at infer_iterator_method.
        let placeholder = Type::TypeParam("__sort_by_key_K".to_string());
        let expected = Type::Function {
            params: vec![elem.clone()],
            return_type: Box::new(placeholder),
        };
        let actual_ty = self.check_expr(&arg.value, &expected);
        let resolved_k = match actual_ty {
            Type::Function { return_type, .. } | Type::OnceFunction { return_type, .. } => {
                *return_type
            }
            _ => return,
        };
        if !matches!(
            resolved_k,
            Type::TypeParam(_) | Type::TypeVar(_) | Type::Error
        ) && !self.type_supports_ord(&resolved_k)
        {
            self.type_error(
                format!(
                    "{}: key closure return type '{}' does not implement Ord",
                    method,
                    type_display(&resolved_k)
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
    }

    /// Infer the return type of a method call on `String` (`Type::Str`).
    /// Called from `infer_method_call` when the object type is `Type::Str`.
    pub(super) fn infer_str_method(&mut self, method: &str, args: &[CallArg], span: &Span) -> Type {
        match method {
            "sorted" => {
                if !args.is_empty() {
                    self.type_error(
                        "'sorted' takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Str
            }
            "sorted_by" => {
                // sorted_by(cmp: Fn(Char, Char) -> Ordering) -> String
                if args.len() != 1 {
                    self.type_error(
                        format!("'sorted_by' expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    self.check_sort_comparator(&Type::Char, &args[0], "sorted_by", span);
                }
                Type::Str
            }
            "chars" => {
                // chars() -> Iterator[char]. Peer of design.md § Character type
                // (line 2299): `for c in s` and `s.chars()` both iterate the
                // string's Unicode scalar values. Tree-walk interpreter
                // implements the same in eval_method_call's "chars" arm; a
                // for-loop on a bare String falls back through the same path.
                if !args.is_empty() {
                    self.type_error(
                        "'chars' takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Char],
                }
            }
            // Unknown string method — typo-suggestion diagnostic if close to
            // a known name, silent otherwise (`len`, `contains`, `is_empty`,
            // … are runtime-only and not yet wired through the typechecker).
            // Flip to always-error once enumeration catches up to the
            // interpreter's String surface — design.md § Method Resolution
            // Step 7.
            _ => self.require_known_method(
                "String",
                method,
                &["chars", "sorted", "sorted_by"],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on a `Slice[T]` or `mut Slice[T]`.
    /// Handles the full read-only surface and the mutation-only surface for
    /// `mut Slice[T]`. Called from `infer_method_call` when the object type is
    /// `Type::Slice`.
    pub(super) fn infer_slice_method(
        &mut self,
        element: &Type,
        mutable: bool,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let elem = element.clone();
        let option_elem = Type::Named {
            name: "Option".to_string(),
            args: vec![elem.clone()],
        };
        let option_i64 = Type::Named {
            name: "Option".to_string(),
            args: vec![Type::Int(IntSize::I64)],
        };
        let slice_elem = Type::Slice {
            element: Box::new(elem.clone()),
            mutable: false,
        };
        let vec_slice = Type::Named {
            name: "Vec".to_string(),
            args: vec![slice_elem.clone()],
        };

        match method {
            // Read-only methods (available on both Slice[T] and mut Slice[T])
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "Slice.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "Slice.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "first" | "last" => {
                if !args.is_empty() {
                    self.type_error(
                        format!("Slice.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                option_elem
            }
            "get" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                option_elem
            }
            "contains" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "binary_search" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                option_i64
            }
            "split_at" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                Type::Tuple(vec![slice_elem.clone(), slice_elem])
            }
            "chunks" | "windows" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                vec_slice
            }
            // Mutation methods (require mut Slice[T])
            "sort" | "reverse" => {
                if !mutable {
                    self.type_error(
                        format!(
                            "Slice.{}() requires a mutable slice (`mut Slice[T]`)",
                            method
                        ),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if !args.is_empty() {
                    self.type_error(
                        format!("Slice.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Unit
            }
            "sort_by" => {
                if !mutable {
                    self.type_error(
                        "Slice.sort_by() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Slice.sort_by() expects 1 argument (comparator closure), found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    self.check_sort_comparator(&elem, &args[0], "sort_by", span);
                }
                Type::Unit
            }
            "sort_by_key" => {
                if !mutable {
                    self.type_error(
                        "Slice.sort_by_key() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Slice.sort_by_key() expects 1 argument (key closure), found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    self.check_sort_key_closure(&elem, &args[0], "sort_by_key", span);
                }
                Type::Unit
            }
            "fill" => {
                if !mutable {
                    self.type_error(
                        "Slice.fill() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Unit
            }
            "swap" => {
                if !mutable {
                    self.type_error(
                        "Slice.swap() requires a mutable slice (`mut Slice[T]`)".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&Type::Int(IntSize::I64), &at, arg.value.span.clone());
                }
                Type::Unit
            }
            // `Slice[T]` IS `Iterator[T]` — `.iter()` / `.into_iter()` route
            // through the same Iterator dispatch as `Vec.iter()` so chained
            // adaptors (`s.iter().map(f).filter(p).collect()`) compose. The
            // receiver-type match in `infer_method_call` lands here before
            // the generic `iter` / `into_iter` arm, so the registration
            // duplicates that arm shape (no-args, returns `Iterator[T]`).
            "iter" | "into_iter" => {
                if !args.is_empty() {
                    self.type_error(
                        format!("Slice.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![elem],
                }
            }
            _ => self.require_known_method(
                "Slice",
                method,
                &[
                    "binary_search",
                    "chunks",
                    "contains",
                    "fill",
                    "first",
                    "get",
                    "into_iter",
                    "is_empty",
                    "iter",
                    "last",
                    "len",
                    "reverse",
                    "sort",
                    "sort_by",
                    "sort_by_key",
                    "split_at",
                    "swap",
                    "windows",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Map[K, V]`.
    /// `key` is K, `val` is V from the receiver's type arguments.
    pub(super) fn infer_map_method(
        &mut self,
        key: &Type,
        val: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        // K: Hash + Eq bound — Map requires the key type to be hashable and equality-comparable.
        if !self.type_supports_hash(key) || !self.type_supports_eq(key) {
            let missing = if !self.type_supports_hash(key) && !self.type_supports_eq(key) {
                "Hash + Eq"
            } else if !self.type_supports_hash(key) {
                "Hash"
            } else {
                "Eq"
            };
            self.type_error(
                format!(
                    "Map[{}, ...]: key type does not implement `{}`; \
                     only hashable equality-comparable types (integers, bool, char, String, \
                     or structs/enums with `#[derive(Hash, Eq)]`) can be Map keys",
                    type_display(key),
                    missing
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
        let k = key.clone();
        let v = val.clone();
        let option_v = Type::Named {
            name: "Option".to_string(),
            args: vec![v.clone()],
        };
        let vec_k = Type::Named {
            name: "Vec".to_string(),
            args: vec![k.clone()],
        };
        let vec_v = Type::Named {
            name: "Vec".to_string(),
            args: vec![v.clone()],
        };
        let tuple_kv = Type::Tuple(vec![k.clone(), v.clone()]);
        let vec_kv = Type::Named {
            name: "Vec".to_string(),
            args: vec![tuple_kv],
        };
        let map_kv = Type::Named {
            name: "Map".to_string(),
            args: vec![k.clone(), v.clone()],
        };

        match method {
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "contains_key" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&k, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "get" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&k, &at, arg.value.span.clone());
                }
                option_v
            }
            "get_or" => {
                if let Some(key_arg) = args.first() {
                    let kt = self.infer_expr(&key_arg.value);
                    self.check_assignable(&k, &kt, key_arg.value.span.clone());
                }
                if let Some(default_arg) = args.get(1) {
                    let dt = self.infer_expr(&default_arg.value);
                    self.check_assignable(&v, &dt, default_arg.value.span.clone());
                }
                v
            }
            "insert" => {
                if let Some(key_arg) = args.first() {
                    let kt = self.infer_expr(&key_arg.value);
                    self.check_assignable(&k, &kt, key_arg.value.span.clone());
                }
                if let Some(val_arg) = args.get(1) {
                    let vt = self.infer_expr(&val_arg.value);
                    self.check_assignable(&v, &vt, val_arg.value.span.clone());
                }
                option_v
            }
            "remove" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&k, &at, arg.value.span.clone());
                }
                option_v
            }
            "keys" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.keys() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                vec_k
            }
            "values" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.values() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                vec_v
            }
            "entries" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.entries() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                vec_kv
            }
            "merge" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&map_kv, &at, arg.value.span.clone());
                }
                map_kv
            }
            "clear" => {
                if !args.is_empty() {
                    self.type_error(
                        "Map.clear() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Unit
            }
            "entry" => {
                // `entry(key: K) -> Entry[K, V]` — view returned for the given
                // key, occupied or vacant. Drives the in-place insert-or-modify
                // chain (or_insert / or_insert_with / and_modify) via
                // `infer_entry_method`. See design.md § Entry[K, V].
                if args.len() != 1 {
                    self.type_error(
                        format!("Map.entry() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let kt = self.infer_expr(&args[0].value);
                    self.check_assignable(&k, &kt, args[0].value.span.clone());
                }
                Type::Named {
                    name: "Entry".to_string(),
                    args: vec![k.clone(), v.clone()],
                }
            }
            _ => self.require_known_method(
                "Map",
                method,
                &[
                    "clear",
                    "contains_key",
                    "entries",
                    "entry",
                    "get",
                    "get_or",
                    "insert",
                    "is_empty",
                    "keys",
                    "len",
                    "merge",
                    "remove",
                    "values",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Entry[K, V]`.
    /// Drives the chain produced by `Map.entry(k)` — `or_insert`,
    /// `or_insert_with`, `and_modify`. Effect polymorphism on the closure-
    /// taking forms is handled by the existing closure-effect-propagation
    /// pass in the effect checker; this layer just types the shape.
    pub(super) fn infer_entry_method(
        &mut self,
        key: &Type,
        val: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let v = val.clone();
        let mut_ref_v = Type::MutRef(Box::new(v.clone()));
        let entry_kv = Type::Named {
            name: "Entry".to_string(),
            args: vec![key.clone(), v.clone()],
        };
        match method {
            "or_insert" => {
                // `or_insert(default: V) -> mut ref V`. Returns a borrow into
                // the map's slot — fresh on Vacant (after writing default),
                // existing on Occupied.
                if args.len() != 1 {
                    self.type_error(
                        format!("Entry.or_insert() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let dt = self.infer_expr(&args[0].value);
                    self.check_assignable(&v, &dt, args[0].value.span.clone());
                }
                mut_ref_v
            }
            "or_insert_with" => {
                // `or_insert_with[with E](f: Fn() -> V with E) -> mut ref V
                // with E`. Closure invoked only on the Vacant arm; effect
                // propagation through `with E` is handled by the effect
                // checker reading the closure's effect set.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Entry.or_insert_with() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let f_ty = Type::Function {
                        params: vec![],
                        return_type: Box::new(v.clone()),
                    };
                    self.check_expr(&args[0].value, &f_ty);
                }
                mut_ref_v
            }
            "and_modify" => {
                // `and_modify[with E](f: Fn(mut ref V) with E) -> Entry[K, V]
                // with E`. Closure invoked only on Occupied; receives a
                // `mut ref V` to the existing slot. Returns self for
                // chaining (e.g. `.and_modify(...).or_insert(default)`).
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Entry.and_modify() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                } else {
                    let f_ty = Type::Function {
                        params: vec![mut_ref_v.clone()],
                        return_type: Box::new(Type::Unit),
                    };
                    self.check_expr(&args[0].value, &f_ty);
                }
                entry_kv
            }
            _ => self.require_known_method(
                "Entry",
                method,
                &["and_modify", "or_insert", "or_insert_with"],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `SortedSet[T]`.
    /// `element` is the resolved `T` from the receiver's type arguments.
    /// Called from `infer_method_call` when the object type is
    /// `Type::Named { name: "SortedSet", ... }`.
    pub(super) fn infer_sorted_set_method(
        &mut self,
        element: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        // T: Ord bound — SortedSet requires a total order on its element type.
        if !self.type_supports_ord(element) {
            self.type_error(
                format!(
                    "SortedSet[{}]: element type does not implement `Ord`; \
                     only types with a total order (integers, bool, char, String, \
                     or structs/enums with `#[derive(Ord)]`) can be SortedSet elements",
                    type_display(element)
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
        let elem = element.clone();
        let option_elem = Type::Named {
            name: "Option".to_string(),
            args: vec![elem.clone()],
        };
        let sorted_set_elem = Type::Named {
            name: "SortedSet".to_string(),
            args: vec![elem.clone()],
        };

        match method {
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "SortedSet.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "SortedSet.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "contains" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "insert" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "remove" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "min" | "max" => {
                if !args.is_empty() {
                    self.type_error(
                        format!("SortedSet.{}() takes no arguments", method),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                option_elem
            }
            "union" | "intersection" | "difference" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&sorted_set_elem, &at, arg.value.span.clone());
                }
                sorted_set_elem
            }
            _ => self.require_known_method(
                "SortedSet",
                method,
                &[
                    "contains",
                    "difference",
                    "insert",
                    "intersection",
                    "is_empty",
                    "len",
                    "max",
                    "min",
                    "remove",
                    "union",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Set[T: Hash + Eq]`.
    /// Hash set with O(1) average insert/remove/contains. Enforces the
    /// `T: Hash + Eq` bound the same way `Map[K, V]` checks `K: Hash + Eq`.
    pub(super) fn infer_set_method(
        &mut self,
        element: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        // T: Hash + Eq bound
        if !self.type_supports_hash(element) || !self.type_supports_eq(element) {
            self.type_error(
                format!(
                    "Set[{}]: element type does not implement `Hash + Eq`; \
                     only types with a hash (integers, bool, char, String, \
                     or structs/enums with `#[derive(Hash, Eq)]`) can be Set elements",
                    type_display(element)
                ),
                span.clone(),
                TypeErrorKind::TraitBoundNotSatisfied,
            );
        }
        let elem = element.clone();
        let set_elem = Type::Named {
            name: "Set".to_string(),
            args: vec![elem.clone()],
        };

        match method {
            "len" => {
                if !args.is_empty() {
                    self.type_error(
                        "Set.len() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "is_empty" => {
                if !args.is_empty() {
                    self.type_error(
                        "Set.is_empty() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Bool
            }
            "contains" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "insert" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "remove" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&elem, &at, arg.value.span.clone());
                }
                Type::Bool
            }
            "union" | "intersection" | "difference" => {
                for arg in args {
                    let at = self.infer_expr(&arg.value);
                    self.check_assignable(&set_elem, &at, arg.value.span.clone());
                }
                set_elem
            }
            _ => self.require_known_method(
                "Set",
                method,
                &[
                    "contains",
                    "difference",
                    "insert",
                    "intersection",
                    "is_empty",
                    "len",
                    "remove",
                    "union",
                ],
                args,
                span,
            ),
        }
    }

    /// Infer the return type of a method call on `Regex`.
    /// Regex is interpreter-only (no codegen). All methods are effect-free.
    pub(super) fn infer_regex_method(
        &mut self,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let match_ty = Type::Named {
            name: "Match".to_string(),
            args: vec![],
        };
        match method {
            "is_match" => {
                if args.len() != 1 {
                    self.type_error(
                        "Regex.is_match() takes 1 argument".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Bool
            }
            "find" => {
                if args.len() != 1 {
                    self.type_error(
                        "Regex.find() takes 1 argument".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![match_ty],
                }
            }
            "find_all" => {
                if args.len() != 1 {
                    self.type_error(
                        "Regex.find_all() takes 1 argument".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Named {
                    name: "Vec".to_string(),
                    args: vec![match_ty],
                }
            }
            "replace_all" => {
                if args.len() != 2 {
                    self.type_error(
                        "Regex.replace_all() takes 2 arguments (s, replacement)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Str
            }
            _ => self.handle_unknown_method(
                "Regex",
                method,
                &["find", "find_all", "is_match", "replace_all"],
                args,
                span,
            ),
        }
    }

    pub(super) fn infer_http_client_method(
        &mut self,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let response_ty = Type::Named {
            name: "Response".to_string(),
            args: vec![],
        };
        let http_error_ty = Type::Named {
            name: "HttpError".to_string(),
            args: vec![],
        };
        let result_response = Type::Named {
            name: "Result".to_string(),
            args: vec![response_ty, http_error_ty],
        };
        match method {
            "get" => {
                if args.len() != 1 {
                    self.type_error(
                        "Client.get() takes 1 argument (url: str)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                result_response
            }
            "post" => {
                if args.len() != 2 {
                    self.type_error(
                        "Client.post() takes 2 arguments (url: str, body: str)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                result_response
            }
            _ => self.handle_unknown_method("Client", method, &["get", "post"], args, span),
        }
    }

    pub(super) fn infer_http_response_method(
        &mut self,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        match method {
            "status" => {
                if !args.is_empty() {
                    self.type_error(
                        "Response.status() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Int(IntSize::I64)
            }
            "body" => {
                if !args.is_empty() {
                    self.type_error(
                        "Response.body() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Str
            }
            "header" => {
                if args.len() != 1 {
                    self.type_error(
                        "Response.header() takes 1 argument (name: str)".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                for arg in args {
                    self.check_expr(&arg.value, &Type::Str);
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![Type::Str],
                }
            }
            _ => self.handle_unknown_method(
                "Response",
                method,
                &["body", "header", "status"],
                args,
                span,
            ),
        }
    }

    pub(super) fn infer_http_error_method(
        &mut self,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        match method {
            "message" => {
                if !args.is_empty() {
                    self.type_error(
                        "HttpError.message() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Str
            }
            _ => self.handle_unknown_method("HttpError", method, &["message"], args, span),
        }
    }

    /// Infer the return type of a method call on `Sender[T]` or `Receiver[T]`.
    /// `is_sender` distinguishes the two ends; `element` is the channel's `T`.
    pub(super) fn infer_channel_method(
        &mut self,
        is_sender: bool,
        element: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        let elem = element.clone();
        let sender_elem = Type::Named {
            name: "Sender".to_string(),
            args: vec![elem.clone()],
        };
        let option_elem = Type::Named {
            name: "Option".to_string(),
            args: vec![elem.clone()],
        };

        if is_sender {
            match method {
                "send" => {
                    for arg in args {
                        let at = self.infer_expr(&arg.value);
                        self.check_assignable(&elem, &at, arg.value.span.clone());
                    }
                    Type::Unit
                }
                "clone" => {
                    if !args.is_empty() {
                        self.type_error(
                            "Sender.clone() takes no arguments".to_string(),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                    }
                    sender_elem
                }
                _ => self.require_known_method("Sender", method, &["clone", "send"], args, span),
            }
        } else {
            // Receiver
            match method {
                "recv" => {
                    if !args.is_empty() {
                        self.type_error(
                            "Receiver.recv() takes no arguments".to_string(),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                    }
                    elem
                }
                "try_recv" => {
                    if !args.is_empty() {
                        self.type_error(
                            "Receiver.try_recv() takes no arguments".to_string(),
                            span.clone(),
                            TypeErrorKind::WrongNumberOfArgs,
                        );
                    }
                    option_elem
                }
                _ => {
                    self.require_known_method("Receiver", method, &["recv", "try_recv"], args, span)
                }
            }
        }
    }

    // ── Label Validation ────────────────────────────────────────

    pub(super) fn validate_labels(
        &mut self,
        args: &[CallArg],
        param_names: &[Option<String>],
        _span: &Span,
    ) {
        let mut seen_label = false;
        let mut seen_unlabeled_after_label = false;

        for (i, arg) in args.iter().enumerate() {
            if let Some(ref label) = arg.label {
                if seen_unlabeled_after_label {
                    self.type_error(
                        "labeled arguments must be contiguous — cannot have unlabeled arguments between labeled ones".to_string(),
                        arg.span.clone(),
                        TypeErrorKind::NonContiguousLabels,
                    );
                }
                seen_label = true;

                // Check label matches parameter name at this position
                if i < param_names.len() {
                    if let Some(ref pname) = param_names[i] {
                        if label != pname {
                            self.type_error(
                                format!(
                                    "label '{}' does not match parameter '{}' at position {}",
                                    label,
                                    pname,
                                    i + 1
                                ),
                                arg.span.clone(),
                                TypeErrorKind::LabelMismatch,
                            );
                        }
                    } else {
                        self.type_error(
                            format!("parameter at position {} cannot be labeled (destructuring pattern)", i + 1),
                            arg.span.clone(),
                            TypeErrorKind::LabelMismatch,
                        );
                    }
                }
            } else if seen_label {
                seen_unlabeled_after_label = true;
            }
        }
    }
}
