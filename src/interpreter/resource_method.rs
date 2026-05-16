//! Resource-provider method dispatch.
//!
//! Houses `eval_resource_method` (the entry from `eval_call` / method
//! dispatch on a `Resource.method(...)` shape), the impl-table lookup
//! arm (`dispatch_resource_method_with_values` — finds and invokes
//! the user-supplied impl), and the ambient-default fallback arm
//! (`dispatch_builtin_resource_method_with_values` — built-in
//! implementations for `Time.now`, `Random.next`, `Console.print`,
//! etc. when no provider is installed).
//!
//! Lives in a sibling `impl<'a> super::Interpreter<'a>` block.

use crate::ast::*;
use crate::token::Span;

use super::exec::ControlFlow;
use super::helpers::{io_err_value, io_error_from_std, io_ok};
use super::value::{EnumData, Value};

impl<'a> super::Interpreter<'a> {
    /// Dispatch `Resource.method(...)` by looking up the active provider for
    /// `Resource` on the provider stack and invoking `method` on the stored
    /// provider value. The value's concrete type (e.g. `InMemoryUserDB`) feeds
    /// the standard impl-block method table — so any `impl Trait for P` whose
    /// bounds satisfy the resource's provider-trait contract resolves
    /// correctly without a vtable. Missing provider bindings produce a
    /// runtime error: the typechecker accepts the call because the effect
    /// declares the resource, but at runtime no `with_provider` scope or
    /// ambient default installed the binding.
    pub(crate) fn eval_resource_method(
        &mut self,
        resource: &str,
        method: &str,
        args: &[CallArg],
        span: &Span,
    ) -> Value {
        let arg_vals: Vec<Value> = args
            .iter()
            .map(|a| self.eval_expr_inner(&a.value))
            .collect();
        if self.check_cf() {
            return Value::Unit;
        }
        self.dispatch_resource_method_with_values(resource, method, arg_vals, span)
    }

    /// Pre-evaluated-args entry into the provider-stack dispatch path.
    /// Same lookup / BuiltinDefault / user-provider routing as
    /// [`eval_resource_method`], but skips argument evaluation so callers
    /// that compute their args via a different path (e.g. the print
    /// router that formats a `Display` value into a `String` before
    /// dispatch) can share the same final dispatch.
    pub(crate) fn dispatch_resource_method_with_values(
        &mut self,
        resource: &str,
        method: &str,
        mut arg_vals: Vec<Value>,
        span: &Span,
    ) -> Value {
        let Some(provider_arc) = self.lookup_provider(resource) else {
            return self.record_runtime_error(
                format!(
                    "no provider bound for resource '{}'; \
                     call `with_provider[{}](..., || {{ ... }})` to scope one",
                    resource, resource
                ),
                span,
            );
        };

        let provider = (*provider_arc).clone();
        let type_name = self.value_type_name(&provider);

        // Ambient program-rooted resources: the default provider is a
        // zero-field `BuiltinDefault<R>` struct (see `register_items`).
        // Dispatch its methods in Rust — `Clock.now()` returns the current
        // Unix timestamp in seconds, etc. User-declared resources never
        // start with the `BuiltinDefault` prefix, so the check is safe.
        if let Some(resource_name) = type_name.strip_prefix("BuiltinDefault") {
            return self.dispatch_builtin_resource_method_with_values(
                resource_name,
                method,
                arg_vals,
                span,
            );
        }

        let method_key = format!("{}.{}", type_name, method);

        let Some(func) = self.env.get(&method_key) else {
            return self.record_runtime_error(
                format!(
                    "provider type '{}' bound to resource '{}' has no method '{}'",
                    type_name, resource, method
                ),
                span,
            );
        };

        let Value::Function {
            param_patterns,
            param_defaults,
            body,
            closure_env,
            ..
        } = func
        else {
            return self.record_runtime_error(
                format!("method '{}.{}' is not callable", type_name, method),
                span,
            );
        };

        // Prepend the provider as the implicit `self` argument.
        arg_vals.insert(0, provider);

        self.env.push_scope();
        if let Some(ref captured) = closure_env {
            for (k, v) in captured {
                self.env.define(k.clone(), v.clone());
            }
        }
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
        match result {
            Ok(v) => v,
            Err(ControlFlow::Return(v)) => v,
            Err(cf) => self.set_cf(cf),
        }
    }

    /// Dispatch a method call against the default provider for an ambient
    /// program-rooted resource. Called from [`eval_resource_method`] when
    /// the provider's type name has the `BuiltinDefault` prefix — i.e., no
    /// user `with_provider` has shadowed it yet. Each primitive's method
    /// surface is hand-coded here; the set grows as additional primitives
    /// land under `PRELUDE_EFFECT_RESOURCES`.
    /// BuiltinDefault dispatch path. Used by the provider-stack router
    /// when no user `with_provider` has shadowed the resource — and by
    /// the print/println router which formats a `Display` value into a
    /// `String` and calls through the same arms a direct
    /// `Stdout.println(s)` call would hit.
    fn dispatch_builtin_resource_method_with_values(
        &mut self,
        resource: &str,
        method: &str,
        arg_vals: Vec<Value>,
        span: &Span,
    ) -> Value {
        match (resource, method) {
            ("Clock", "now") => {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Value::Int(secs)
            }
            ("RandomSource", "next_u64") => {
                // Xorshift64 — adequate for the interpreter's non-cryptographic
                // use; real entropy comes through LLVM codegen later. The
                // `u64 as i64` cast is lossless bit-for-bit and matches the
                // Clock arm's convention for fitting wider values into
                // `Value::Int`.
                let mut x = self.rand_state;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.rand_state = x;
                Value::Int(x as i64)
            }
            ("Env", "args") => {
                // Process argv as `Vec[String]`. `std::env::args()` is
                // platform-safe and includes the binary path as element 0,
                // matching the Kāra spec's `env.args()` surface (design.md
                // § Built-in Resources — Nondeterminism, line 2799). Lossy
                // conversion for non-UTF-8 argv: `std::env::args` itself
                // panics in that case, same as Rust's convention.
                let vals: Vec<Value> = std::env::args().map(Value::String).collect();
                Value::array_of(vals)
            }
            ("Env", "var") => {
                // `env.var(name) -> Result[String, VarError]` per design.md
                // § Built-in Resources line 2799. `VarError` shape settled
                // in brainstorming v49: single `NotPresent` variant, no
                // payload. `std::env::var` returns `Err(NotPresent)` for
                // missing vars and `Err(NotUnicode)` for non-UTF-8 values
                // — we collapse both to `VarError.NotPresent` since Kāra's
                // strict-UTF-8 `String` cannot carry the offending bytes.
                let name = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Env.var expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                match std::env::var(&name) {
                    Ok(v) => Value::EnumVariant {
                        enum_name: "Result".to_string(),
                        variant: "Ok".to_string(),
                        data: EnumData::Tuple(vec![Value::String(v)]),
                    },
                    Err(_) => Value::EnumVariant {
                        enum_name: "Result".to_string(),
                        variant: "Err".to_string(),
                        data: EnumData::Tuple(vec![Value::EnumVariant {
                            enum_name: "VarError".to_string(),
                            variant: "NotPresent".to_string(),
                            data: EnumData::Unit,
                        }]),
                    },
                }
            }
            ("Env", "set") => {
                // `env.set(name, value) -> Unit` with `writes(Env)`. POSIX
                // `setenv` shape — overwrites if already present, creates if
                // absent. Companion to `Env.var` and `Env.args`. The runtime
                // crate is Rust 2021 edition, where `std::env::set_var` is
                // safe; the safety contract (no concurrent reads of the
                // environment block on other threads) is upheld here because
                // the interpreter is single-threaded at this surface.
                self.track_effect("writes(Env)");
                let name = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Env.set expects a String name argument".to_string(),
                            span,
                        );
                    }
                };
                let value = match arg_vals.get(1) {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Env.set expects a String value argument".to_string(),
                            span,
                        );
                    }
                };
                std::env::set_var(&name, &value);
                Value::Unit
            }
            // ── Stdin ──────────────────────────────────────────────
            ("Stdin", "read_line") => {
                self.track_effect("reads(Stdin)");
                let mut buf = String::new();
                match std::io::stdin().read_line(&mut buf) {
                    Ok(_) => io_ok(Value::String(buf)),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }
            ("Stdin", "read_to_string") => {
                self.track_effect("reads(Stdin)");
                let mut buf = String::new();
                use std::io::Read;
                match std::io::stdin().read_to_string(&mut buf) {
                    Ok(_) => io_ok(Value::String(buf)),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }

            // ── Stdout / Stderr ────────────────────────────────────
            ("Stdout", "print") => {
                self.track_effect("writes(Stdout)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stdout.print expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stdout(&s, false);
                Value::Unit
            }
            ("Stdout", "println") => {
                self.track_effect("writes(Stdout)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stdout.println expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stdout(&s, true);
                Value::Unit
            }
            ("Stdout", "flush") => {
                self.track_effect("writes(Stdout)");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Value::Unit
            }
            ("Stderr", "print") => {
                self.track_effect("writes(Stderr)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stderr.print expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stderr(&s, false);
                Value::Unit
            }
            ("Stderr", "println") => {
                self.track_effect("writes(Stderr)");
                let s = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "Stderr.println expects a String argument".to_string(),
                            span,
                        );
                    }
                };
                self.write_stderr(&s, true);
                Value::Unit
            }
            ("Stderr", "flush") => {
                self.track_effect("writes(Stderr)");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                Value::Unit
            }

            // ── FileSystem ─────────────────────────────────────────
            ("FileSystem", "read_to_string") => {
                self.track_effect("reads(FileSystem)");
                let path = match arg_vals.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        return self.record_runtime_error(
                            "FileSystem.read_to_string expects a String path".to_string(),
                            span,
                        );
                    }
                };
                match std::fs::read_to_string(&path) {
                    Ok(contents) => io_ok(Value::String(contents)),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }
            ("FileSystem", "write") => {
                self.track_effect("writes(FileSystem)");
                let (path, contents) = match (arg_vals.first(), arg_vals.get(1)) {
                    (Some(Value::String(p)), Some(Value::String(c))) => (p.clone(), c.clone()),
                    _ => {
                        return self.record_runtime_error(
                            "FileSystem.write expects (String path, String contents)".to_string(),
                            span,
                        );
                    }
                };
                match std::fs::write(&path, contents.as_bytes()) {
                    Ok(()) => io_ok(Value::Unit),
                    Err(e) => io_err_value(io_error_from_std(&e)),
                }
            }

            _ => self.record_runtime_error(
                format!(
                    "ambient resource '{}' has no default method '{}' yet",
                    resource, method
                ),
                span,
            ),
        }
    }
}
