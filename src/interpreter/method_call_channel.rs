//! Channel method dispatch — the bodies of the `send`/`recv`/
//! `try_recv` arms lifted out of `eval_method_call`. Receivers are
//! `Value::Sender` and `Value::Receiver`.

use crate::ast::*;
use crate::token::Span;

use super::value::{EnumData, Value};

impl<'a> super::Interpreter<'a> {
    pub(super) fn try_eval_channel_method(
        &mut self,
        method: &str,
        obj: Value,
        args: &[CallArg],
        _span: &Span,
    ) -> Option<Value> {
        match method {
            "send" => {
                let val = args
                    .first()
                    .map(|a| self.eval_expr_inner(&a.value))
                    .unwrap_or(Value::Unit);
                if let Value::Sender(ref queue) = obj {
                    queue.lock().unwrap().push_back(val);
                    return Some(Value::Unit);
                }
            }
            "recv" => {
                if let Value::Receiver(ref queue) = obj {
                    // In the tree-walk interpreter tests the sender always
                    // fires before recv, so the queue has an item. If empty
                    // (would deadlock in a real runtime) return Unit rather
                    // than blocking the interpreter thread forever.
                    let val = queue.lock().unwrap().pop_front().unwrap_or(Value::Unit);
                    return Some(val);
                }
            }
            "try_recv" => {
                if let Value::Receiver(ref queue) = obj {
                    let opt = queue.lock().unwrap().pop_front();
                    return Some(match opt {
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
                    });
                }
            }
            _ => return None,
        }
        None
    }
}
