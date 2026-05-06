// Config A — std::collections::HashMap baseline.
// Two-sum-style workload: for each i in 0..N, get(complement); if missing, insert(num, i).
// Repeats M iterations with shifted keys to defeat any iteration-caching.
//
// Prints `hits=<n>` for cross-config verification — A/B/C must agree.

use std::collections::HashMap;

const N: i64 = 1_000_000;
const M: i64 = 10;

fn two_sum_workload(seed: i64) -> i64 {
    let mut seen: HashMap<i64, i64> = HashMap::new();
    let target: i64 = -1;
    let mut hits: i64 = 0;
    for i in 0..N {
        let num = ((i.wrapping_mul(7).wrapping_add(seed)) % (2 * N)) - N;
        let complement = target - num;
        if let Some(&j) = seen.get(&complement) {
            hits = hits.wrapping_add(i + j);
        }
        seen.insert(num, i);
    }
    hits
}

fn main() {
    let mut total: i64 = 0;
    for iter in 0..M {
        total = total.wrapping_add(two_sum_workload(iter * 31));
    }
    println!("hits={}", total);
}
