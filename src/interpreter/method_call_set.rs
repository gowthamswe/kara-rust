//! Set / SortedSet method dispatch — the bodies of the `clear`/`min`/
//! `max`/`union`/`intersection`/`difference` arms lifted out of
//! `eval_method_call`. Receivers are `Value::Set` / `Value::SortedSet`
//! / `Value::Map`.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::token::Span;

use super::value::{EnumData, OrdValue, Value};

impl<'a> super::Interpreter<'a> {
    pub(super) fn try_eval_set_method(
        &mut self,
        method: &str,
        object: &Expr,
        obj: Value,
        args: &[CallArg],
        _span: &Span,
    ) -> Option<Value> {
        match method {
            "clear" => {
                if let Value::Map(_) = obj {
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Map(Vec::new()));
                    }
                    return Some(Value::Unit);
                }
            }
            "min" => {
                if let Value::SortedSet(ref set) = obj {
                    return Some(match set.keys().next() {
                        Some(k) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![k.0.clone()]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    });
                }
            }
            "max" => {
                if let Value::SortedSet(ref set) = obj {
                    return Some(match set.keys().next_back() {
                        Some(k) => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "Some".to_string(),
                            data: EnumData::Tuple(vec![k.0.clone()]),
                        },
                        None => Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        },
                    });
                }
            }
            "union" => {
                let other = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let (Value::SortedSet(ref a_set), Value::SortedSet(ref b_set)) = (&obj, &other) {
                    #[allow(clippy::mutable_key_type)]
                    let mut result = a_set.clone();
                    for (k, _v) in b_set.iter() {
                        result.insert(k.clone(), ());
                    }
                    return Some(Value::SortedSet(result));
                }
                if let (Value::Set(ref a_set), Value::Set(ref b_set)) = (&obj, &other) {
                    let mut result = a_set.clone();
                    for v in b_set {
                        if !result.contains(v) {
                            result.push(v.clone());
                        }
                    }
                    return Some(Value::Set(result));
                }
            }
            "intersection" => {
                let other = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let (Value::SortedSet(ref a_set), Value::SortedSet(ref b_set)) = (&obj, &other) {
                    #[allow(clippy::mutable_key_type)]
                    let result: BTreeMap<OrdValue, ()> = a_set
                        .iter()
                        .filter(|(k, _)| b_set.contains_key(*k))
                        .map(|(k, v)| (k.clone(), *v))
                        .collect();
                    return Some(Value::SortedSet(result));
                }
                if let (Value::Set(ref a_set), Value::Set(ref b_set)) = (&obj, &other) {
                    let result: Vec<Value> = a_set
                        .iter()
                        .filter(|v| b_set.contains(v))
                        .cloned()
                        .collect();
                    return Some(Value::Set(result));
                }
            }
            "difference" => {
                let other = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let (Value::SortedSet(ref a_set), Value::SortedSet(ref b_set)) = (&obj, &other) {
                    #[allow(clippy::mutable_key_type)]
                    let result: BTreeMap<OrdValue, ()> = a_set
                        .iter()
                        .filter(|(k, _)| !b_set.contains_key(*k))
                        .map(|(k, v)| (k.clone(), *v))
                        .collect();
                    return Some(Value::SortedSet(result));
                }
                if let (Value::Set(ref a_set), Value::Set(ref b_set)) = (&obj, &other) {
                    let result: Vec<Value> = a_set
                        .iter()
                        .filter(|v| !b_set.contains(v))
                        .cloned()
                        .collect();
                    return Some(Value::Set(result));
                }
            }
            _ => return None,
        }
        None
    }
}
