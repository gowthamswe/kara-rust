//! Process-method dispatch — `Command.spawn`, `Child.wait` / `try_wait`
//! / `kill`. Uses `std::process` under the hood.
//!
//! Storage: `Interpreter.child_table: HashMap<i64, std::process::Child>`
//! keyed by OS pid. `spawn` populates it; `wait` removes on success;
//! `try_wait` removes when the child has exited; `kill` leaves the
//! entry in place (caller still needs to `wait` to reap the process).
//!
//! Pid as the key is safe in the single-threaded tree-walk interpreter
//! because each `spawn` produces a fresh entry before any code can
//! observe a pid reuse — the OS guarantees pid uniqueness while the
//! child is in our table.

use std::collections::HashMap;
use std::process::Command as StdCommand;

use crate::ast::*;
use crate::token::Span;

use super::value::{EnumData, Value};

impl<'a> super::Interpreter<'a> {
    pub(super) fn try_eval_process_method(
        &mut self,
        method: &str,
        obj: Value,
        _args: &[CallArg],
        _span: &Span,
    ) -> Option<Value> {
        match method {
            "spawn" => self.eval_command_spawn(obj),
            "wait" => self.eval_child_wait(obj),
            "try_wait" => self.eval_child_try_wait(obj),
            "kill" => self.eval_child_kill(obj),
            _ => None,
        }
    }

    fn eval_command_spawn(&mut self, obj: Value) -> Option<Value> {
        let Value::Struct { name, fields } = &obj else {
            return None;
        };
        if name != "Command" {
            return None;
        }
        let program = read_string_field(fields, "program");
        let cmd_args = read_string_vec_field(fields, "cmd_args");
        let cmd_env = read_env_var_vec_field(fields, "cmd_env");

        let mut std_cmd = StdCommand::new(&program);
        for a in &cmd_args {
            std_cmd.arg(a);
        }
        for (k, v) in &cmd_env {
            std_cmd.env(k, v);
        }
        match std_cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as i64;
                self.child_table.insert(pid, child);
                let mut child_fields = HashMap::new();
                child_fields.insert("pid".to_string(), Value::Int(pid));
                Some(result_ok(Value::Struct {
                    name: "Child".to_string(),
                    fields: child_fields,
                }))
            }
            Err(e) => Some(result_err(io_error_variant_from(&e))),
        }
    }

    fn eval_child_wait(&mut self, obj: Value) -> Option<Value> {
        let pid = child_pid(&obj)?;
        match self.child_table.remove(&pid) {
            Some(mut child) => match child.wait() {
                Ok(status) => Some(result_ok(exit_status_value(status))),
                Err(e) => Some(result_err(io_error_variant_from(&e))),
            },
            None => Some(result_err(io_not_found())),
        }
    }

    fn eval_child_try_wait(&mut self, obj: Value) -> Option<Value> {
        let pid = child_pid(&obj)?;
        let result = match self.child_table.get_mut(&pid) {
            Some(child) => child.try_wait(),
            None => return Some(result_err(io_not_found())),
        };
        match result {
            Ok(Some(status)) => {
                self.child_table.remove(&pid);
                Some(result_ok(option_some(exit_status_value(status))))
            }
            Ok(None) => Some(result_ok(option_none())),
            Err(e) => Some(result_err(io_error_variant_from(&e))),
        }
    }

    fn eval_child_kill(&mut self, obj: Value) -> Option<Value> {
        let pid = child_pid(&obj)?;
        let result = match self.child_table.get_mut(&pid) {
            Some(child) => child.kill(),
            None => return Some(result_err(io_not_found())),
        };
        match result {
            Ok(()) => Some(result_ok(Value::Unit)),
            Err(e) => Some(result_err(io_error_variant_from(&e))),
        }
    }
}

// ── Receiver-shape helpers ────────────────────────────────────────

fn child_pid(obj: &Value) -> Option<i64> {
    let Value::Struct { name, fields } = obj else {
        return None;
    };
    if name != "Child" {
        return None;
    }
    match fields.get("pid") {
        Some(Value::Int(p)) => Some(*p),
        _ => None,
    }
}

fn read_string_field(fields: &HashMap<String, Value>, key: &str) -> String {
    match fields.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn read_string_vec_field(fields: &HashMap<String, Value>, key: &str) -> Vec<String> {
    let Some(Value::Array(arc)) = fields.get(key) else {
        return Vec::new();
    };
    arc.read()
        .unwrap()
        .iter()
        .filter_map(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn read_env_var_vec_field(fields: &HashMap<String, Value>, key: &str) -> Vec<(String, String)> {
    let Some(Value::Array(arc)) = fields.get(key) else {
        return Vec::new();
    };
    arc.read()
        .unwrap()
        .iter()
        .filter_map(|v| {
            let Value::Struct { name, fields } = v else {
                return None;
            };
            if name != "EnvVar" {
                return None;
            }
            let k = match fields.get("key") {
                Some(Value::String(s)) => s.clone(),
                _ => return None,
            };
            let val = match fields.get("value") {
                Some(Value::String(s)) => s.clone(),
                _ => return None,
            };
            Some((k, val))
        })
        .collect()
}

// ── Kāra-value constructors ───────────────────────────────────────

fn result_ok(v: Value) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![v]),
    }
}

fn result_err(v: Value) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Err".to_string(),
        data: EnumData::Tuple(vec![v]),
    }
}

fn option_some(v: Value) -> Value {
    Value::EnumVariant {
        enum_name: "Option".to_string(),
        variant: "Some".to_string(),
        data: EnumData::Tuple(vec![v]),
    }
}

fn option_none() -> Value {
    Value::EnumVariant {
        enum_name: "Option".to_string(),
        variant: "None".to_string(),
        data: EnumData::Unit,
    }
}

fn io_not_found() -> Value {
    Value::EnumVariant {
        enum_name: "IoError".to_string(),
        variant: "NotFound".to_string(),
        data: EnumData::Unit,
    }
}

// Map a `std::io::Error` to the matching `IoError` variant. Falls
// back to `Other(message)` for kinds outside our enum so the user
// always gets a tagged Err rather than a panic.
fn io_error_variant_from(e: &std::io::Error) -> Value {
    use std::io::ErrorKind as K;
    let (variant, payload): (&str, Option<Value>) = match e.kind() {
        K::NotFound => ("NotFound", None),
        K::PermissionDenied => ("PermissionDenied", None),
        K::AlreadyExists => ("AlreadyExists", None),
        K::UnexpectedEof => ("UnexpectedEof", None),
        K::InvalidData => ("InvalidUtf8", None),
        K::Interrupted => ("Interrupted", None),
        _ => ("Other", Some(Value::String(e.to_string()))),
    };
    Value::EnumVariant {
        enum_name: "IoError".to_string(),
        variant: variant.to_string(),
        data: match payload {
            Some(p) => EnumData::Tuple(vec![p]),
            None => EnumData::Unit,
        },
    }
}

fn exit_status_value(status: std::process::ExitStatus) -> Value {
    let code = status.code().unwrap_or(-1) as i64;
    let success = status.success();
    let mut fields = HashMap::new();
    fields.insert("code".to_string(), Value::Int(code));
    fields.insert("success".to_string(), Value::Bool(success));
    Value::Struct {
        name: "ExitStatus".to_string(),
        fields,
    }
}
