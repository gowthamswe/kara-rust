// Tells Cargo to link libkarac_runtime.a (built via `cargo build -p karac-runtime --release`
// from the parent karac-rust workspace) into config_c only.
// Cargo applies link directives to all binaries in the package; that's fine — config_a and
// config_b never reference karac_map_* symbols, so the linker drops the archive for them.

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let runtime_dir = format!("{}/../../target/release", manifest_dir);
    println!("cargo:rustc-link-search=native={}", runtime_dir);
    println!("cargo:rustc-link-lib=static=karac_runtime");
    println!("cargo:rerun-if-changed={}/libkarac_runtime.a", runtime_dir);
}
