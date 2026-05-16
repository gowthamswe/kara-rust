//! Method-call evaluation: the big `eval_method_call` dispatch on
//! receiver shape (Vec/String/Slice/Map/Set/iterator-adapters/etc.).
//!
//! Lives in a sibling `impl<'a> super::Interpreter<'a>` block.

use std::sync::Arc;

use crate::ast::*;
use crate::token::Span;

use super::exec::ControlFlow;
use super::helpers::{eval_http_get, kara_json_to_serde_json, value_compare};
use super::pascal_to_snake;
use super::value::{try_write_or_panic, EnumData, IteratorSource, OrdValue, Value};

impl<'a> super::Interpreter<'a> {
    pub(crate) fn eval_method_call(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Value {
        // Type-receiver associated calls: `T.method(...)` where `T` is a
        // primitive type name. The receiver is an identifier naming a type
        // — not a value — so eval_expr_inner would panic. Handle two shapes:
        //   (a) `.from(x)` — numeric widening (identity at interpreter layer)
        //   (b) operator methods (add/sub/lt/eq/bitand/not/…) — delegate to
        //       the same dispatch used for the lowered `Call(Path)` form.
        if let ExprKind::Identifier(type_name) = &object.kind {
            let target = type_name.as_str();
            let is_primitive = matches!(
                target,
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
                if method == "from" {
                    if let Some(arg) = args.first() {
                        return self.eval_expr_inner(&arg.value);
                    }
                }
                if let Some(result) = self.dispatch_lowered_op(method, args, span) {
                    return result;
                }
            }

            // Lowercase stdlib module aliases: `env.args()`, `env.var(name)`.
            // Map to the capitalized effect resource name so the provider
            // stack lookup in `eval_resource_method` finds the right binding.
            let resource_alias = match type_name.as_str() {
                "env" => Some("Env"),
                _ => None,
            };
            if let Some(resource) = resource_alias {
                return self.eval_resource_method(resource, method, args, span);
            }

            // Effect-resource receiver: `UserDB.query(...)` resolves through
            // the top-of-stack provider binding for `UserDB` (design.md §
            // Provider-Rooted Resources > Runtime mechanics). `UserDB` is
            // not a value — it's a tracked identity — so we skip
            // `eval_expr_inner(object)` on this path and dispatch directly
            // on the provider instance stored in `provider_stack`.
            if self.effect_resources.contains(type_name) {
                return self.eval_resource_method(type_name, method, args, span);
            }
        }

        let obj = self.eval_expr_inner(object);

        // Slice 3 — mut-Slice mutation methods that route their writes
        // back to the original storage. These dispatch BEFORE the
        // Slice→Array normalization below; the normalization is for
        // read-only methods that can safely operate on a fresh snapshot.
        if let Value::Slice {
            storage,
            start,
            len,
            ..
        } = &obj
        {
            if method == "swap" {
                let i_val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                let j_val = args
                    .get(1)
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                if let (Value::Int(i_v), Value::Int(j_v)) = (i_val, j_val) {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let mut guard = try_write_or_panic(storage, &label);
                    let i = i_v as usize;
                    let j = j_v as usize;
                    if i < *len && j < *len {
                        guard.swap(start + i, start + j);
                    }
                }
                return Value::Unit;
            }
        }

        // Slice 3 — methods on `Slice[T]` / `mut Slice[T]` dispatch via
        // the existing Array-method surface. The interpreter snapshots
        // the slice's window into a fresh `Value::Array` so each
        // read-only method (`first` / `last` / `get` / `contains` /
        // `chunks` / `windows` / `len` / `is_empty` / `iter` / etc.)
        // sees a uniform shape. The slice itself is preserved by the
        // `.as_slice` / `.as_slice_mut` MethodCall arm above (which
        // detects the Slice receiver and rebuilds the view) and by the
        // Index expression path for read/write through `[i]`. Mutation
        // methods that need source-aliasing semantics (`swap`) dispatch
        // above this fence.
        let obj = match obj {
            Value::Slice {
                storage,
                start,
                len,
                ..
            } if !matches!(method, "as_slice" | "as_slice_mut") => {
                let snap = storage.read().unwrap()[start..start + len].to_vec();
                Value::array_of(snap)
            }
            other => other,
        };

        // Slice F (`std.json`): `j.stringify()` on a `Json`-typed
        // receiver. Walks the enum tree to a `serde_json::Value` and
        // calls `serde_json::to_string`. Locked design (ii)'s insertion-
        // order property is preserved because the receiver's `Object`
        // payload is a `Vec[(String, Json)]` and the runtime crate's
        // `serde_json` is built with `preserve_order`, so the
        // intermediate `serde_json::Map` round-trips key ordering.
        if method == "stringify" {
            if let Value::EnumVariant { ref enum_name, .. } = obj {
                if enum_name == "Json" {
                    let v = kara_json_to_serde_json(&obj);
                    let s = serde_json::to_string(&v).unwrap_or_else(|_| "null".to_string());
                    return Value::String(s);
                }
            }
        }

        // `#[derive(Display)]` — `to_string()` on a unit enum variant.
        if method == "to_string" {
            if let Value::EnumVariant {
                enum_name,
                variant,
                data: EnumData::Unit,
            } = &obj
            {
                let has_display = self
                    .typecheck_result
                    .enum_info
                    .get(enum_name.as_str())
                    .map(|info| info.derived_traits.contains("Display"))
                    .unwrap_or(false);
                if has_display {
                    let s = if self
                        .typecheck_result
                        .display_snake_case_enums
                        .contains(enum_name.as_str())
                    {
                        pascal_to_snake(variant)
                    } else {
                        variant.clone()
                    };
                    return Value::String(s);
                }
            }
            // All other Display-able values: delegate to Value::fmt
            return Value::String(format!("{}", obj));
        }

        // Category dispatchers — each returns `Some(Value)` if `method`
        // matches one of its handled names and the receiver shape is
        // compatible; otherwise `None` and we fall through to the next.
        if let Some(v) = self.try_eval_iterator_method(method, object, obj.clone(), args, span) {
            return v;
        }
        if let Some(v) = self.try_eval_http_method(method, obj.clone(), args, span) {
            return v;
        }
        if let Some(v) = self.try_eval_regex_method(method, obj.clone(), args, span) {
            return v;
        }
        if let Some(v) = self.try_eval_set_method(method, object, obj.clone(), args, span) {
            return v;
        }
        if let Some(v) = self.try_eval_map_method(method, object, obj.clone(), args, span) {
            return v;
        }
        if let Some(v) = self.try_eval_option_result_method(method, object, obj.clone(), args, span)
        {
            return v;
        }

        // Built-in methods
        match method {
            "len" => {
                return match &obj {
                    Value::Array(rc) => Value::Int(rc.read().unwrap().len() as i64),
                    Value::Slice { len, .. } => Value::Int(*len as i64),
                    Value::String(s) => Value::Int(s.len() as i64),
                    Value::Map(m) => Value::Int(m.len() as i64),
                    Value::SortedSet(s) => Value::Int(s.len() as i64),
                    Value::Set(s) => Value::Int(s.len() as i64),
                    // Note: Map also handled via Map.len() match above
                    _ => unreachable!(
                        "len() on unsupported type at {}:{}; should be caught by typechecker",
                        span.line, span.column
                    ),
                };
            }
            "chars" => {
                // `String.chars() -> Iterator[char]`. Snapshot the chars
                // eagerly into a Value::Iterator so adaptor chains (`map`,
                // `filter`, …) and `for c in s.chars()` go through the same
                // step-machine as other collections. Peer of design.md
                // § Character type (line 2299): the design pins `for c in s`
                // and `s.chars()` as semantic peers; both route here in the
                // tree-walk interpreter (the `for` site dispatches on
                // `Value::String` directly via the same `s.chars()` shape).
                return match &obj {
                    Value::String(s) => {
                        let items: Vec<Value> = s.chars().map(Value::Char).collect();
                        Value::Iterator {
                            source: IteratorSource::Eager { items, cursor: 0 },
                            steps: Vec::new(),
                        }
                    }
                    _ => unreachable!(
                        "chars() on unsupported type at {}:{}; should be caught by typechecker",
                        span.line, span.column
                    ),
                };
            }
            "as_slice" | "as_slice_mut" => {
                // Slice 3 — produce a Value::Slice that shares the
                // source's `Arc<RwLock<Vec<Value>>>` storage. Mutation
                // through a `mut Slice[T]` propagates back to the source
                // because the storage is the same handle, and the
                // runtime guard fires on aliased writes via
                // try_write_or_panic.
                let mutable = method == "as_slice_mut";
                return match &obj {
                    Value::Array(rc) => {
                        let len = rc.read().unwrap().len();
                        Value::Slice {
                            storage: rc.clone(),
                            start: 0,
                            len,
                            mutable,
                        }
                    }
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => Value::Slice {
                        storage: storage.clone(),
                        start: *start,
                        len: *len,
                        mutable,
                    },
                    _ => unreachable!(
                        "{}() on unsupported type at {}:{}; should be caught by typechecker",
                        method, span.line, span.column
                    ),
                };
            }
            "push" => {
                if let Value::Array(rc) = &obj {
                    let val = if let Some(arg) = args.first() {
                        self.eval_expr_inner(&arg.value)
                    } else {
                        Value::Unit
                    };
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    try_write_or_panic(rc, &label).push(val);
                    return Value::Unit;
                }
            }
            // ── VecDeque[T] surface (design.md). The runtime shape is
            //    the same `Value::Array` storage as `Vec[T]`; front-end
            //    ops translate to `Vec::insert(0, …)` / `Vec::remove(0)`
            //    (O(n) — acceptable for the tree-walk interpreter). The
            //    typechecker is permissive, so `Vec[T]` receivers can
            //    also reach these arms; valid Kāra source guards via
            //    receiver type. Routed through helpers to keep
            //    `eval_method_call`'s debug-mode stack frame compact.
            "push_back" | "push_front" | "pop_back" | "pop_front" => {
                if matches!(&obj, Value::Array(_)) {
                    return self.eval_vec_deque_method(method, &obj, object, args);
                }
            }
            // ── Slice[T] / Vec[T] / Array[T,N] shared read-only methods ──────────
            // The interpreter uses Value::Array for all sequence types (Vec,
            // Array, Slice). Each arm only returns when `obj` IS a
            // Value::Array; otherwise it falls through to the impl-block
            // lookup so user-defined structs with the same method name
            // (`struct Counter { fn get(self) ... }`) still resolve correctly.
            "is_empty" => {
                if let Value::Array(ref rc) = obj {
                    return Value::Bool(rc.read().unwrap().is_empty());
                }
                if let Value::Slice { len, .. } = &obj {
                    return Value::Bool(*len == 0);
                }
                if let Value::String(ref s) = obj {
                    return Value::Bool(s.is_empty());
                }
                if let Value::SortedSet(ref s) = obj {
                    return Value::Bool(s.is_empty());
                }
                if let Value::Set(ref s) = obj {
                    return Value::Bool(s.is_empty());
                }
                if let Value::Map(ref m) = obj {
                    return Value::Bool(m.is_empty());
                }
            }
            "first" => {
                let elem = match &obj {
                    Value::Array(rc) => rc.read().unwrap().first().cloned(),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => {
                        if *len > 0 {
                            Some(storage.read().unwrap()[*start].clone())
                        } else {
                            None
                        }
                    }
                    _ => return Value::Unit,
                };
                return match elem {
                    Some(v) => Value::EnumVariant {
                        enum_name: "Option".to_string(),
                        variant: "Some".to_string(),
                        data: EnumData::Tuple(vec![v]),
                    },
                    None => Value::EnumVariant {
                        enum_name: "Option".to_string(),
                        variant: "None".to_string(),
                        data: EnumData::Unit,
                    },
                };
            }
            "last" => {
                let elem = match &obj {
                    Value::Array(rc) => rc.read().unwrap().last().cloned(),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => {
                        if *len > 0 {
                            Some(storage.read().unwrap()[*start + *len - 1].clone())
                        } else {
                            None
                        }
                    }
                    _ => return Value::Unit,
                };
                return match elem {
                    Some(v) => Value::EnumVariant {
                        enum_name: "Option".to_string(),
                        variant: "Some".to_string(),
                        data: EnumData::Tuple(vec![v]),
                    },
                    None => Value::EnumVariant {
                        enum_name: "Option".to_string(),
                        variant: "None".to_string(),
                        data: EnumData::Unit,
                    },
                };
            }
            "get_unchecked" => {
                // Direct-index read with no bounds check. Codegen returns
                // garbage on OOB; the interpreter mirror panics with the
                // standard out-of-bounds message rather than return Value::Unit,
                // so misuse surfaces immediately under `karac run` even though
                // the codegen path is "UB" by design.
                let array_view: Option<Vec<Value>> = match &obj {
                    Value::Array(rc) => Some(rc.read().unwrap().clone()),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => Some(storage.read().unwrap()[*start..*start + *len].to_vec()),
                    _ => None,
                };
                if let Some(v) = array_view {
                    let idx = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(0));
                    if let Value::Int(i) = idx {
                        let i = i as usize;
                        if i >= v.len() {
                            panic!(
                                "Vec.get_unchecked: index {} out of bounds (len={}) — \
                                 caller broke the unsafe precondition",
                                i,
                                v.len()
                            );
                        }
                        return v[i].clone();
                    }
                    return Value::Unit;
                }
                return Value::Unit;
            }
            "get" => {
                let array_view: Option<Vec<Value>> = match &obj {
                    Value::Array(rc) => Some(rc.read().unwrap().clone()),
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => Some(storage.read().unwrap()[*start..*start + *len].to_vec()),
                    _ => None,
                };
                if let Some(v) = array_view {
                    let idx = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(0));
                    return if let Value::Int(i) = idx {
                        let i = i as usize;
                        if i < v.len() {
                            Value::EnumVariant {
                                enum_name: "Option".to_string(),
                                variant: "Some".to_string(),
                                data: EnumData::Tuple(vec![v[i].clone()]),
                            }
                        } else {
                            Value::EnumVariant {
                                enum_name: "Option".to_string(),
                                variant: "None".to_string(),
                                data: EnumData::Unit,
                            }
                        }
                    } else {
                        Value::Unit
                    };
                }
                if let Value::Map(ref m) = obj {
                    let key = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return match m.iter().find(|(k, _)| *k == key) {
                        Some((_, v)) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![v.clone()]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
                if let Value::Struct { ref name, .. } = obj {
                    if name == "Client" {
                        let url = args
                            .first()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        return eval_http_get(&url);
                    }
                }
            }
            "contains" => {
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(v.contains(&needle));
                }
                if let Value::String(ref s) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    if let Value::String(sub) = needle {
                        return Value::Bool(s.contains(sub.as_str()));
                    }
                    return Value::Bool(false);
                }
                if let Value::SortedSet(ref s) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(s.contains_key(&OrdValue(needle)));
                }
                if let Value::Set(ref s) = obj {
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(s.contains(&needle));
                }
            }
            "contains_key" => {
                if let Value::Map(ref m) = obj {
                    let key = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return Value::Bool(m.iter().any(|(k, _)| *k == key));
                }
            }
            "binary_search" => {
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let needle = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Unit);
                    return match v.binary_search_by(|probe| value_compare(probe, &needle)) {
                        Ok(i) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![Value::Int(i as i64)]),
                        },
                        Err(_) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
            }
            "split_at" => {
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let idx = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(0));
                    return if let Value::Int(i) = idx {
                        let i = (i as usize).min(v.len());
                        let left = Value::array_of(v[..i].to_vec());
                        let right = Value::array_of(v[i..].to_vec());
                        Value::Tuple(vec![left, right])
                    } else {
                        Value::Unit
                    };
                }
            }
            "chunks" => {
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let n = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(1));
                    if let Value::Int(n) = n {
                        let n = if n > 0 { n as usize } else { 1 };
                        let chunks: Vec<Value> =
                            v.chunks(n).map(|c| Value::array_of(c.to_vec())).collect();
                        return Value::array_of(chunks);
                    }
                }
                // Iterator-trait variant — lazy chunks; wraps the
                // receiver into an `IteratorSource::Chunks`. Each
                // pull yields a freshly allocated `Vec[T]`. n is
                // clamped to `n.max(1)`, matching `step_by`'s policy.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.chunks() requires an integer argument".to_string(),
                            span,
                        );
                    };
                    let n = match self.eval_expr_inner(&arg.value) {
                        Value::Int(n) => n.max(1) as usize,
                        v => {
                            return self.record_runtime_error(
                                format!("Iterator.chunks() expects an integer; got {}", v),
                                span,
                            );
                        }
                    };
                    return Value::Iterator {
                        source: IteratorSource::Chunks {
                            inner: Box::new(obj),
                            n,
                            exhausted: false,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "windows" => {
                if let Value::Array(ref rc) = obj {
                    let v = rc.read().unwrap();
                    let n = args
                        .first()
                        .map(|a| self.eval_expr_inner(&a.value))
                        .unwrap_or(Value::Int(1));
                    if let Value::Int(n) = n {
                        let n = if n > 0 && (n as usize) <= v.len() {
                            n as usize
                        } else {
                            return Value::array_of(vec![]);
                        };
                        let wins: Vec<Value> =
                            v.windows(n).map(|w| Value::array_of(w.to_vec())).collect();
                        return Value::array_of(wins);
                    }
                }
                // Iterator-trait variant — lazy sliding window; each
                // pull yields a freshly cloned buffer of size n. n=0
                // and n>source-length both produce zero windows; we
                // clamp to n.max(1) at the dispatch site so the
                // first-prime-pull naturally trips the
                // sticky-exhausted path on a too-small source.
                if matches!(obj, Value::Iterator { .. }) {
                    let Some(arg) = args.first() else {
                        return self.record_runtime_error(
                            "Iterator.windows() requires an integer argument".to_string(),
                            span,
                        );
                    };
                    let n = match self.eval_expr_inner(&arg.value) {
                        Value::Int(n) => n.max(1) as usize,
                        v => {
                            return self.record_runtime_error(
                                format!("Iterator.windows() expects an integer; got {}", v),
                                span,
                            );
                        }
                    };
                    return Value::Iterator {
                        source: IteratorSource::Windows {
                            inner: Box::new(obj),
                            n,
                            buffer: Vec::with_capacity(n),
                            primed: false,
                            exhausted: false,
                        },
                        steps: Vec::new(),
                    };
                }
            }
            "sort" => {
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    try_write_or_panic(rc, &label).sort_by(value_compare);
                    return Value::Unit;
                }
            }
            "sort_by" => {
                // sort_by(|a, b| -> Ordering) — snapshot the vec so the user
                // closure can re-enter the interpreter freely (std::sync::RwLock
                // is non-reentrant on the same thread), sort the snapshot via
                // the user comparator, then write the result back.
                if args.len() != 1 {
                    panic!(
                        "sort_by expects 1 argument (comparator closure), got {}",
                        args.len()
                    );
                }
                let cmp_val = self.eval_expr_inner(&args[0].value);
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let mut snapshot = rc.read().unwrap().clone();
                    snapshot.sort_by(|a, b| {
                        self.invoke_value_comparator(&cmp_val, a.clone(), b.clone(), "sort_by")
                    });
                    *try_write_or_panic(rc, &label) = snapshot;
                    return Value::Unit;
                }
            }
            "sorted" => {
                if let Value::String(ref s) = obj {
                    let mut chars: Vec<char> = s.chars().collect();
                    chars.sort_unstable();
                    return Value::String(chars.into_iter().collect());
                }
                if let Value::Array(ref rc) = obj {
                    let mut v = rc.read().unwrap().clone();
                    v.sort_by(value_compare);
                    return Value::array_of(v);
                }
            }
            "sorted_by" => {
                // sorted_by(|a, b| -> Ordering) — same snapshot-then-sort
                // pattern as `sort_by`, but returns a new collection instead
                // of mutating in place. The `.clone()` releases the read
                // guard before the comparator runs, so the user closure can
                // re-enter the interpreter freely.
                if args.len() != 1 {
                    panic!(
                        "sorted_by expects 1 argument (comparator closure), got {}",
                        args.len()
                    );
                }
                let cmp_val = self.eval_expr_inner(&args[0].value);
                if let Value::String(ref s) = obj {
                    let mut chars: Vec<char> = s.chars().collect();
                    chars.sort_by(|a, b| {
                        self.invoke_value_comparator(
                            &cmp_val,
                            Value::Char(*a),
                            Value::Char(*b),
                            "sorted_by",
                        )
                    });
                    return Value::String(chars.into_iter().collect());
                }
                if let Value::Array(ref rc) = obj {
                    let mut v = rc.read().unwrap().clone();
                    v.sort_by(|a, b| {
                        self.invoke_value_comparator(&cmp_val, a.clone(), b.clone(), "sorted_by")
                    });
                    return Value::array_of(v);
                }
            }
            "sort_by_key" => {
                // sort_by_key(|t| -> K) where K: Ord — precompute keys once
                // (Rust's `sort_by_key` semantics: each element's key is
                // computed exactly once, not on every comparator invocation),
                // sort the (key, value) pairs by key via `value_compare`, write
                // the values back. Snapshot-then-replace mirrors `sort_by` to
                // keep the user closure free to re-enter the interpreter.
                if args.len() != 1 {
                    panic!(
                        "sort_by_key expects 1 argument (key closure), got {}",
                        args.len()
                    );
                }
                let key_val = self.eval_expr_inner(&args[0].value);
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let snapshot = rc.read().unwrap().clone();
                    let mut keyed: Vec<(Value, Value)> = snapshot
                        .into_iter()
                        .map(|v| {
                            let k = self.invoke_function_value(key_val.clone(), vec![v.clone()]);
                            (k, v)
                        })
                        .collect();
                    keyed.sort_by(|(k1, _), (k2, _)| value_compare(k1, k2));
                    let sorted: Vec<Value> = keyed.into_iter().map(|(_, v)| v).collect();
                    *try_write_or_panic(rc, &label) = sorted;
                    return Value::Unit;
                }
            }
            "sorted_by_key" => {
                // sorted_by_key(|t| -> K) where K: Ord — same precompute-keys
                // pattern as `sort_by_key`, but returns a new Vec instead of
                // mutating in place.
                if args.len() != 1 {
                    panic!(
                        "sorted_by_key expects 1 argument (key closure), got {}",
                        args.len()
                    );
                }
                let key_val = self.eval_expr_inner(&args[0].value);
                if let Value::Array(ref rc) = obj {
                    let snapshot = rc.read().unwrap().clone();
                    let mut keyed: Vec<(Value, Value)> = snapshot
                        .into_iter()
                        .map(|v| {
                            let k = self.invoke_function_value(key_val.clone(), vec![v.clone()]);
                            (k, v)
                        })
                        .collect();
                    keyed.sort_by(|(k1, _), (k2, _)| value_compare(k1, k2));
                    let sorted: Vec<Value> = keyed.into_iter().map(|(_, v)| v).collect();
                    return Value::array_of(sorted);
                }
            }
            "reverse" => {
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    try_write_or_panic(rc, &label).reverse();
                    return Value::Unit;
                }
            }
            "fill" => {
                let fill_val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Array(ref rc) = obj {
                    let label = match &object.kind {
                        ExprKind::Identifier(n) => n.clone(),
                        _ => "<value>".to_string(),
                    };
                    let mut guard = try_write_or_panic(rc, &label);
                    for elem in guard.iter_mut() {
                        *elem = fill_val.clone();
                    }
                    return Value::Unit;
                }
            }
            "swap" => {
                let i = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                let j = args
                    .get(1)
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Int(0));
                if let (Value::Int(i_val), Value::Int(j_val)) = (i, j) {
                    if let Value::Array(ref rc) = obj {
                        let label = match &object.kind {
                            ExprKind::Identifier(n) => n.clone(),
                            _ => "<value>".to_string(),
                        };
                        let mut guard = try_write_or_panic(rc, &label);
                        let i = i_val as usize;
                        let j = j_val as usize;
                        if i < guard.len() && j < guard.len() {
                            guard.swap(i, j);
                        }
                        return Value::Unit;
                    }
                } else {
                    // consume obj to avoid borrow-after-move
                    let _ = obj;
                }
            }
            // ── Channel[T] / Sender[T] / Receiver[T] methods ──────────────
            "send" => {
                let val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Sender(ref queue) = obj {
                    queue.lock().unwrap().push_back(val);
                    return Value::Unit;
                }
            }
            "recv" => {
                if let Value::Receiver(ref queue) = obj {
                    // In the tree-walk interpreter tests the sender always
                    // fires before recv, so the queue has an item. If empty
                    // (would deadlock in a real runtime) return Unit rather
                    // than blocking the interpreter thread forever.
                    let val = queue.lock().unwrap().pop_front().unwrap_or(Value::Unit);
                    return val;
                }
            }
            "try_recv" => {
                if let Value::Receiver(ref queue) = obj {
                    let opt = queue.lock().unwrap().pop_front();
                    return match opt {
                        Some(v) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![v]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    };
                }
            }
            // clone() — Sender creates an additional producer sharing the
            // same queue Arc. For collection types (Array/String/Map/Set/
            // SortedSet) the canonical Clone impl is a structural deep
            // copy: each `Value` variant is itself `Clone` so
            // `obj.clone()` does the right thing without per-type
            // unrolling. Non-Clone payloads (closures, iterators, refs,
            // entries, shared cells) fall through; the typechecker
            // rejects `clone()` on those receivers via `clone_self_type_for`.
            "clone" => {
                if let Value::Sender(ref queue) = obj {
                    return Value::Sender(Arc::clone(queue));
                }
                match &obj {
                    Value::Array(rc) => {
                        // Deep copy — clone the inner Vec into a fresh
                        // shared cell so the clone has independent
                        // storage. Slice 3: this matches the v1
                        // value-semantics rule that `arr.clone()`
                        // produces a structurally independent array.
                        return Value::array_of(rc.read().unwrap().clone());
                    }
                    Value::Slice {
                        storage,
                        start,
                        len,
                        ..
                    } => {
                        return Value::array_of(
                            storage.read().unwrap()[*start..*start + *len].to_vec(),
                        );
                    }
                    Value::String(s) => return Value::String(s.clone()),
                    Value::Map(m) => return Value::Map(m.clone()),
                    Value::Set(s) => return Value::Set(s.clone()),
                    Value::SortedSet(s) => return Value::SortedSet(s.clone()),
                    _ => {}
                }
            }

            _ => {}
        }

        // Primitive value-receiver dispatch for the builtin Eq/Ord methods.
        // The typechecker registers `eq`/`ne`/`lt`/`le`/`gt`/`ge`/`cmp` for
        // every integer width, bool, char, String, and the F32/F64 total-
        // order wrappers (`register_builtin_impl("Ord", ...)` in
        // src/typechecker.rs) — but those registrations live in the
        // typechecker's env, not the interpreter's, so a call like
        // `b.cmp(a)` with a primitive receiver would otherwise fall through
        // to the impl-block lookup below and panic. The type-name receiver
        // form `i64.cmp(a, b)` already routes through `dispatch_lowered_op`;
        // this mirrors that path for the value-receiver form (one arg
        // instead of two) so `xs.sort_by(|a, b| b.cmp(a))` works.
        if matches!(
            &obj,
            Value::Int(_)
                | Value::Char(_)
                | Value::Bool(_)
                | Value::String(_)
                | Value::TotalFloat32(_)
                | Value::TotalFloat64(_)
        ) {
            if method == "cmp" && args.len() == 1 {
                let other = self.eval_expr_inner(&args[0].value);
                let ord = value_compare(&obj, &other);
                return Value::EnumVariant {
                    enum_name: "Ordering".to_string(),
                    variant: match ord {
                        std::cmp::Ordering::Less => "Less".to_string(),
                        std::cmp::Ordering::Equal => "Equal".to_string(),
                        std::cmp::Ordering::Greater => "Greater".to_string(),
                    },
                    data: EnumData::Unit,
                };
            }
            let bin_op = match method {
                "eq" => Some(BinOp::Eq),
                "ne" => Some(BinOp::NotEq),
                "lt" => Some(BinOp::Lt),
                "le" => Some(BinOp::LtEq),
                "gt" => Some(BinOp::Gt),
                "ge" => Some(BinOp::GtEq),
                _ => None,
            };
            if let Some(op) = bin_op {
                if args.len() == 1 {
                    let rhs = self.eval_expr_inner(&args[0].value);
                    return self.eval_binary(&op, obj.clone(), rhs, span);
                }
            }
        }

        // Try to find method via impl block
        let type_name = self.value_type_name(&obj);
        let method_key = format!("{}.{}", type_name, method);

        if let Some(func) = self.env.get(&method_key) {
            let mut arg_vals: Vec<Value> = vec![obj];
            arg_vals.extend(args.iter().map(|a| self.eval_expr_inner(&a.value)));

            if let Value::Function {
                param_patterns,
                param_defaults,
                body,
                closure_env,
                ..
            } = func
            {
                self.env.push_scope();
                if let Some(ref captured) = closure_env {
                    for (k, v) in captured {
                        self.env.define(k.clone(), v.clone());
                    }
                }
                // `param_patterns` already includes the `self` binding for
                // self-taking methods (prepended at impl-registration time),
                // so a straight in-order bind handles both receiver and args.
                for (i, pat) in param_patterns.iter().enumerate() {
                    let val = if let Some(v) = arg_vals.get(i) {
                        v.clone()
                    } else if let Some(Some(default_expr)) = param_defaults.get(i) {
                        self.eval_expr_inner(default_expr)
                    } else {
                        continue;
                    };
                    self.bind_pattern(pat, val);
                }
                let result = self.eval_block_inner(&body);
                self.env.pop_scope();
                return match result {
                    Ok(v) => v,
                    Err(ControlFlow::Return(v)) => v,
                    Err(cf) => self.set_cf(cf),
                };
            }
        }

        unreachable!(
            "method '{}' not found on type '{}' at {}:{}; should be caught by typechecker",
            method, type_name, span.line, span.column
        )
    }
}
