// tests/json.rs
//
// Slice F (`std.json`) — v1 surface tests. Covers the locked design
// invariants: round-trip on objects / arrays / scalars, parse-error
// `line`/`column` surface, insertion-order Object iteration, manual
// `ToJson` impl on a user struct.
//
// All six tests run through the tree-walk interpreter (no `--features
// llvm` gate); the codegen-side wiring lands in a sibling slice as part
// of Slice B's `Response.json[T: ToJson]` builder. The runtime-crate
// FFI exports (`karac_runtime_json_*`) are exercised separately by the
// `karac_runtime::tests::test_karac_runtime_json_*` unit tests at the
// bottom of `runtime/src/lib.rs`.

use karac::run_program;

fn run(source: &str) -> String {
    run_program(source).join("")
}

#[test]
fn test_json_parse_roundtrip_object() {
    // Locked design (i): all numbers stringify as f64 (`1` → `1.0`).
    // Locked design (ii): Object keys preserved in input order.
    let output = run(
        "fn main() {\n\
             match Json.parse(\"{\\\"a\\\": 1, \\\"b\\\": \\\"hello\\\", \\\"c\\\": [true, null]}\") {\n\
                 Ok(j) => println(j.stringify()),\n\
                 Err(e) => println(\"err\"),\n\
             }\n\
         }",
    );
    assert_eq!(
        output, "{\"a\":1.0,\"b\":\"hello\",\"c\":[true,null]}\n",
        "object round-trip should preserve keys and stringify numbers as f64"
    );
}

#[test]
fn test_json_parse_roundtrip_array() {
    let output = run("fn main() {\n\
             match Json.parse(\"[1, 2.5, \\\"x\\\", true, null]\") {\n\
                 Ok(j) => println(j.stringify()),\n\
                 Err(e) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "[1.0,2.5,\"x\",true,null]\n");
}

#[test]
fn test_json_parse_roundtrip_primitives() {
    // One scalar at a time — number, string, bool, null. Each goes
    // through parse + stringify and must come back byte-equivalent
    // (modulo the int → f64 stringification rule from locked design (i)).
    let output = run("fn main() {\n\
             match Json.parse(\"42\") {\n\
                 Ok(j) => println(j.stringify()),\n\
                 Err(e) => println(\"err\"),\n\
             }\n\
             match Json.parse(\"\\\"hi\\\"\") {\n\
                 Ok(j) => println(j.stringify()),\n\
                 Err(e) => println(\"err\"),\n\
             }\n\
             match Json.parse(\"true\") {\n\
                 Ok(j) => println(j.stringify()),\n\
                 Err(e) => println(\"err\"),\n\
             }\n\
             match Json.parse(\"null\") {\n\
                 Ok(j) => println(j.stringify()),\n\
                 Err(e) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "42.0\n\"hi\"\ntrue\nnull\n");
}

#[test]
fn test_json_parse_error_surfaces_line_col() {
    // Malformed input `{"a": }` — serde_json reports the error at line
    // 1, column 7 (the offending `}` byte). Locked design (iv): the
    // JsonError carries line + column from `serde_json::Error::line()` /
    // `column()`.
    let output = run("fn main() {\n\
             match Json.parse(\"{\\\"a\\\": }\") {\n\
                 Ok(j) => println(\"ok\"),\n\
                 Err(e) => {\n\
                     println(e.line);\n\
                     println(e.column);\n\
                 },\n\
             }\n\
         }");
    assert_eq!(
        output, "1\n7\n",
        "JsonError should carry serde_json's 1-indexed line + column"
    );
}

#[test]
fn test_json_object_preserves_insertion_order() {
    // Locked design (ii): Object iterates in input insertion order, NOT
    // alphabetical. Backed by `Vec[(String, Json)]` on the Kāra side
    // and `serde_json` with `preserve_order` on the Rust side. If this
    // test fails with `{"a":2.0,"m":3.0,"z":1.0}` (alphabetical), the
    // `preserve_order` feature was dropped from the runtime crate's
    // `serde_json` dependency.
    let output = run("fn main() {\n\
             match Json.parse(\"{\\\"z\\\": 1, \\\"a\\\": 2, \\\"m\\\": 3}\") {\n\
                 Ok(j) => println(j.stringify()),\n\
                 Err(e) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "{\"z\":1.0,\"a\":2.0,\"m\":3.0}\n");
}

#[test]
fn test_to_json_manual_impl() {
    // Manual `ToJson` impl on a user struct. Locked design (v):
    // derived `#[derive(ToJson)]` ships in v1.5; v1 is hand-written.
    // The impl builds an `Object` variant via a pair-Vec literal, then
    // `stringify` produces the expected JSON.
    let output = run("struct Point { x: i32, y: i32 }\n\
         \n\
         impl ToJson for Point {\n\
             fn to_json(self) -> Json {\n\
                 let pairs = [\n\
                     (\"x\", Json.Number(self.x as f64)),\n\
                     (\"y\", Json.Number(self.y as f64)),\n\
                 ];\n\
                 Json.Object(pairs)\n\
             }\n\
         }\n\
         \n\
         fn main() {\n\
             let p = Point { x: 1, y: 2 };\n\
             println(p.to_json().stringify());\n\
         }");
    assert_eq!(output, "{\"x\":1.0,\"y\":2.0}\n");
}
