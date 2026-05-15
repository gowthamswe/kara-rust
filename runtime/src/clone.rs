//! Per-type clone runtime helpers used by `emit_clone_fn_for_type_expr`.
//!
//! The codegen-emitted `karac_clone_<typename>` functions (one per type
//! mangled name, cached in the codegen `clone_fn_cache`) inline most of
//! their work: primitives are a load+store, Vec/Map/Set/Tuple recurse
//! through per-element clones synthesised in LLVM IR. The cases that
//! genuinely need a runtime helper are:
//!
//! * `String` — the codegen would otherwise have to duplicate the
//!   alloc-then-memcpy dance every emit site, including the static-
//!   literal `cap == 0` special case. One helper is cleaner.
//!
//! Future helpers (cycle-safe Rc clone, finalizer-aware refcounted clone)
//! land here too.

use std::alloc::{alloc, Layout};
use std::ffi::c_void;
use std::ptr;

/// Layout of a Kāra `String` value: `{ ptr data, i64 len, i64 cap }`.
/// Matches the codegen-side `string_struct_type` (Vec[u8] re-used for
/// String). Layout-equivalent on every supported target.
#[repr(C)]
struct KaracString {
    data: *mut u8,
    len: i64,
    cap: i64,
}

/// Deep-copy a Kāra `String`. Reads `*src` (`{data, len, cap}`), allocates
/// a fresh buffer holding `len` bytes, copies the source contents, and
/// writes `{new_data, len, new_cap}` to `*dst`.
///
/// Static-literal handling: when the source `cap == 0` (the convention for
/// strings whose buffer lives in the program's read-only string pool and
/// therefore must never be freed), the clone allocates a `len`-byte buffer
/// with `new_cap = len` so the cloned String's scope-exit cleanup correctly
/// frees it; the source's `cap = 0` keeps the static buffer untouched. For
/// already-heap-owned source strings (`cap > 0`), the clone's capacity
/// matches the source so a follow-up `push_str` in the cloned String has
/// the same headroom characteristic as a fresh copy.
///
/// Empty strings (`len == 0`) skip the allocation: the new String gets
/// `data = null`, `cap = 0`. The interpreter and codegen scope-exit free
/// paths already handle null-data Strings as no-ops.
///
/// # Safety
///
/// * `src` must point to a readable, fully-initialised `KaracString`.
/// * `dst` must point to a writable `KaracString`-sized region.
/// * The caller is responsible for the resulting String's lifetime —
///   typically registered with the codegen scope-cleanup machinery via
///   the same `track_vec_var` path Strings already use.
#[no_mangle]
pub unsafe extern "C" fn karac_string_clone(src: *const c_void, dst: *mut c_void) {
    let src = &*(src as *const KaracString);
    let dst = &mut *(dst as *mut KaracString);

    if src.len == 0 {
        dst.data = ptr::null_mut();
        dst.len = 0;
        dst.cap = 0;
        return;
    }

    let new_cap = src.len.max(1) as usize;
    let layout = Layout::array::<u8>(new_cap).unwrap();
    let new_data = alloc(layout);
    ptr::copy_nonoverlapping(src.data, new_data, src.len as usize);

    dst.data = new_data;
    dst.len = src.len;
    dst.cap = src.len; // capacity matches len — fresh buffer, no headroom.
}

/// Decode the next UTF-8 character starting at `byte_offset` in the byte
/// slice `(data, len)`. Writes the Unicode scalar value (codepoint) through
/// `out_codepoint` and returns the byte offset after the decoded character.
///
/// Used by codegen for `for c in s` / `for c in s.chars()` on a Kāra
/// `String`. The interpreter side uses Rust's `str::chars` directly; this
/// extern is the codegen-side equivalent so compiled-mode and tree-walk
/// produce identical per-char sequences (same Unicode scalar values).
///
/// Malformed UTF-8 produces the standard replacement character `U+FFFD`
/// for the offending byte and advances by one byte — matches Rust's
/// `String::from_utf8_lossy` recovery semantics. v1 expects well-formed
/// UTF-8 from sources upstream of codegen; the recovery path exists to
/// keep the loop forward-progressing on garbage rather than infinite-
/// looping.
///
/// # Safety
///
/// * `data` must point to a readable buffer of at least `len` bytes when
///   `byte_offset < len`; the helper performs no out-of-bounds read past
///   `len`. Callers (the codegen-emitted for-loop) gate this call on
///   `byte_offset < len`.
/// * `out_codepoint` must point to a writable `u32`.
#[no_mangle]
pub unsafe extern "C" fn karac_string_decode_char(
    data: *const u8,
    len: i64,
    byte_offset: i64,
    out_codepoint: *mut u32,
) -> i64 {
    if byte_offset < 0 || byte_offset >= len {
        *out_codepoint = 0;
        return len;
    }
    let start = byte_offset as usize;
    let total = len as usize;
    let slice = std::slice::from_raw_parts(data.add(start), total - start);
    match std::str::from_utf8(slice) {
        Ok(s) => {
            // Well-formed: take the first char, advance by its UTF-8 width.
            if let Some(c) = s.chars().next() {
                *out_codepoint = c as u32;
                (start + c.len_utf8()) as i64
            } else {
                *out_codepoint = 0;
                len
            }
        }
        Err(e) => {
            // Decode up to the first invalid byte; if that's offset 0 we
            // emit the replacement char and advance by one byte to keep
            // the loop forward-progressing.
            let valid = e.valid_up_to();
            if valid > 0 {
                let valid_slice = std::str::from_utf8_unchecked(&slice[..valid]);
                if let Some(c) = valid_slice.chars().next() {
                    *out_codepoint = c as u32;
                    return (start + c.len_utf8()) as i64;
                }
            }
            *out_codepoint = 0xFFFD; // U+FFFD REPLACEMENT CHARACTER
            (start + 1) as i64
        }
    }
}
