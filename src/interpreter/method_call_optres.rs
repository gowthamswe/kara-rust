//! Option / Result / Atomic method dispatch — the bodies of the
//! `unwrap`/`expect`/`is_some`/`is_none`/`is_ok`/`is_err`/`load`/
//! `store` arms lifted out of `eval_method_call`.

use crate::ast::*;
use crate::token::Span;

use super::value::{EnumData, Value};

impl<'a> super::Interpreter<'a> {
    pub(super) fn try_eval_option_result_method(
        &mut self,
        method: &str,
        object: &Expr,
        obj: Value,
        args: &[CallArg],
        span: &Span,
    ) -> Option<Value> {
        match method {
            "unwrap" => {
                return Some(match &obj {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } if variant == "Ok" || variant == "Some" => {
                        vals.first().cloned().unwrap_or(Value::Unit)
                    }
                    Value::EnumVariant { variant, .. } if variant == "Err" || variant == "None" => {
                        return Some(self.record_runtime_error(
                            format!("called unwrap() on {}", variant),
                            span,
                        ));
                    }
                    other => other.clone(),
                });
            }
            "expect" => {
                let msg = if let Some(arg) = args.first() {
                    match self.eval_expr_inner(&arg.value) {
                        Value::String(s) => s,
                        v => format!("{}", v),
                    }
                } else {
                    String::new()
                };
                return Some(match &obj {
                    Value::EnumVariant {
                        variant,
                        data: EnumData::Tuple(vals),
                        ..
                    } if variant == "Ok" || variant == "Some" => {
                        vals.first().cloned().unwrap_or(Value::Unit)
                    }
                    Value::EnumVariant { variant, .. } if variant == "Err" || variant == "None" => {
                        return Some(self.record_runtime_error(
                            if msg.is_empty() {
                                format!("expect() called on {}", variant)
                            } else {
                                format!("{}: {}", msg, variant)
                            },
                            span,
                        ));
                    }
                    other => other.clone(),
                });
            }
            "is_some" => {
                return Some(match &obj {
                    Value::EnumVariant { variant, .. } if variant == "Some" => Value::Bool(true),
                    Value::EnumVariant { variant, .. } if variant == "None" => Value::Bool(false),
                    _ => Value::Bool(true),
                });
            }
            "is_none" => {
                return Some(match &obj {
                    Value::EnumVariant { variant, .. } if variant == "None" => Value::Bool(true),
                    _ => Value::Bool(false),
                });
            }
            "is_ok" => {
                return Some(match &obj {
                    Value::EnumVariant { variant, .. } if variant == "Ok" => Value::Bool(true),
                    _ => Value::Bool(false),
                });
            }
            "is_err" => {
                return Some(match &obj {
                    Value::EnumVariant { variant, .. } if variant == "Err" => Value::Bool(true),
                    _ => Value::Bool(false),
                });
            }
            "load" => {
                if let Value::Atomic(inner) = &obj {
                    // Ordering argument accepted but ignored (no concurrency in tree-walk interpreter)
                    return Some(*inner.clone());
                }
            }
            "store" => {
                if let Value::Atomic(_) = &obj {
                    let val = if let Some(arg) = args.first() {
                        self.eval_expr_inner(&arg.value)
                    } else {
                        Value::Unit
                    };
                    // Update the atomic in the environment
                    if let ExprKind::Identifier(name) = &object.kind {
                        self.env.set(name, Value::Atomic(Box::new(val)));
                    }
                    return Some(Value::Unit);
                }
            }
            _ => return None,
        }
        None
    }
}
