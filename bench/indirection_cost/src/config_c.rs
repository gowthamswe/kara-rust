// Config C — FFI to Karac's actual runtime (libkarac_runtime.a, runtime/src/map.rs).
// Same workload as A and B. Goes through:
//   * Function-pointer dispatch on hash (`hash_fn` extern "C")
//   * Function-pointer dispatch on eq (`eq_fn` extern "C")
//   * Byte-blob storage with dynamic-size memcpy on insert/get
//   * The full karac_map_new / karac_map_insert / karac_map_get pipeline
//
// Hash and eq are emitted as monomorphic `i64`-aware functions but called
// indirectly. The eq path also goes through *const c_void load instead of
// direct i64 ==. This mirrors what user-compiled Kāra code looks like today.

use std::ffi::c_void;

#[link(name = "karac_runtime", kind = "static")]
extern "C" {
    fn karac_map_new(
        key_size: usize,
        val_size: usize,
        hash_fn: unsafe extern "C" fn(*const c_void) -> u64,
        eq_fn: unsafe extern "C" fn(*const c_void, *const c_void) -> bool,
    ) -> *mut c_void;

    fn karac_map_free(map: *mut c_void);

    fn karac_map_insert(map: *mut c_void, key: *const c_void, val: *const c_void);

    fn karac_map_get(map: *const c_void, key: *const c_void, out_val: *mut c_void) -> bool;
}

// Mirror of Karac's `emit_hash_fn_for_type` for i64 — FNV-1a over 8 bytes.
unsafe extern "C" fn hash_i64_fn(key: *const c_void) -> u64 {
    const FNV_BASIS: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let bytes = std::slice::from_raw_parts(key as *const u8, 8);
    let mut h: u64 = FNV_BASIS;
    for b in bytes.iter() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

unsafe extern "C" fn eq_i64_fn(a: *const c_void, b: *const c_void) -> bool {
    *(a as *const i64) == *(b as *const i64)
}

const N: i64 = 1_000_000;
const M: i64 = 10;

unsafe fn two_sum_workload(seed: i64) -> i64 {
    let map = karac_map_new(8, 8, hash_i64_fn, eq_i64_fn);
    let target: i64 = -1;
    let mut hits: i64 = 0;
    for i in 0..N {
        let num = (i.wrapping_mul(7).wrapping_add(seed)) % (2 * N) - N;
        let complement = target - num;
        let mut out_val: i64 = 0;
        let found = karac_map_get(
            map,
            &complement as *const i64 as *const c_void,
            &mut out_val as *mut i64 as *mut c_void,
        );
        if found {
            hits = hits.wrapping_add(i + out_val);
        }
        karac_map_insert(
            map,
            &num as *const i64 as *const c_void,
            &i as *const i64 as *const c_void,
        );
    }
    karac_map_free(map);
    hits
}

fn main() {
    let mut total: i64 = 0;
    for iter in 0..M {
        unsafe {
            total = total.wrapping_add(two_sum_workload(iter * 31));
        }
    }
    println!("hits={}", total);
}
