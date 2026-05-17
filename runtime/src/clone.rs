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
    // O(1)-per-call single-char UTF-8 decoder. Prior versions
    // delegated to `std::str::from_utf8(slice)` over the *whole*
    // remaining slice, then `chars().next()`. That made each call
    // O(remaining_bytes) — for a 104K-char `for c in s.chars()`
    // pass, total validation work grew quadratically (~5.4B bytes
    // re-validated). Investigation (`wip-chars-inline.md`, 2026-05-15)
    // measured this as the dominant per-char cost in karac vs Rust
    // (karac 776 ns/char vs Rust 0.96 ns/char → 810× slower on
    // pure-iter bench). The fix is mechanical: look at one to four
    // bytes for the next character; never touch the rest of the
    // slice.
    //
    // Output parity with the prior implementation on well-formed
    // input is exact (same Unicode scalar value, same byte
    // advancement). For malformed input the new version emits
    // U+FFFD and advances 1 byte at the malformed position; the
    // prior version tried a small valid-prefix recovery before
    // emitting FFFD. Both shapes are forward-progressing and match
    // `String::from_utf8_lossy`'s recovery family; the simpler
    // single-byte advance is the standard "WHATWG UTF-8 decoder"
    // recovery rule.
    if byte_offset < 0 || byte_offset >= len {
        *out_codepoint = 0;
        return len;
    }
    let start = byte_offset as usize;
    let total = len as usize;
    let remaining = total - start;
    let b0 = *data.add(start);

    // ── ASCII fast path (the hot path for English / source code) ─
    if b0 < 0x80 {
        *out_codepoint = b0 as u32;
        return (start + 1) as i64;
    }

    // ── Determine continuation width from lead byte ──────────────
    let width: usize = if b0 < 0xC2 {
        // 0x80..0xC0: stray continuation byte at start (malformed).
        // 0xC0..0xC2: 2-byte overlong of a 1-byte ASCII codepoint —
        // disallowed by the UTF-8 spec since RFC 3629. Reject both.
        *out_codepoint = 0xFFFD;
        return (start + 1) as i64;
    } else if b0 < 0xE0 {
        2
    } else if b0 < 0xF0 {
        3
    } else if b0 < 0xF5 {
        // 0xF5..0xF8 would technically be 4-byte leads but they
        // start above the U+10FFFF Unicode cap — disallowed.
        4
    } else {
        *out_codepoint = 0xFFFD;
        return (start + 1) as i64;
    };

    if remaining < width {
        // Truncated sequence at end of string.
        *out_codepoint = 0xFFFD;
        return (start + 1) as i64;
    }

    // ── Combine continuation bytes, validating each ──────────────
    let mut cp: u32 = match width {
        2 => (b0 & 0x1F) as u32,
        3 => (b0 & 0x0F) as u32,
        4 => (b0 & 0x07) as u32,
        _ => unreachable!(),
    };
    for i in 1..width {
        let b = *data.add(start + i);
        if b & 0xC0 != 0x80 {
            // Expected a `10xxxxxx` continuation byte; got something
            // else. Bail out with FFFD; advance 1 byte (don't
            // consume the malformed lead+partial run).
            *out_codepoint = 0xFFFD;
            return (start + 1) as i64;
        }
        cp = (cp << 6) | ((b & 0x3F) as u32);
    }

    // ── Reject surrogates, overlongs, out-of-range codepoints ────
    let valid = match width {
        2 => cp >= 0x80,
        3 => cp >= 0x800 && !(0xD800..=0xDFFF).contains(&cp),
        4 => (0x10000..=0x10FFFF).contains(&cp),
        _ => unreachable!(),
    };
    if !valid {
        *out_codepoint = 0xFFFD;
        return (start + 1) as i64;
    }

    *out_codepoint = cp;
    (start + width) as i64
}

/// Encode a Unicode scalar value as 1–4 UTF-8 bytes written through `out`.
/// Returns the number of bytes written. Peer to `karac_string_decode_char`
/// — used by codegen's `compile_print` / f-string `char`-arm to render a
/// codepoint as the glyph rather than the integer codepoint. Codepoints
/// outside the Unicode scalar range (≥ 0x110000) and the surrogate range
/// (0xD800..=0xDFFF) are normalized to U+FFFD (`EF BF BD`, 3 bytes).
///
/// # Safety
///
/// * `out` must point to a writable buffer of at least 4 bytes. The
///   compiler emits a 4-byte stack alloca per call site (see
///   `emit_codepoint_to_utf8`), so this precondition is satisfied at
///   every generated call site.
#[no_mangle]
pub unsafe extern "C" fn karac_string_encode_char(cp: u32, out: *mut u8) -> i64 {
    if cp < 0x80 {
        *out = cp as u8;
        1
    } else if cp < 0x800 {
        *out = 0xC0 | ((cp >> 6) as u8);
        *out.add(1) = 0x80 | ((cp & 0x3F) as u8);
        2
    } else if cp < 0x10000 {
        // Surrogates (0xD800..=0xDFFF) aren't valid scalar values; emit
        // U+FFFD instead of round-tripping the surrogate as 3 bytes (which
        // would produce malformed UTF-8 the next reader would reject).
        // Well-formed Kāra `char` values can't hold a surrogate — the
        // decoder normalizes them on the way in — but a downstream
        // arithmetic op could land here on a synthetic codepoint.
        if (0xD800..=0xDFFF).contains(&cp) {
            *out = 0xEF;
            *out.add(1) = 0xBF;
            *out.add(2) = 0xBD;
            return 3;
        }
        *out = 0xE0 | ((cp >> 12) as u8);
        *out.add(1) = 0x80 | (((cp >> 6) & 0x3F) as u8);
        *out.add(2) = 0x80 | ((cp & 0x3F) as u8);
        3
    } else if cp < 0x110000 {
        *out = 0xF0 | ((cp >> 18) as u8);
        *out.add(1) = 0x80 | (((cp >> 12) & 0x3F) as u8);
        *out.add(2) = 0x80 | (((cp >> 6) & 0x3F) as u8);
        *out.add(3) = 0x80 | ((cp & 0x3F) as u8);
        4
    } else {
        // Out-of-range codepoint → U+FFFD (3 bytes).
        *out = 0xEF;
        *out.add(1) = 0xBF;
        *out.add(2) = 0xBD;
        3
    }
}
