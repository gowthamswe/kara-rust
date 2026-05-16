//! HTTP-method dispatch — the bodies of the `post`/`path`/`method`/
//! `status`/`body`/`header`/`message` arms lifted out of
//! `eval_method_call`. These handle Client, Request, Response, and
//! HttpError receiver shapes.

use crate::ast::*;
use crate::token::Span;

use super::helpers::eval_http_post;
use super::value::{EnumData, Value};

impl<'a> super::Interpreter<'a> {
    pub(super) fn try_eval_http_method(
        &mut self,
        method: &str,
        obj: Value,
        args: &[CallArg],
        _span: &Span,
    ) -> Option<Value> {
        match method {
            // ── Client method dispatch ────────────────────────────────────────
            "post" => {
                if let Value::Struct { ref name, .. } = obj {
                    if name == "Client" {
                        let mut arg_iter = args.iter();
                        let url = arg_iter
                            .next()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        let body = arg_iter
                            .next()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        return Some(eval_http_post(&url, &body));
                    }
                }
            }
            // ── Request method dispatch (HTTP handler ABI trampoline, 2026-05-09) ──
            // F2 owned-String contract: each call returns a freshly-cloned
            // `Value::String`, so multiple calls to `req.path()` / `.method()`
            // never collide on a borrowed buffer. v1 returns an empty String
            // — the interpreter doesn't run a real HTTP server, so there's
            // no real path/method to surface. Pinned by
            // `tests/interpreter.rs::test_server_serve_handler_request_path_returns_owned_string`.
            "path" | "method" if matches!(&obj, Value::Struct { name, .. } if name == "Request") => {
                return Some(Value::String(String::new()));
            }
            // ── Response / HttpError method dispatch ──────────────────────────
            "status" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Response" {
                        if let Some(v) = fields.get("status") {
                            return Some(v.clone());
                        }
                        return Some(Value::Int(0));
                    }
                }
            }
            "body" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Response" {
                        if let Some(v) = fields.get("body") {
                            return Some(v.clone());
                        }
                        return Some(Value::String(String::new()));
                    }
                }
            }
            "header" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "Response" {
                        let header_name = args
                            .first()
                            .map(|a| match self.eval_expr_inner(&a.value) {
                                Value::String(s) => s,
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        // Headers are stored as a Map field (key → value strings).
                        if let Some(Value::Map(ref pairs)) = fields.get("headers") {
                            for (k, v) in pairs {
                                if let (Value::String(k_str), Value::String(v_str)) = (k, v) {
                                    if k_str.eq_ignore_ascii_case(&header_name) {
                                        return Some(Value::EnumVariant {
                                            enum_name: "Option".to_string(),
                                            variant: "Some".to_string(),
                                            data: EnumData::Tuple(vec![Value::String(
                                                v_str.clone(),
                                            )]),
                                        });
                                    }
                                }
                            }
                        }
                        return Some(Value::EnumVariant {
                            enum_name: "Option".to_string(),
                            variant: "None".to_string(),
                            data: EnumData::Unit,
                        });
                    }
                }
            }
            "message" => {
                if let Value::Struct {
                    ref name,
                    ref fields,
                } = obj
                {
                    if name == "HttpError" {
                        if let Some(v) = fields.get("message") {
                            return Some(v.clone());
                        }
                        return Some(Value::String(String::new()));
                    }
                }
            }
            _ => return None,
        }
        None
    }
}
