//! Open-addressing hash map for compiled Kāra programs.
//!
//! The map is type-erased: keys and values are raw byte blobs. The compiler
//! passes `key_size`/`val_size` at construction time and emits concrete
//! `hash_fn`/`eq_fn` function pointers for the monomorphised K type.
//!
//! Layout: two parallel heap allocations —
//!   `status[capacity]`       — u8 per bucket: EMPTY | OCCUPIED | TOMBSTONE
//!   `kv[capacity*(ks+vs)]`   — packed (key, val) pairs, no alignment padding
//!
//! Collision resolution: linear probing. Load factor ceiling 3/4; resize
//! doubles capacity and rehashes. Deletion marks tombstones; tombstones count
//! toward the load factor so a tombstone-heavy table still triggers a resize.

use std::alloc::{alloc, dealloc, Layout};
use std::ffi::c_void;
use std::ptr;

extern "C" {
    /// libc `free`, used to release per-entry `Vec[T]` data buffers in
    /// `karac_map_free_with_val_drop_vec`. The codegen-side Vec.push path
    /// allocates the same buffers via libc `malloc` (see
    /// `Codegen::malloc_fn`), so pairing with libc `free` is the matching
    /// alloc/free pair. We avoid `std::alloc::dealloc` here because that
    /// path requires reconstructing the original `Layout`, which the
    /// codegen-emitted Vec.push doesn't record per-buffer.
    fn free(ptr: *mut c_void);
}

const INITIAL_CAPACITY: usize = 16;
const BUCKET_EMPTY: u8 = 0;
const BUCKET_OCCUPIED: u8 = 1;
const BUCKET_TOMBSTONE: u8 = 2;

/// `#[repr(C)]` is load-bearing — codegen-side monomorphized
/// `Map[K, V]` symbols (`src/codegen.rs`, see
/// [`wip-monomorphized-collections.md`](../../docs/implementation_checklist/wip-monomorphized-collections.md))
/// load the `len` / `capacity` / `status` / `kv` fields by direct
/// GEP + load against this layout. Reordering or inserting fields
/// here is an ABI break against codegen; the offsets are pinned by
/// the `karac_map_field_offsets_match_codegen` unit test below.
/// Slice 5's atomic delete of the erased runtime will untether
/// codegen from this layout.
#[repr(C)]
struct KaracMap {
    status: *mut u8,
    kv: *mut u8,
    capacity: usize,
    len: usize,
    tombstones: usize,
    key_size: usize,
    val_size: usize,
    hash_fn: unsafe extern "C" fn(*const c_void) -> u64,
    eq_fn: unsafe extern "C" fn(*const c_void, *const c_void) -> bool,
}

// Maps are local to a single thread; the compiler never moves them across
// thread boundaries without going through Arc/Mutex at a higher level.
unsafe impl Send for KaracMap {}

impl KaracMap {
    unsafe fn new(
        key_size: usize,
        val_size: usize,
        hash_fn: unsafe extern "C" fn(*const c_void) -> u64,
        eq_fn: unsafe extern "C" fn(*const c_void, *const c_void) -> bool,
    ) -> *mut Self {
        let (status, kv) = Self::alloc_storage(INITIAL_CAPACITY, key_size, val_size);
        let map = Box::new(KaracMap {
            status,
            kv,
            capacity: INITIAL_CAPACITY,
            len: 0,
            tombstones: 0,
            key_size,
            val_size,
            hash_fn,
            eq_fn,
        });
        Box::into_raw(map)
    }

    unsafe fn alloc_storage(
        capacity: usize,
        key_size: usize,
        val_size: usize,
    ) -> (*mut u8, *mut u8) {
        let status_layout = Layout::array::<u8>(capacity).unwrap();
        let status = alloc(status_layout);
        ptr::write_bytes(status, BUCKET_EMPTY, capacity);

        let kv_size = (key_size + val_size).max(1);
        let kv_layout = Layout::array::<u8>(capacity * kv_size).unwrap();
        let kv = alloc(kv_layout);

        (status, kv)
    }

    unsafe fn free_storage(&mut self) {
        let status_layout = Layout::array::<u8>(self.capacity).unwrap();
        dealloc(self.status, status_layout);

        let kv_size = (self.key_size + self.val_size).max(1);
        let kv_layout = Layout::array::<u8>(self.capacity * kv_size).unwrap();
        dealloc(self.kv, kv_layout);
    }

    #[inline]
    unsafe fn key_ptr(&self, slot: usize) -> *const c_void {
        self.kv.add(slot * (self.key_size + self.val_size)) as *const c_void
    }

    #[inline]
    unsafe fn val_ptr(&self, slot: usize) -> *const c_void {
        self.kv
            .add(slot * (self.key_size + self.val_size) + self.key_size) as *const c_void
    }

    // Find an occupied slot holding `key`. Returns Some(slot) or None.
    unsafe fn lookup(&self, key: *const c_void) -> Option<usize> {
        let hash = (self.hash_fn)(key);
        let start = (hash as usize) & (self.capacity - 1);
        for i in 0..self.capacity {
            let slot = (start + i) & (self.capacity - 1);
            match *self.status.add(slot) {
                BUCKET_EMPTY => return None,
                BUCKET_OCCUPIED if (self.eq_fn)(self.key_ptr(slot), key) => {
                    return Some(slot);
                }
                _ => {} // TOMBSTONE or non-matching OCCUPIED — keep probing
            }
        }
        None
    }

    // Find the slot to write a new key into. Also returns whether the key
    // already exists (update vs. fresh insert).
    unsafe fn find_insert_slot(&self, key: *const c_void) -> (usize, bool) {
        let hash = (self.hash_fn)(key);
        let start = (hash as usize) & (self.capacity - 1);
        let mut first_tombstone: Option<usize> = None;
        for i in 0..self.capacity {
            let slot = (start + i) & (self.capacity - 1);
            match *self.status.add(slot) {
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
                    if (self.eq_fn)(self.key_ptr(slot), key) {
                        return (slot, true);
                    }
                }
                _ => unreachable!(),
            }
        }
        // Should not reach here if resize policy is respected.
        (first_tombstone.unwrap_or(0), false)
    }

    unsafe fn insert(&mut self, key: *const c_void, val: *const c_void) {
        // Resize when (occupied + tombstones) / capacity > 3/4.
        if (self.len + self.tombstones + 1) * 4 > self.capacity * 3 {
            self.resize();
        }
        let (slot, exists) = self.find_insert_slot(key);
        let was_tombstone = *self.status.add(slot) == BUCKET_TOMBSTONE;
        let kv_offset = slot * (self.key_size + self.val_size);
        if !exists {
            ptr::copy_nonoverlapping(key as *const u8, self.kv.add(kv_offset), self.key_size);
            self.len += 1;
            if was_tombstone {
                self.tombstones -= 1;
            }
        }
        ptr::copy_nonoverlapping(
            val as *const u8,
            self.kv.add(kv_offset + self.key_size),
            self.val_size,
        );
        *self.status.add(slot) = BUCKET_OCCUPIED;
    }

    unsafe fn get(&self, key: *const c_void, out_val: *mut c_void) -> bool {
        if let Some(slot) = self.lookup(key) {
            ptr::copy_nonoverlapping(
                self.val_ptr(slot) as *const u8,
                out_val as *mut u8,
                self.val_size,
            );
            true
        } else {
            false
        }
    }

    unsafe fn remove(&mut self, key: *const c_void) -> bool {
        if let Some(slot) = self.lookup(key) {
            *self.status.add(slot) = BUCKET_TOMBSTONE;
            self.len -= 1;
            self.tombstones += 1;
            true
        } else {
            false
        }
    }

    unsafe fn resize(&mut self) {
        let new_cap = self.capacity * 2;
        let (new_status, new_kv) = Self::alloc_storage(new_cap, self.key_size, self.val_size);

        let old_status = self.status;
        let old_kv = self.kv;
        let old_cap = self.capacity;

        self.status = new_status;
        self.kv = new_kv;
        self.capacity = new_cap;
        self.len = 0;
        self.tombstones = 0;

        for i in 0..old_cap {
            if *old_status.add(i) == BUCKET_OCCUPIED {
                let kv_size = self.key_size + self.val_size;
                let key = old_kv.add(i * kv_size) as *const c_void;
                let val = old_kv.add(i * kv_size + self.key_size) as *const c_void;
                self.insert(key, val);
            }
        }

        let status_layout = Layout::array::<u8>(old_cap).unwrap();
        dealloc(old_status, status_layout);
        let kv_layout =
            Layout::array::<u8>(old_cap * (self.key_size + self.val_size).max(1)).unwrap();
        dealloc(old_kv, kv_layout);
    }
}

struct KaracMapIter {
    map: *const KaracMap,
    index: usize,
}

// ── Public C ABI ─────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn karac_map_new(
    key_size: usize,
    val_size: usize,
    hash_fn: unsafe extern "C" fn(*const c_void) -> u64,
    eq_fn: unsafe extern "C" fn(*const c_void, *const c_void) -> bool,
) -> *mut c_void {
    KaracMap::new(key_size, val_size, hash_fn, eq_fn) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn karac_map_free(map: *mut c_void) {
    if map.is_null() {
        return;
    }
    let mut m = Box::from_raw(map as *mut KaracMap);
    m.free_storage();
    // Box drop frees the KaracMap allocation itself.
}

/// `karac_map_free` variant that recursively drops per-entry Vec / String
/// content before deallocating the bucket storage. Selected when *either*
/// the key type or the value type follows the `{ptr, i64, i64}` runtime
/// layout (`Vec[T]`, `String`, `Set[Vec[T]]`, `Set[String]`,
/// `Map[String, V]`, `Map[K, Vec[T]]`, `Map[String, Vec[T]]`, etc.).
///
/// `drop_key != 0` → for each live entry, free the key's `data` pointer
/// when its `cap > 0`. `drop_val != 0` → same for the value. Both flags
/// may be set simultaneously (e.g. `Map[String, Vec[T]]`). When both
/// are zero the caller should route through plain `karac_map_free`
/// instead — this function still works in that case but loops with no
/// side-effect.
///
/// **Layout contract.** When `drop_key != 0`, key_size MUST be 24 and
/// the key value at each live slot is exactly the karac runtime
/// Vec/String struct (offset 0: 8-byte data pointer; offset 8: 8-byte
/// len; offset 16: 8-byte cap). Same for `drop_val != 0` and val_size.
/// The codegen-side `FreeMapHandle` cleanup arm guards both flags via
/// `llvm_ty_is_vec_struct` against the registered key / value LLVM
/// types.
///
/// **Set[T] handling.** Set lowers to `Map[T, ()]` with `val_size = 0`.
/// For `Set[Vec[T]]` / `Set[String]`, codegen passes `drop_key = 1,
/// drop_val = 0`. The val-side loop is gated by `drop_val != 0` so it
/// never reads the (non-existent) value blob.
///
/// Closes the 2026-05-13 / 2026-05-14 leak class where heap-owning keys
/// or non-Vec heap-owning values in Maps / Sets were never released.
/// Replaces the narrower `karac_map_free_with_val_drop_vec` (val-only)
/// helper.
#[no_mangle]
pub unsafe extern "C" fn karac_map_free_with_drop_vec(
    map: *mut c_void,
    drop_key: i32,
    drop_val: i32,
) {
    if map.is_null() {
        return;
    }
    let mut m = Box::from_raw(map as *mut KaracMap);
    if drop_key != 0 || drop_val != 0 {
        let entry_stride = m.key_size + m.val_size;
        for slot in 0..m.capacity {
            if *m.status.add(slot) != BUCKET_OCCUPIED {
                continue;
            }
            if drop_key != 0 {
                // Key lives at kv + slot*stride.
                let key_base = m.kv.add(slot * entry_stride);
                let data_ptr = ptr::read_unaligned(key_base as *const *mut u8);
                let cap = ptr::read_unaligned(key_base.add(16) as *const i64);
                if cap > 0 && !data_ptr.is_null() {
                    free(data_ptr as *mut c_void);
                }
            }
            if drop_val != 0 {
                // Value lives at kv + slot*stride + key_size.
                let val_base = m.kv.add(slot * entry_stride + m.key_size);
                let data_ptr = ptr::read_unaligned(val_base as *const *mut u8);
                let cap = ptr::read_unaligned(val_base.add(16) as *const i64);
                if cap > 0 && !data_ptr.is_null() {
                    free(data_ptr as *mut c_void);
                }
            }
        }
    }
    m.free_storage();
}

#[no_mangle]
pub unsafe extern "C" fn karac_map_insert(
    map: *mut c_void,
    key: *const c_void,
    val: *const c_void,
) {
    (*(map as *mut KaracMap)).insert(key, val);
}

/// Inserts `key → val`. If `key` already existed, copies the **old** value into
/// `out_old_val` and returns `true`. If it was a fresh insertion, returns `false`
/// and leaves `out_old_val` untouched. Matches `Map.insert → Option[V]` semantics.
#[no_mangle]
pub unsafe extern "C" fn karac_map_insert_old(
    map: *mut c_void,
    key: *const c_void,
    val: *const c_void,
    out_old_val: *mut c_void,
) -> bool {
    let m = &mut *(map as *mut KaracMap);
    // Resize before probing so find_insert_slot always finds a slot.
    if (m.len + m.tombstones + 1) * 4 > m.capacity * 3 {
        m.resize();
    }
    let (slot, exists) = m.find_insert_slot(key);
    let was_tombstone = *m.status.add(slot) == BUCKET_TOMBSTONE;
    let kv_offset = slot * (m.key_size + m.val_size);
    if exists {
        // Copy old value out before overwriting.
        ptr::copy_nonoverlapping(
            m.kv.add(kv_offset + m.key_size),
            out_old_val as *mut u8,
            m.val_size,
        );
    } else {
        ptr::copy_nonoverlapping(key as *const u8, m.kv.add(kv_offset), m.key_size);
        m.len += 1;
        if was_tombstone {
            m.tombstones -= 1;
        }
    }
    ptr::copy_nonoverlapping(
        val as *const u8,
        m.kv.add(kv_offset + m.key_size),
        m.val_size,
    );
    *m.status.add(slot) = BUCKET_OCCUPIED;
    exists
}

/// Returns `true` and copies the value into `out_val` if the key exists.
/// Returns `false` and leaves `out_val` untouched otherwise.
#[no_mangle]
pub unsafe extern "C" fn karac_map_get(
    map: *const c_void,
    key: *const c_void,
    out_val: *mut c_void,
) -> bool {
    (*(map as *const KaracMap)).get(key, out_val)
}

/// Returns `true` if the key was present and has been removed.
#[no_mangle]
pub unsafe extern "C" fn karac_map_remove(map: *mut c_void, key: *const c_void) -> bool {
    (*(map as *mut KaracMap)).remove(key)
}

/// Removes `key`. If it existed, copies the **old** value into `out_old_val` and
/// returns `true`. Returns `false` and leaves `out_old_val` untouched otherwise.
/// Matches `Map.remove → Option[V]` semantics.
#[no_mangle]
pub unsafe extern "C" fn karac_map_remove_old(
    map: *mut c_void,
    key: *const c_void,
    out_old_val: *mut c_void,
) -> bool {
    let m = &mut *(map as *mut KaracMap);
    if let Some(slot) = m.lookup(key) {
        ptr::copy_nonoverlapping(
            m.val_ptr(slot) as *const u8,
            out_old_val as *mut u8,
            m.val_size,
        );
        *m.status.add(slot) = BUCKET_TOMBSTONE;
        m.len -= 1;
        m.tombstones += 1;
        true
    } else {
        false
    }
}

#[no_mangle]
pub unsafe extern "C" fn karac_map_contains(map: *const c_void, key: *const c_void) -> bool {
    (*(map as *const KaracMap)).lookup(key).is_some()
}

/// Probe-and-insert-on-vacant. Used by `Map.entry(k)` chains whose
/// terminal step is `or_insert` / `or_insert_with` — the codegen knows it
/// will write a default through the returned slot pointer when the key was
/// missing, so the runtime claims the bucket up front.
///
/// On Vacant: writes the key bytes, marks the bucket OCCUPIED, and leaves
/// the value half uninitialised. Returns `false` so the caller overwrites.
/// On Occupied: leaves the bucket alone, returns `true`.
///
/// Resizes before probing so the slot index — and therefore the slot
/// pointer — is stable for the rest of the call. The returned pointer is
/// valid until the next mutating call on the same map (matches the Rust
/// `HashMap::entry` lifetime contract).
#[no_mangle]
pub unsafe extern "C" fn karac_map_entry(
    map: *mut c_void,
    key: *const c_void,
    out_slot_ptr: *mut *mut c_void,
) -> bool {
    let m = &mut *(map as *mut KaracMap);
    if (m.len + m.tombstones + 1) * 4 > m.capacity * 3 {
        m.resize();
    }
    let (slot, exists) = m.find_insert_slot(key);
    if !exists {
        let was_tombstone = *m.status.add(slot) == BUCKET_TOMBSTONE;
        let kv_offset = slot * (m.key_size + m.val_size);
        ptr::copy_nonoverlapping(key as *const u8, m.kv.add(kv_offset), m.key_size);
        *m.status.add(slot) = BUCKET_OCCUPIED;
        m.len += 1;
        if was_tombstone {
            m.tombstones -= 1;
        }
    }
    *out_slot_ptr = m.val_ptr(slot) as *mut c_void;
    exists
}

/// Read-only lookup variant used to lower `Map.entry(k)` chains whose
/// terminal step is `and_modify` — the codegen runs the closure only when
/// the key is present, and never inserts. Distinct C ABI from
/// `karac_map_entry` so the runtime can keep the pure / mutating contracts
/// separate.
///
/// On Occupied: writes the value-half pointer to `out_slot_ptr`, returns
/// `true`. On Vacant: leaves `out_slot_ptr` untouched, returns `false`.
/// Pointer lifetime matches `karac_map_entry`'s contract.
#[no_mangle]
pub unsafe extern "C" fn karac_map_lookup_slot(
    map: *mut c_void,
    key: *const c_void,
    out_slot_ptr: *mut *mut c_void,
) -> bool {
    let m = &*(map as *const KaracMap);
    if let Some(slot) = m.lookup(key) {
        *out_slot_ptr = m.val_ptr(slot) as *mut c_void;
        true
    } else {
        false
    }
}

#[no_mangle]
pub unsafe extern "C" fn karac_map_len(map: *const c_void) -> u64 {
    (*(map as *const KaracMap)).len as u64
}

/// Removes every entry from `map`. Resets `len` and `tombstones` to 0 and
/// zeroes the status array so every bucket reads as `BUCKET_EMPTY`. The bucket
/// capacity is preserved — matches the Rust `HashMap::clear` contract. The
/// `kv` byte buffer is left untouched (its contents become unreachable but
/// remain allocated for reuse on subsequent inserts).
#[no_mangle]
pub unsafe extern "C" fn karac_map_clear(map: *mut c_void) {
    let m = &mut *(map as *mut KaracMap);
    ptr::write_bytes(m.status, BUCKET_EMPTY, m.capacity);
    m.len = 0;
    m.tombstones = 0;
}

#[no_mangle]
pub unsafe extern "C" fn karac_map_iter_new(map: *const c_void) -> *mut c_void {
    let iter = Box::new(KaracMapIter {
        map: map as *const KaracMap,
        index: 0,
    });
    Box::into_raw(iter) as *mut c_void
}

/// Advances the iterator. Copies the next key into `out_key` and value into
/// `out_val`. Returns `true` if a pair was written, `false` when exhausted.
#[no_mangle]
pub unsafe extern "C" fn karac_map_iter_next(
    iter: *mut c_void,
    out_key: *mut c_void,
    out_val: *mut c_void,
) -> bool {
    let it = &mut *(iter as *mut KaracMapIter);
    let m = &*it.map;
    while it.index < m.capacity {
        let i = it.index;
        it.index += 1;
        if *m.status.add(i) == BUCKET_OCCUPIED {
            let kv_size = m.key_size + m.val_size;
            ptr::copy_nonoverlapping(m.kv.add(i * kv_size), out_key as *mut u8, m.key_size);
            ptr::copy_nonoverlapping(
                m.kv.add(i * kv_size + m.key_size),
                out_val as *mut u8,
                m.val_size,
            );
            return true;
        }
    }
    false
}

#[no_mangle]
pub unsafe extern "C" fn karac_map_iter_free(iter: *mut c_void) {
    if !iter.is_null() {
        drop(Box::from_raw(iter as *mut KaracMapIter));
    }
}

#[cfg(test)]
mod tests {
    use super::KaracMap;
    use std::mem::offset_of;

    /// Codegen-side monomorphized `Map[K, V]` symbols load
    /// `KaracMap.len` / `KaracMap.capacity` / `KaracMap.status` /
    /// `KaracMap.kv` by direct GEP + load against this struct's
    /// `#[repr(C)]` layout. The offsets are hardcoded in
    /// `src/codegen.rs` (see `KARAC_MAP_LEN_OFFSET` etc.). Any
    /// reorder / insert / type-change of `KaracMap` fields breaks
    /// the ABI; this test catches the drift before runtime/binary
    /// diverge.
    #[test]
    fn karac_map_field_offsets_match_codegen() {
        assert_eq!(offset_of!(KaracMap, status), 0);
        assert_eq!(offset_of!(KaracMap, kv), 8);
        assert_eq!(offset_of!(KaracMap, capacity), 16);
        assert_eq!(offset_of!(KaracMap, len), 24);
        assert_eq!(offset_of!(KaracMap, tombstones), 32);
    }
}
