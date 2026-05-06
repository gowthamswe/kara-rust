// Config B — monomorphic Karac algorithm.
// Open-addressing hash map mirroring runtime/src/map.rs's behavior:
//   * INITIAL_CAPACITY = 16
//   * Linear probing
//   * Load factor 3/4 triggers resize (doubling)
//   * FNV-1a hash over the 8 bytes of i64 (matches Karac's emit_hash_fn_for_type)
//   * Same EMPTY/OCCUPIED/TOMBSTONE state machine
// BUT, unlike Karac runtime: hash and eq are inlined direct calls; keys/values are
// typed (i64, i64) slots, not raw byte blobs. No fn pointers, no byte memcpy.
//
// This isolates the "fn pointer + byte blob" tax from "Karac probe sequence /
// load factor / hash quality differs from std::HashMap" tax (= B-vs-A).

const INITIAL_CAPACITY: usize = 16;
const BUCKET_EMPTY: u8 = 0;
const BUCKET_OCCUPIED: u8 = 1;
const BUCKET_TOMBSTONE: u8 = 2;

const FNV_BASIS: u64 = 14695981039346656037;
const FNV_PRIME: u64 = 1099511628211;

#[inline(always)]
fn hash_i64(key: i64) -> u64 {
    let bytes = key.to_ne_bytes();
    let mut h: u64 = FNV_BASIS;
    for b in bytes.iter() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

struct KaracMonoMap {
    status: Vec<u8>,
    kv: Vec<(i64, i64)>,
    capacity: usize,
    len: usize,
    tombstones: usize,
}

impl KaracMonoMap {
    fn new() -> Self {
        Self {
            status: vec![BUCKET_EMPTY; INITIAL_CAPACITY],
            kv: vec![(0, 0); INITIAL_CAPACITY],
            capacity: INITIAL_CAPACITY,
            len: 0,
            tombstones: 0,
        }
    }

    #[inline]
    fn lookup(&self, key: i64) -> Option<usize> {
        let hash = hash_i64(key);
        let start = (hash as usize) & (self.capacity - 1);
        for i in 0..self.capacity {
            let slot = (start + i) & (self.capacity - 1);
            match self.status[slot] {
                BUCKET_EMPTY => return None,
                BUCKET_OCCUPIED if self.kv[slot].0 == key => return Some(slot),
                _ => {} // TOMBSTONE or non-matching OCCUPIED — keep probing
            }
        }
        None
    }

    #[inline]
    fn find_insert_slot(&self, key: i64) -> (usize, bool) {
        let hash = hash_i64(key);
        let start = (hash as usize) & (self.capacity - 1);
        let mut first_tombstone: Option<usize> = None;
        for i in 0..self.capacity {
            let slot = (start + i) & (self.capacity - 1);
            match self.status[slot] {
                BUCKET_EMPTY => {
                    let target = first_tombstone.unwrap_or(slot);
                    return (target, false);
                }
                BUCKET_TOMBSTONE => {
                    if first_tombstone.is_none() {
                        first_tombstone = Some(slot);
                    }
                }
                BUCKET_OCCUPIED => {
                    if self.kv[slot].0 == key {
                        return (slot, true);
                    }
                }
                _ => unreachable!(),
            }
        }
        (first_tombstone.unwrap_or(0), false)
    }

    fn insert(&mut self, key: i64, val: i64) {
        if (self.len + self.tombstones + 1) * 4 > self.capacity * 3 {
            self.resize();
        }
        let (slot, exists) = self.find_insert_slot(key);
        let was_tombstone = self.status[slot] == BUCKET_TOMBSTONE;
        if !exists {
            self.kv[slot].0 = key;
            self.len += 1;
            if was_tombstone {
                self.tombstones -= 1;
            }
        }
        self.kv[slot].1 = val;
        self.status[slot] = BUCKET_OCCUPIED;
    }

    #[inline]
    fn get(&self, key: i64) -> Option<i64> {
        self.lookup(key).map(|slot| self.kv[slot].1)
    }

    fn resize(&mut self) {
        let new_cap = self.capacity * 2;
        let old_status = std::mem::replace(&mut self.status, vec![BUCKET_EMPTY; new_cap]);
        let old_kv = std::mem::replace(&mut self.kv, vec![(0, 0); new_cap]);
        let old_cap = self.capacity;

        self.capacity = new_cap;
        self.len = 0;
        self.tombstones = 0;

        for i in 0..old_cap {
            if old_status[i] == BUCKET_OCCUPIED {
                let (k, v) = old_kv[i];
                self.insert(k, v);
            }
        }
    }
}

const N: i64 = 1_000_000;
const M: i64 = 10;

fn two_sum_workload(seed: i64) -> i64 {
    let mut seen = KaracMonoMap::new();
    let target: i64 = -1;
    let mut hits: i64 = 0;
    for i in 0..N {
        let num = ((i.wrapping_mul(7).wrapping_add(seed)) % (2 * N)) - N;
        let complement = target - num;
        if let Some(j) = seen.get(complement) {
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
