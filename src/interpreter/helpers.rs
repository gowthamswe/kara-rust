//! Free helper functions used across the interpreter.
//!
//! Houses value-comparison helpers (`value_compare`, `value_discriminant`),
//! the `eval_stats_fn` stdlib stats dispatch, the encoding family
//! (base64/hex/url encode + decode), I/O result wrappers
//! (`io_ok`/`io_err_value`/`io_error_from_std`), the HTTP request
//! emitters (`eval_http_get`/`eval_http_post`, `make_response`,
//! `make_http_error`, `wrap_ok_response`), and JSON ↔ Value
//! conversion (`serde_json_to_kara_json`, `kara_json_to_serde_json`,
//! `make_json_error`).
//!
//! All `pub(super)` so the `Interpreter` impl in `super` can call them.

use std::collections::HashMap;

use crate::token::Span;

use super::{EnumData, Value};

pub(super) fn value_compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Char(x), Value::Char(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Tuple(xs), Value::Tuple(ys)) => xs
            .iter()
            .zip(ys.iter())
            .find_map(|(a, b)| {
                let ord = value_compare(a, b);
                if ord != Ordering::Equal {
                    Some(ord)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| xs.len().cmp(&ys.len())),
        // Two Maps: lexicographic over (key, value) pairs in insertion order
        (Value::Map(a), Value::Map(b)) => a
            .iter()
            .zip(b.iter())
            .find_map(|((ak, av), (bk, bv))| {
                let k_ord = value_compare(ak, bk);
                if k_ord != Ordering::Equal {
                    Some(k_ord)
                } else {
                    let v_ord = value_compare(av, bv);
                    if v_ord != Ordering::Equal {
                        Some(v_ord)
                    } else {
                        None
                    }
                }
            })
            .unwrap_or_else(|| a.len().cmp(&b.len())),
        // Two SortedSets: lexicographic over their ascending key sequences
        (Value::SortedSet(a), Value::SortedSet(b)) => {
            let ak: Vec<_> = a.keys().collect();
            let bk: Vec<_> = b.keys().collect();
            ak.iter()
                .zip(bk.iter())
                .find_map(|(x, y)| {
                    let ord = value_compare(&x.0, &y.0);
                    if ord != Ordering::Equal {
                        Some(ord)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| ak.len().cmp(&bk.len()))
        }
        // Cross-variant ordering by discriminant index
        _ => value_discriminant(a).cmp(&value_discriminant(b)),
    }
}

pub(super) fn value_discriminant(v: &Value) -> u8 {
    match v {
        Value::Int(_) => 0,
        Value::Float(_) => 1,
        Value::Bool(_) => 2,
        Value::Char(_) => 3,
        Value::String(_) => 4,
        Value::Tuple(_) => 5,
        Value::Array(_) => 6,
        Value::Unit => 7,
        Value::Map(_) => 12,
        Value::SortedSet(_) => 9,
        Value::Set(_) => 13,
        Value::Sender(_) => 10,
        Value::Receiver(_) => 11,
        _ => 8,
    }
}

// ── Stats stdlib helpers ─────────────────────────────────────────────────────

pub(super) fn eval_stats_fn(name: &str, xs: &[f64], span: &Span) -> Value {
    match name {
        "Stats.sum" => Value::Float(xs.iter().sum()),
        "Stats.prod" => Value::Float(xs.iter().product()),
        "Stats.mean" => {
            if xs.is_empty() {
                panic!(
                    "Stats.mean() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            Value::Float(xs.iter().sum::<f64>() / xs.len() as f64)
        }
        "Stats.variance" => {
            if xs.is_empty() {
                panic!(
                    "Stats.variance() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            let mean = xs.iter().sum::<f64>() / xs.len() as f64;
            let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64;
            Value::Float(var)
        }
        "Stats.stddev" => {
            if xs.is_empty() {
                panic!(
                    "Stats.stddev() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            let mean = xs.iter().sum::<f64>() / xs.len() as f64;
            let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64;
            Value::Float(var.sqrt())
        }
        "Stats.median" => {
            if xs.is_empty() {
                panic!(
                    "Stats.median() called on empty slice at {}:{}",
                    span.line, span.column
                );
            }
            let mut sorted = xs.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = sorted.len() / 2;
            let median = if sorted.len().is_multiple_of(2) {
                (sorted[mid - 1] + sorted[mid]) / 2.0
            } else {
                sorted[mid]
            };
            Value::Float(median)
        }
        "Stats.min" => {
            let result = xs.iter().copied().reduce(f64::min);
            match result {
                Some(v) => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "Some".to_string(),
                    data: EnumData::Tuple(vec![Value::Float(v)]),
                },
                None => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "None".to_string(),
                    data: EnumData::Unit,
                },
            }
        }
        "Stats.max" => {
            let result = xs.iter().copied().reduce(f64::max);
            match result {
                Some(v) => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "Some".to_string(),
                    data: EnumData::Tuple(vec![Value::Float(v)]),
                },
                None => Value::EnumVariant {
                    enum_name: "Option".to_string(),
                    variant: "None".to_string(),
                    data: EnumData::Unit,
                },
            }
        }
        _ => Value::Unit,
    }
}

// ── Encoding stdlib helpers (Base64 / Hex / Url) ────────────────────────────

const BASE64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub(super) fn base64_encode(bytes: &[u8], url_safe: bool) -> String {
    let alphabet = if url_safe { BASE64_URL } else { BASE64_STD };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((n >> 18) & 0x3f) as usize] as char);
        out.push(alphabet[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() >= 2 {
            out.push(alphabet[((n >> 6) & 0x3f) as usize] as char);
        } else if !url_safe {
            out.push('=');
        }
        if chunk.len() == 3 {
            out.push(alphabet[(n & 0x3f) as usize] as char);
        } else if !url_safe {
            out.push('=');
        }
    }
    out
}

pub(super) fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn decode_char(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let trimmed = s.trim_end_matches('=');
    let mut bytes = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut buf = [0u8; 4];
    let mut n = 0;
    for c in trimmed.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let v =
            decode_char(c).ok_or_else(|| format!("invalid base64 character: {:?}", c as char))?;
        buf[n] = v;
        n += 1;
        if n == 4 {
            bytes.push((buf[0] << 2) | (buf[1] >> 4));
            bytes.push((buf[1] << 4) | (buf[2] >> 2));
            bytes.push((buf[2] << 6) | buf[3]);
            n = 0;
        }
    }
    match n {
        0 => {}
        1 => return Err("invalid base64 length: trailing single character".to_string()),
        2 => bytes.push((buf[0] << 2) | (buf[1] >> 4)),
        3 => {
            bytes.push((buf[0] << 2) | (buf[1] >> 4));
            bytes.push((buf[1] << 4) | (buf[2] >> 2));
        }
        _ => unreachable!(),
    }
    Ok(bytes)
}

pub(super) fn hex_encode(bytes: &[u8], upper: bool) -> String {
    let lut: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(lut[(b >> 4) as usize] as char);
        out.push(lut[(b & 0xf) as usize] as char);
    }
    out
}

pub(super) fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    fn from_hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bs = s.as_bytes();
    if !bs.len().is_multiple_of(2) {
        return Err(format!("invalid hex length: {} (must be even)", bs.len()));
    }
    let mut out = Vec::with_capacity(bs.len() / 2);
    for chunk in bs.chunks(2) {
        let hi = from_hex(chunk[0])
            .ok_or_else(|| format!("invalid hex character: {:?}", chunk[0] as char))?;
        let lo = from_hex(chunk[1])
            .ok_or_else(|| format!("invalid hex character: {:?}", chunk[1] as char))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

pub(super) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

pub(super) fn url_decode(s: &str) -> Result<String, String> {
    fn from_hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bs = s.as_bytes();
    let mut out = Vec::with_capacity(bs.len());
    let mut i = 0;
    while i < bs.len() {
        if bs[i] == b'%' {
            if i + 2 >= bs.len() {
                return Err("incomplete percent-encoded sequence at end of input".to_string());
            }
            let hi = from_hex(bs[i + 1]).ok_or_else(|| {
                format!(
                    "invalid percent-encoded byte: %{}{}",
                    bs[i + 1] as char,
                    bs[i + 2] as char
                )
            })?;
            let lo = from_hex(bs[i + 2]).ok_or_else(|| {
                format!(
                    "invalid percent-encoded byte: %{}{}",
                    bs[i + 1] as char,
                    bs[i + 2] as char
                )
            })?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bs[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|e| format!("invalid UTF-8 in decoded URL: {e}"))
}

pub(super) fn decode_ok_bytes(bytes: Vec<u8>) -> Value {
    let arr: Vec<Value> = bytes.into_iter().map(|b| Value::Int(b as i64)).collect();
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![Value::array_of(arr)]),
    }
}

pub(super) fn decode_ok_string(s: String) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![Value::String(s)]),
    }
}

pub(super) fn decode_err(message: String) -> Value {
    let mut fields = HashMap::new();
    fields.insert("message".to_string(), Value::String(message));
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Err".to_string(),
        data: EnumData::Tuple(vec![Value::Struct {
            name: "DecodeError".to_string(),
            fields,
        }]),
    }
}

// ── I/O stdlib helpers ──────────────────────────────────────────────────────

pub(super) fn io_ok(val: Value) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![val]),
    }
}

pub(super) fn io_err_value(io_error: Value) -> Value {
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Err".to_string(),
        data: EnumData::Tuple(vec![io_error]),
    }
}

pub(super) fn io_error_from_std(e: &std::io::Error) -> Value {
    let (variant, payload) = match e.kind() {
        std::io::ErrorKind::NotFound => ("NotFound", None),
        std::io::ErrorKind::PermissionDenied => ("PermissionDenied", None),
        std::io::ErrorKind::AlreadyExists => ("AlreadyExists", None),
        std::io::ErrorKind::UnexpectedEof => ("UnexpectedEof", None),
        std::io::ErrorKind::InvalidData => ("InvalidUtf8", None),
        std::io::ErrorKind::Interrupted => ("Interrupted", None),
        _ => ("Other", Some(e.to_string())),
    };
    Value::EnumVariant {
        enum_name: "IoError".to_string(),
        variant: variant.to_string(),
        data: match payload {
            None => EnumData::Unit,
            Some(msg) => EnumData::Tuple(vec![Value::String(msg)]),
        },
    }
}

// ── std.http helpers ──────────────────────────────────────────────────────────

pub(super) fn make_response(status: u16, body: String, headers: Vec<(String, String)>) -> Value {
    let mut fields = HashMap::new();
    fields.insert("status".to_string(), Value::Int(status as i64));
    fields.insert("body".to_string(), Value::String(body));
    let header_pairs: Vec<Value> = headers
        .into_iter()
        .map(|(k, v)| Value::Tuple(vec![Value::String(k), Value::String(v)]))
        .collect();
    // Store headers as a flat Vec<(k,v)> in a Map value for header() lookup.
    let map_pairs: Vec<(Value, Value)> = header_pairs
        .iter()
        .filter_map(|v| {
            if let Value::Tuple(ref kv) = v {
                if kv.len() == 2 {
                    return Some((kv[0].clone(), kv[1].clone()));
                }
            }
            None
        })
        .collect();
    fields.insert("headers".to_string(), Value::Map(map_pairs));
    Value::Struct {
        name: "Response".to_string(),
        fields,
    }
}

pub(super) fn make_http_error(message: String) -> Value {
    let mut fields = HashMap::new();
    fields.insert("message".to_string(), Value::String(message));
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Err".to_string(),
        data: EnumData::Tuple(vec![Value::Struct {
            name: "HttpError".to_string(),
            fields,
        }]),
    }
}

pub(super) fn wrap_ok_response(resp: ureq::Response) -> Value {
    let status = resp.status();
    // Collect headers before consuming the response.
    let content_type = resp.header("content-type").unwrap_or("").to_string();
    let body = resp.into_string().unwrap_or_default();
    let mut headers = Vec::new();
    if !content_type.is_empty() {
        headers.push(("content-type".to_string(), content_type));
    }
    Value::EnumVariant {
        enum_name: "Result".to_string(),
        variant: "Ok".to_string(),
        data: EnumData::Tuple(vec![make_response(status, body, headers)]),
    }
}

pub(super) fn eval_http_get(url: &str) -> Value {
    match ureq::get(url).call() {
        Ok(resp) => wrap_ok_response(resp),
        Err(e) => make_http_error(e.to_string()),
    }
}

// ── Slice F (`std.json`) helpers ─────────────────────────────────────────
//
// Translation between `serde_json::Value` and the Kāra `Json` enum
// (modeled as `Value::EnumVariant { enum_name: "Json", ... }`). The
// interpreter dispatches `Json.parse(s)` and `j.stringify()` directly
// against `serde_json` rather than crossing the runtime FFI surface —
// the runtime crate's `karac_runtime_json_*` exports exist for codegen
// builds (Slice B's `Response.json[T: ToJson]` builder, deferred), but
// going through them from the interpreter is pure overhead since both
// sides link the same `serde_json` version.

/// Build a Kāra `Json` enum value from a `serde_json::Value` tree.
pub(super) fn serde_json_to_kara_json(v: &serde_json::Value) -> Value {
    let (variant, data) = match v {
        serde_json::Value::Null => ("Null", EnumData::Unit),
        serde_json::Value::Bool(b) => ("Bool", EnumData::Tuple(vec![Value::Bool(*b)])),
        serde_json::Value::Number(n) => (
            "Number",
            EnumData::Tuple(vec![Value::Float(n.as_f64().unwrap_or(0.0))]),
        ),
        serde_json::Value::String(s) => ("String", EnumData::Tuple(vec![Value::String(s.clone())])),
        serde_json::Value::Array(items) => {
            let xs: Vec<Value> = items.iter().map(serde_json_to_kara_json).collect();
            ("Array", EnumData::Tuple(vec![Value::array_of(xs)]))
        }
        serde_json::Value::Object(map) => {
            // Locked design (ii): Object backs a `Vec[(String, Json)]`.
            // The interpreter shape is `Value::Array` of `Value::Tuple`s.
            let pairs: Vec<Value> = map
                .iter()
                .map(|(k, val)| {
                    Value::Tuple(vec![Value::String(k.clone()), serde_json_to_kara_json(val)])
                })
                .collect();
            ("Object", EnumData::Tuple(vec![Value::array_of(pairs)]))
        }
    };
    Value::EnumVariant {
        enum_name: "Json".to_string(),
        variant: variant.to_string(),
        data,
    }
}

/// Inverse: walk a Kāra `Json` value and produce a `serde_json::Value`
/// for `serde_json::to_string`. Reads the variant tag off the
/// `EnumVariant`'s `variant` string and pulls the payload out of the
/// `EnumData::Tuple` slot. Mismatched shapes degrade to `null` rather
/// than panicking — pre-typecheck guarantees match the legal shape, but
/// defensiveness here keeps stringify side-effect-free under stress.
pub(super) fn kara_json_to_serde_json(v: &Value) -> serde_json::Value {
    let Value::EnumVariant {
        enum_name,
        variant,
        data,
    } = v
    else {
        return serde_json::Value::Null;
    };
    if enum_name != "Json" {
        return serde_json::Value::Null;
    }
    let payload = match data {
        EnumData::Unit => Vec::new(),
        EnumData::Tuple(vals) => vals.clone(),
        EnumData::Struct(_) => Vec::new(),
    };
    match variant.as_str() {
        "Null" => serde_json::Value::Null,
        "Bool" => match payload.first() {
            Some(Value::Bool(b)) => serde_json::Value::Bool(*b),
            _ => serde_json::Value::Null,
        },
        "Number" => match payload.first() {
            Some(Value::Float(f)) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Some(Value::Int(i)) => serde_json::Number::from_f64(*i as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Some(Value::TotalFloat64(f)) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Some(Value::TotalFloat32(f)) => serde_json::Number::from_f64(*f as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            _ => serde_json::Value::Null,
        },
        "String" => match payload.first() {
            Some(Value::String(s)) => serde_json::Value::String(s.clone()),
            _ => serde_json::Value::Null,
        },
        "Array" => match payload.first() {
            Some(Value::Array(rc)) => {
                let items: Vec<serde_json::Value> = rc
                    .read()
                    .unwrap()
                    .iter()
                    .map(kara_json_to_serde_json)
                    .collect();
                serde_json::Value::Array(items)
            }
            _ => serde_json::Value::Null,
        },
        "Object" => match payload.first() {
            Some(Value::Array(rc)) => {
                let mut map = serde_json::Map::with_capacity(rc.read().unwrap().len());
                for entry in rc.read().unwrap().iter() {
                    if let Value::Tuple(t) = entry {
                        if t.len() == 2 {
                            if let Value::String(k) = &t[0] {
                                map.insert(k.clone(), kara_json_to_serde_json(&t[1]));
                            }
                        }
                    }
                }
                serde_json::Value::Object(map)
            }
            _ => serde_json::Value::Null,
        },
        _ => serde_json::Value::Null,
    }
}

/// Build a `JsonError` struct value from `serde_json::Error`.
pub(super) fn make_json_error(e: &serde_json::Error) -> Value {
    let mut fields = HashMap::new();
    fields.insert("line".to_string(), Value::Int(e.line() as i64));
    fields.insert("column".to_string(), Value::Int(e.column() as i64));
    fields.insert("message".to_string(), Value::String(e.to_string()));
    Value::Struct {
        name: "JsonError".to_string(),
        fields,
    }
}

pub(super) fn eval_http_post(url: &str, body: &str) -> Value {
    match ureq::post(url).send_string(body) {
        Ok(resp) => wrap_ok_response(resp),
        Err(e) => make_http_error(e.to_string()),
    }
}
