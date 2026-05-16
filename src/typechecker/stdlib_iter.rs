//! Iterator-adapter method-inference dispatch.
//!
//! Houses `infer_iterator_method` — the per-adapter return-type
//! synthesizer invoked when the receiver is `Iterator[Item = T]`.

use crate::ast::*;
use crate::token::Span;

use super::types::{IntSize, Type};
use super::TypeErrorKind;

impl<'a> super::TypeChecker<'a> {
    /// Infer the return type of a method call on `Iterator[Item = T]`.
    /// `next()` lands in subtask 1; `map(f)` / `filter(pred)` in subtask 3;
    /// the rest of the surface follows in `wip-list2.md` subtasks 4+.
    pub(super) fn infer_iterator_method(
        &mut self,
        item: &Type,
        method: &str,
        args: &[CallArg],
        span: &Span,
        is_peekable: bool,
    ) -> Type {
        match method {
            "next" => {
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.next() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![item.clone()],
                }
            }
            "map" => {
                // `map(f: Fn(T) -> U) -> Iterator[U]` — U is solved by
                // pushing `Fn(T) -> TypeParam("__iter_map_U")` into
                // `check_expr`. The closure-pushdown path (lines 5429+) seeds
                // the closure's parameter from T and infers the body type
                // freely; the resulting `actual` is `Fn(T) -> body_ty`. We
                // then read body_ty back out as the new Item type.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.map() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Error],
                    };
                }
                let f_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_map_U".to_string())),
                };
                let actual_ty = self.check_expr(&args[0].value, &f_ty);
                let new_item = match actual_ty {
                    Type::Function { return_type, .. } => *return_type,
                    _ => Type::Error,
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![new_item],
                }
            }
            "filter" => {
                // `filter(pred: Fn(T) -> bool) -> Iterator[T]` — no fresh
                // type variable; the predicate's signature is fully known
                // so check_expr suffices for closure-param pushdown.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.filter() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let pred_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::Bool),
                };
                self.check_expr(&args[0].value, &pred_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "count" => {
                // `count() -> i64` — terminal. Drains the iterator and
                // returns the element count.
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.count() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Int(IntSize::I64)
            }
            "collect" => {
                // `collect() -> Vec[T]` — terminal. v1 is Vec-only; full
                // FromIterator (collect into Set / Map / Array / etc. via
                // type-context inference) is a follow-up CR per
                // wip-list2.md subtask 4.
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.collect() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Vec".to_string(),
                    args: vec![item.clone()],
                }
            }
            "fold" => {
                // `fold(init: A, f: Fn(A, T) -> A) -> A` — terminal. A is
                // inferred from `init` (concrete after infer_expr); both
                // closure params and return are then concrete so
                // check_expr suffices for closure-pushdown.
                if args.len() != 2 {
                    self.type_error(
                        format!("Iterator.fold() expects 2 arguments, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Error;
                }
                let acc_ty = self.infer_expr(&args[0].value);
                let f_ty = Type::Function {
                    params: vec![acc_ty.clone(), item.clone()],
                    return_type: Box::new(acc_ty.clone()),
                };
                self.check_expr(&args[1].value, &f_ty);
                acc_ty
            }
            "any" | "all" => {
                // Short-circuit terminals — `any(pred) -> bool` /
                // `all(pred) -> bool`. Same predicate signature as
                // `filter`, so check_expr against `Fn(T) -> bool`
                // suffices for closure-pushdown.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Bool;
                }
                let pred_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::Bool),
                };
                self.check_expr(&args[0].value, &pred_ty);
                Type::Bool
            }
            "enumerate" => {
                // `enumerate() -> Iterator[(i64, T)]` — wraps each item
                // into a tuple of (index, item).
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.enumerate() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Tuple(vec![Type::Int(IntSize::I64), item.clone()])],
                }
            }
            "take" | "skip" => {
                // `take(n: i64) -> Iterator[T]` and `skip(n: i64) ->
                // Iterator[T]`. Argument is checked against i64; the
                // element type passes through unchanged.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                self.check_expr(&args[0].value, &Type::Int(IntSize::I64));
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "take_while" | "skip_while" => {
                // `take_while(pred: Fn(T) -> bool) -> Iterator[T]` and
                // `skip_while(pred: Fn(T) -> bool) -> Iterator[T]` —
                // same predicate signature as `filter`, so check_expr
                // against `Fn(T) -> bool` suffices for closure-pushdown.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let pred_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::Bool),
                };
                self.check_expr(&args[0].value, &pred_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "flat_map" => {
                // `flat_map(f: Fn(T) -> Iterator[U]) -> Iterator[U]` —
                // the closure body must return an iterator; its element
                // type becomes the new Item. Same pushdown pattern as
                // `map`: `Fn(T) -> TypeParam("__iter_flatmap_U")` lets
                // the body's actual return type flow back, then we
                // pattern-match it for `Iterator[U]`. A non-iterator
                // return raises a TypeMismatch explicitly.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.flat_map() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Error],
                    };
                }
                let f_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_flatmap_U".to_string())),
                };
                let actual_ty = self.check_expr(&args[0].value, &f_ty);
                let new_item = match actual_ty {
                    Type::Function { return_type, .. } => match *return_type {
                        Type::Named {
                            name,
                            args: mut iter_args,
                        } if name == "Iterator" && iter_args.len() == 1 => iter_args.remove(0),
                        other => {
                            self.type_error(
                                format!(
                                    "Iterator.flat_map() closure must return Iterator[U], found {:?}",
                                    other
                                ),
                                span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            Type::Error
                        }
                    },
                    _ => Type::Error,
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![new_item],
                }
            }
            "step_by" => {
                // `step_by(n: i64) -> Iterator[T]` — element type
                // passes through. Argument is checked against i64;
                // negative or zero `n` is a runtime concern (clamped
                // to 1 by the interpreter), so the typechecker
                // accepts any i64.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.step_by() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                self.check_expr(&args[0].value, &Type::Int(IntSize::I64));
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "cycle" => {
                // `cycle() -> Iterator[T]` — element type passes
                // through. The "cloneable source" requirement noted
                // in design.md is implicit here: every Value derives
                // Clone, so any iterator can cycle.
                if !args.is_empty() {
                    self.type_error(
                        "Iterator.cycle() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "inspect" => {
                // `inspect(f: Fn(T) -> R) -> Iterator[T]` — closure's
                // return is discarded so we leave R free via TypeParam
                // pushdown. Element type passes through unchanged.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.inspect() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let f_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_inspect_R".to_string())),
                };
                self.check_expr(&args[0].value, &f_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "scan" => {
                // `scan(init: A, f: Fn(A, T) -> Option<(A, U)>) ->
                // Iterator[U]`. A is inferred from init; the closure's
                // return is constrained via post-hoc unwrap of
                // Option<(A, U)>. U becomes the new Item.
                if args.len() != 2 {
                    self.type_error(
                        format!("Iterator.scan() expects 2 arguments, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Error],
                    };
                }
                let acc_ty = self.infer_expr(&args[0].value);
                let f_ty = Type::Function {
                    params: vec![acc_ty.clone(), item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_scan_R".to_string())),
                };
                let actual_ty = self.check_expr(&args[1].value, &f_ty);
                let new_item = match actual_ty {
                    Type::Function { return_type, .. } => match *return_type {
                        Type::Named {
                            name,
                            args: mut opt_args,
                        } if name == "Option" && opt_args.len() == 1 => match opt_args.remove(0) {
                            Type::Tuple(mut tuple_args) if tuple_args.len() == 2 => {
                                let actual_acc = tuple_args.remove(0);
                                self.check_assignable(&acc_ty, &actual_acc, span.clone());
                                tuple_args.remove(0)
                            }
                            other => {
                                self.type_error(
                                    format!(
                                        "Iterator.scan() closure must return Option<(A, U)>, found Option<{:?}>",
                                        other
                                    ),
                                    span.clone(),
                                    TypeErrorKind::TypeMismatch,
                                );
                                Type::Error
                            }
                        },
                        other => {
                            self.type_error(
                                format!(
                                    "Iterator.scan() closure must return Option<(A, U)>, found {:?}",
                                    other
                                ),
                                span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            Type::Error
                        }
                    },
                    _ => Type::Error,
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![new_item],
                }
            }
            "chunks" | "windows" => {
                // `chunks(n: i64) -> Iterator[Vec[T]]` — non-overlapping
                // groups of up to n consecutive items (final group may
                // be shorter when the source length isn't a multiple
                // of n).
                // `windows(n: i64) -> Iterator[Vec[T]]` — sliding view
                // of size n, advancing by 1 per pull (yields nothing
                // when source has fewer than n items). Both buffer
                // and allocate a fresh `Vec[T]` per yielded group; the
                // effect-checker seeds `allocates(Heap)` on
                // `Iterator.{chunks,windows}`. Argument is checked
                // against i64 (clamped at the runtime layer like
                // `take(n)` / `step_by(n)`).
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.{}() expects 1 argument, found {}",
                            method,
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Named {
                            name: "Vec".to_string(),
                            args: vec![item.clone()],
                        }],
                    };
                }
                let arg_ty = self.infer_expr(&args[0].value);
                self.check_assignable(
                    &Type::Int(IntSize::I64),
                    &arg_ty,
                    args[0].value.span.clone(),
                );
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Named {
                        name: "Vec".to_string(),
                        args: vec![item.clone()],
                    }],
                }
            }
            "chunk_by" => {
                // `chunk_by(key_fn: Fn(T) -> K) -> Iterator[Vec[T]]` —
                // groups consecutive elements where `key_fn(item)`
                // produces equal keys. Each group is allocated as a
                // fresh `Vec[T]` (the effect-checker seeds
                // `allocates(Heap)` on `Iterator.chunk_by`). K is left
                // free via TypeParam pushdown — equality is enforced
                // at runtime via `Value::PartialEq`, matching the
                // permissive pattern used by scan/inspect.
                if args.len() != 1 {
                    self.type_error(
                        format!(
                            "Iterator.chunk_by() expects 1 argument, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Named {
                            name: "Vec".to_string(),
                            args: vec![item.clone()],
                        }],
                    };
                }
                let key_fn_ty = Type::Function {
                    params: vec![item.clone()],
                    return_type: Box::new(Type::TypeParam("__iter_chunk_by_K".to_string())),
                };
                self.check_expr(&args[0].value, &key_fn_ty);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Named {
                        name: "Vec".to_string(),
                        args: vec![item.clone()],
                    }],
                }
            }
            "chain" => {
                // `chain(other: Iterator[T]) -> Iterator[T]` — the
                // element type must agree on both sides. Push down
                // `Iterator[T]` so the argument's element type is
                // checked against ours.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.chain() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![item.clone()],
                    };
                }
                let expected = Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                };
                self.check_expr(&args[0].value, &expected);
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![item.clone()],
                }
            }
            "zip" => {
                // `zip(other: Iterator[U]) -> Iterator[(T, U)]` — the
                // other iterator's element type can differ; we infer
                // it and use it as the second tuple slot. infer_expr
                // gives us back the actual `Iterator[U]` from which we
                // can extract U.
                if args.len() != 1 {
                    self.type_error(
                        format!("Iterator.zip() expects 1 argument, found {}", args.len()),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Named {
                        name: "Iterator".to_string(),
                        args: vec![Type::Tuple(vec![item.clone(), Type::Error])],
                    };
                }
                let other_ty = self.infer_expr(&args[0].value);
                let other_item = match &other_ty {
                    Type::Named { name, args } if name == "Iterator" && args.len() == 1 => {
                        args[0].clone()
                    }
                    _ => {
                        self.type_error(
                            format!(
                                "Iterator.zip() expects an Iterator argument, found {:?}",
                                other_ty
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        Type::Error
                    }
                };
                Type::Named {
                    name: "Iterator".to_string(),
                    args: vec![Type::Tuple(vec![item.clone(), other_item])],
                }
            }
            "peekable" => {
                // `peekable() -> Peekable[T]` — wraps the receiver into a
                // distinct named type that exposes `peek()` in addition
                // to the rest of the Iterator surface. Idempotent on
                // a Peekable receiver (still returns Peekable[T]).
                if !args.is_empty() {
                    self.type_error(
                        format!(
                            "Iterator.peekable() takes no arguments, found {}",
                            args.len()
                        ),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Peekable".to_string(),
                    args: vec![item.clone()],
                }
            }
            "peek" => {
                // `peek() -> Option<T>` — only valid on `Peekable[T]`. The
                // distinct receiver name is the type-level signal that
                // peekable() has been called; on a plain Iterator we
                // emit UnknownMethod (via Type::Error) so adaptor pipelines
                // that drop the Peekable wrapper (e.g. `peekable().map(f)`
                // returning Iterator[U]) reject downstream `.peek()`.
                if !is_peekable {
                    self.type_error(
                        "peek() is only available on Peekable[T] (call .peekable() first)"
                            .to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                    return Type::Error;
                }
                if !args.is_empty() {
                    self.type_error(
                        "Peekable.peek() takes no arguments".to_string(),
                        span.clone(),
                        TypeErrorKind::WrongNumberOfArgs,
                    );
                    for arg in args {
                        self.infer_expr(&arg.value);
                    }
                }
                Type::Named {
                    name: "Option".to_string(),
                    args: vec![item.clone()],
                }
            }
            _ => self.require_known_method(
                "Iterator",
                method,
                &[
                    "all",
                    "any",
                    "chain",
                    "chunk_by",
                    "chunks",
                    "collect",
                    "count",
                    "cycle",
                    "enumerate",
                    "filter",
                    "flat_map",
                    "fold",
                    "inspect",
                    "map",
                    "next",
                    "peek",
                    "peekable",
                    "scan",
                    "skip",
                    "skip_while",
                    "step_by",
                    "take",
                    "take_while",
                    "windows",
                    "zip",
                ],
                args,
                span,
            ),
        }
    }
}
