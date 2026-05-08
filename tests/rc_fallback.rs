// tests/rc_fallback.rs
//
// Phase 7 RC fallback: detection (Phase 1), Rc → Arc promotion (Phase 2),
// and #[no_rc] / @no_rc / #[allow(rc_fallback)] enforcement.

use karac::ownership::*;
use karac::{ownershipcheck, parse, resolve, typecheck};

fn run(source: &str) -> OwnershipCheckResult {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {:?}",
        parsed.errors
    );
    let resolved = resolve(&parsed.program);
    assert!(
        resolved.errors.is_empty(),
        "Resolve errors: {:?}",
        resolved.errors
    );
    let typed = typecheck(&parsed.program, &resolved);
    ownershipcheck(&parsed.program, &typed)
}

fn rc_entry<'a>(result: &'a OwnershipCheckResult, fn_name: &str, binding: &str) -> &'a RcEntry {
    result
        .rc_values
        .get(fn_name)
        .unwrap_or_else(|| panic!("no rc map for {fn_name}"))
        .get(binding)
        .unwrap_or_else(|| panic!("binding {binding} not flagged RC in {fn_name}"))
}

// ── Trigger 1: branch-divergent re-use after consume ────────────

#[test]
fn trigger1_if_branch_consume_then_outer_use() {
    // Consume on the then-branch, then outer use → RC fallback (not error).
    let src = "struct Data { value: i64 }\n\
               fn consume(d: Data) { }\n\
               fn use_d(d: Data) { }\n\
               fn process(cond: bool, d: Data) {\n\
                   if cond { consume(d); }\n\
                   use_d(d);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "process", "d");
    assert_eq!(entry.trigger, RcTrigger::DirectReuseAfterConsume);
    assert!(!result.notes.is_empty(), "expected at least one perf note");
    assert_eq!(
        result.representations.get("process.d").map(String::as_str),
        Some("shared (Rc)")
    );
}

#[test]
fn trigger1_match_arm_consume_then_outer_use() {
    let src = "struct Data { value: i64 }\n\
               fn consume(d: Data) { }\n\
               fn use_d(d: Data) { }\n\
               fn process(d: Data) {\n\
                   match d.value {\n\
                       0 => consume(d),\n\
                       _ => {},\n\
                   }\n\
                   use_d(d);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "process", "d");
    assert_eq!(entry.trigger, RcTrigger::DirectReuseAfterConsume);
}

// ── Trigger 2: closure capture + outer use ─────────────────────

#[test]
fn trigger2_closure_capture_then_outer_use() {
    // The closure body consumes cfg; outer log(cfg) afterward → RC trigger 2.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "make_handler", "cfg");
    assert_eq!(entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
}

// ── Trigger 3: container store + subsequent use ────────────────

#[test]
fn trigger3_container_insert_then_outer_use() {
    // Sequential `bag.insert("k", w); audit(w);` — without trigger 3 this
    // would be a use-after-move error (Direct consume into the owned arg of
    // `insert` followed by a sequential read). Trigger 3 retags the consume
    // because the receiver is `mut ref self` (a container that outlives the
    // call) and reroutes the subsequent use into RC fallback.
    let src = "struct Widget { value: i64 }\n\
               struct Bag { count: i64 }\n\
               impl Bag {\n\
                   fn insert(mut ref self, key: i64, value: Widget) { }\n\
               }\n\
               fn audit(w: Widget) { }\n\
               fn register(w: Widget, bag: mut ref Bag) {\n\
                   bag.insert(0, w);\n\
                   audit(w);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors (trigger 3 should reroute UAM to RC), got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "register", "w");
    assert_eq!(entry.trigger, RcTrigger::ContainerStoreWithSubsequentUse);
    assert!(!result.notes.is_empty(), "expected at least one perf note");
    let note = result
        .notes
        .iter()
        .find(|n| n.message.contains("'w'"))
        .expect("expected a note for 'w'");
    assert!(
        note.message.contains("container store with subsequent use"),
        "note should name the trigger 3 label: {}",
        note.message
    );
}

#[test]
fn trigger3_clean_container_store_no_rc() {
    // A container store with NO subsequent use is just a clean move — no RC.
    // The trigger 3 retag still happens (Moved/ContainerStore), but no second
    // use ever fires `handle_moved_use`, so no rc_values entry is recorded.
    let src = "struct Widget { value: i64 }\n\
               struct Bag { count: i64 }\n\
               impl Bag {\n\
                   fn insert(mut ref self, key: i64, value: Widget) { }\n\
               }\n\
               fn register(w: Widget, bag: mut ref Bag) {\n\
                   bag.insert(0, w);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    assert!(
        result
            .rc_values
            .get("register")
            .is_none_or(|m| m.is_empty()),
        "clean container store should NOT produce an RC entry"
    );
}

#[test]
fn trigger3_does_not_fire_for_owned_self_method() {
    // `widget.consume(other)` where `consume` takes `self` (consuming receiver):
    // the receiver is consumed too, so `widget` is no longer a "container" left
    // alive after the call. `other` consumed sequentially + used after should
    // remain a use-after-move error, not trigger 3.
    let src = "struct Widget { value: i64 }\n\
               impl Widget {\n\
                   fn merge(self, other: Widget) { }\n\
               }\n\
               fn audit(w: Widget) { }\n\
               fn register(a: Widget, b: Widget) {\n\
                   a.merge(b);\n\
                   audit(b);\n\
               }";
    let result = run(src);
    assert!(
        !result.errors.is_empty(),
        "owned-self method consuming an owned arg should still error on subsequent use"
    );
    assert_eq!(result.errors[0].kind, OwnershipErrorKind::UseAfterMove);
    assert!(
        result
            .rc_values
            .get("register")
            .is_none_or(|m| !m.contains_key("b")),
        "no trigger-3 RC entry expected for owned-self container move"
    );
}

#[test]
fn trigger3_does_not_fire_for_ref_self_method() {
    // `ref self` methods can't take ownership of an owned arg in a way that
    // outlives the call as a "container store" (the receiver is just a borrow
    // for reading). Sequential consume+use should still error.
    let src = "struct Widget { value: i64 }\n\
               struct Reader { count: i64 }\n\
               impl Reader {\n\
                   fn observe(ref self, value: Widget) { }\n\
               }\n\
               fn audit(w: Widget) { }\n\
               fn run_it(w: Widget, r: ref Reader) {\n\
                   r.observe(w);\n\
                   audit(w);\n\
               }";
    let result = run(src);
    assert!(
        !result.errors.is_empty(),
        "ref-self method consuming arg + subsequent use should error"
    );
    assert_eq!(result.errors[0].kind, OwnershipErrorKind::UseAfterMove);
}

#[test]
fn trigger3_partial_move_through_field_projection() {
    // Container store of `obj.field` is a partial move of `obj` per the
    // consume-predicate's projection rule. A subsequent use of `obj` should
    // fire trigger 3 (container store with subsequent use) the same way a
    // bare-identifier consume would.
    let src = "struct Inner { n: i64 }\n\
               struct Outer { inner: Inner }\n\
               struct Bag { count: i64 }\n\
               impl Bag {\n\
                   fn insert(mut ref self, key: i64, value: Inner) { }\n\
               }\n\
               fn audit(o: Outer) { }\n\
               fn register(o: Outer, bag: mut ref Bag) {\n\
                   bag.insert(0, o.inner);\n\
                   audit(o);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors (trigger 3 reroutes partial-move + subsequent use), got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "register", "o");
    assert_eq!(entry.trigger, RcTrigger::ContainerStoreWithSubsequentUse);
}

#[test]
fn trigger3_local_binding_not_just_param() {
    // Trigger 3 should fire for any owned binding, not just function params.
    // A locally-constructed Widget moved into a container then used after
    // gets the same RC fallback treatment.
    let src = "struct Widget { value: i64 }\n\
               struct Bag { count: i64 }\n\
               impl Bag {\n\
                   fn insert(mut ref self, key: i64, value: Widget) { }\n\
               }\n\
               fn audit(w: Widget) { }\n\
               fn main() {\n\
                   let w = Widget { value: 42 };\n\
                   let mut bag = Bag { count: 0 };\n\
                   bag.insert(0, w);\n\
                   audit(w);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "main", "w");
    assert_eq!(entry.trigger, RcTrigger::ContainerStoreWithSubsequentUse);
}

// ── Sequential consume + use is still an error ─────────────────

#[test]
fn sequential_consume_then_use_still_errors() {
    let src = "struct Data { value: i64 }\n\
               fn consume(d: Data) { }\n\
               fn main() {\n\
                   let d = Data { value: 1 };\n\
                   consume(d);\n\
                   consume(d);\n\
               }";
    let result = run(src);
    assert!(
        !result.errors.is_empty(),
        "expected use-after-move error for sequential consume"
    );
    assert_eq!(result.errors[0].kind, OwnershipErrorKind::UseAfterMove);
    assert!(
        result.rc_values.get("main").is_none_or(|m| m.is_empty()),
        "sequential consume should NOT produce an RC entry"
    );
}

// ── @no_rc on struct ───────────────────────────────────────────

#[test]
fn at_no_rc_struct_blocks_trigger() {
    let src = "@no_rc\n\
               struct Particle { x: i64 }\n\
               fn consume(p: Particle) { }\n\
               fn use_p(p: Particle) { }\n\
               fn process(cond: bool, p: Particle) {\n\
                   if cond { consume(p); }\n\
                   use_p(p);\n\
               }";
    let result = run(src);
    assert!(
        !result.errors.is_empty(),
        "@no_rc Particle should reject RC fallback"
    );
    assert!(result
        .errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::NoRcViolation
            && e.message.contains("Particle")
            && e.message.contains("@no_rc")));
}

// ── #[no_rc] on function ───────────────────────────────────────

#[test]
fn hash_no_rc_function_blocks_trigger() {
    let src = "struct Data { value: i64 }\n\
               fn consume(d: Data) { }\n\
               fn use_d(d: Data) { }\n\
               #[no_rc]\n\
               fn process(cond: bool, d: Data) {\n\
                   if cond { consume(d); }\n\
                   use_d(d);\n\
               }";
    let result = run(src);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::NoRcViolation
                && e.message.contains("#[no_rc]")
                && e.message.contains("process")),
        "expected #[no_rc] violation, got: {:?}",
        result.errors
    );
}

// ── #[allow(rc_fallback)] suppresses notes ─────────────────────

#[test]
fn allow_rc_fallback_suppresses_notes() {
    let src = "struct Data { value: i64 }\n\
               fn consume(d: Data) { }\n\
               fn use_d(d: Data) { }\n\
               #[allow(rc_fallback)]\n\
               fn process(cond: bool, d: Data) {\n\
                   if cond { consume(d); }\n\
                   use_d(d);\n\
               }";
    let result = run(src);
    assert!(result.errors.is_empty());
    // RC entry still recorded (so query/budget work) but no perf note.
    rc_entry(&result, "process", "d");
    assert!(
        result.notes.is_empty(),
        "expected no notes under #[allow(rc_fallback)], got: {:?}",
        result.notes
    );
}

// ── Phase 2: Rc → Arc promotion via par {} overlap ─────────────

#[test]
fn par_block_promotes_rc_to_arc() {
    // d gets RC via trigger 1, AND its other use lands inside par{} → Arc.
    let src = "struct Data { value: i64 }\n\
               fn consume(d: Data) { }\n\
               fn use_d(d: Data) { }\n\
               fn process(cond: bool, d: Data) {\n\
                   if cond { consume(d); }\n\
                   par { use_d(d); }\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "process", "d");
    let arc = result
        .arc_values
        .get("process")
        .expect("expected an Arc set for process");
    assert!(arc.contains("d"), "expected 'd' to be Arc-promoted");
    assert_eq!(
        result.representations.get("process.d").map(String::as_str),
        Some("shared (Arc)")
    );
    let note = result
        .notes
        .iter()
        .find(|n| n.message.contains("'d'"))
        .expect("expected a note for 'd'");
    assert!(
        note.message.contains("shared (Arc)"),
        "note should name Arc flavor: {}",
        note.message
    );
    assert!(
        note.message.contains("crosses a parallel region"),
        "note should explain promotion: {}",
        note.message
    );
}

#[test]
fn no_par_block_keeps_rc() {
    let src = "struct Data { value: i64 }\n\
               fn consume(d: Data) { }\n\
               fn use_d(d: Data) { }\n\
               fn process(cond: bool, d: Data) {\n\
                   if cond { consume(d); }\n\
                   use_d(d);\n\
               }";
    let result = run(src);
    rc_entry(&result, "process", "d");
    assert!(
        result
            .arc_values
            .get("process")
            .is_none_or(|s| !s.contains("d")),
        "expected 'd' to stay Rc (no par block in body)"
    );
    assert_eq!(
        result.representations.get("process.d").map(String::as_str),
        Some("shared (Rc)")
    );
    let note = result
        .notes
        .iter()
        .find(|n| n.message.contains("'d'"))
        .expect("expected a note for 'd'");
    assert!(
        note.message.contains("shared (Rc)"),
        "note should name Rc flavor: {}",
        note.message
    );
    assert!(
        note.message.contains("does not cross a parallel region"),
        "note should explain no promotion: {}",
        note.message
    );
}

// ── Step 4 sentinels: closure creation site as a use in outer RC dataflow ──
//
// Step 4 of the closure-ownership track: the outer function's RC dataflow
// records each captured value as a use at the closure creation site. The
// structural piece shipped in round 12.11 (CFG sibling-sink-block split) and
// the in-place integration closed in round 12.21 (predicate is authoritative
// for both RC and UAM). These tests pin the resulting behavior — closure
// creation that consumes a non-Copy capture pairs with a subsequent outer use
// of the same binding via the formal RC predicate; read-only / Copy / single-
// consume cases produce no RcEntry.

#[test]
fn trigger2_read_only_capture_keeps_consume_site_flavor() {
    // Read-only closure capture (body calls a ref-self method) + outer consume.
    // The closure body's read sits in a sibling sink block (round 12.11), and
    // the formal RC predicate fires on (C = outer log consume, U = closure body
    // read) because they are dominance-incomparable. Step 4 says "closure
    // creation = a use of each capture"; this test pins the orthogonal Step 3
    // claim — flavor attribution follows the *Consume* site's origin (Direct
    // here, since the consume sits in main flow), NOT the U site's location.
    // Trigger 2 (ClosureCaptureWithOuterUse) requires the consume itself to be
    // inside the closure body, where the classifier's consume_origin_ctx tags
    // it ClosureCapture (round 12.14).
    let src = "struct Config { name: i64 }\n\
               impl Config {\n\
                   fn id(ref self) -> i64 { self.name }\n\
               }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let h = || cfg.id();\n\
                   log(cfg);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "make_handler", "cfg");
    assert_eq!(entry.trigger, RcTrigger::DirectReuseAfterConsume);
}

#[test]
fn trigger2_copy_capture_no_rc() {
    // Capture is i64 (Copy). Copy types skip the RC fallback path entirely —
    // every "consume" of a Copy value is silently a copy at the binding level.
    let src = "fn use_n(n: i64) { }\n\
               fn make_handler(n: i64) {\n\
                   let h = || use_n(n);\n\
                   use_n(n);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    assert!(
        result
            .rc_values
            .get("make_handler")
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "expected no RC entries, got {:?}",
        result.rc_values.get("make_handler"),
    );
}

#[test]
fn trigger2_capture_consume_no_outer_use_no_rc() {
    // Closure body consumes cfg; no outer use of cfg afterward. The single
    // Consume site has no partner U for the predicate (C ≠ U requirement),
    // so no RcEntry is recorded — clean move into the closure's captured set.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    assert!(
        result
            .rc_values
            .get("make_handler")
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "expected no RC entries, got {:?}",
        result.rc_values.get("make_handler"),
    );
}

#[test]
fn trigger2_multiple_captures_each_register() {
    // One closure captures both cfg and cred by consume; outer reuses both.
    // Each binding produces an independent RcEntry — Step 4's "each captured
    // value as a use" property applies per-name, not per-closure.
    let src = "struct Config { name: i64 }\n\
               struct Cred { token: i64 }\n\
               fn apply_cfg(c: Config) { }\n\
               fn apply_cred(c: Cred) { }\n\
               fn log_cfg(c: Config) { }\n\
               fn log_cred(c: Cred) { }\n\
               fn make_handler(cfg: Config, cred: Cred) {\n\
                   let h = || { apply_cfg(cfg); apply_cred(cred); };\n\
                   log_cfg(cfg);\n\
                   log_cred(cred);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let cfg_entry = rc_entry(&result, "make_handler", "cfg");
    let cred_entry = rc_entry(&result, "make_handler", "cred");
    assert_eq!(cfg_entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
    assert_eq!(cred_entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
}

#[test]
fn trigger2_closure_in_if_branch_outer_use_after_if() {
    // Closure created inside a conditional branch; outer use sits after the if.
    // The consume origin is preserved as ClosureCapture through the classifier's
    // consume_origin_ctx threading (round 12.14), so the predicate produces a
    // ClosureCaptureWithOuterUse RcEntry — not a DirectReuseAfterConsume one,
    // even though the if-branch + post-if shape would otherwise resemble
    // trigger 1.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cond: bool, cfg: Config) {\n\
                   if cond {\n\
                       let h = || apply(cfg);\n\
                   }\n\
                   log(cfg);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "make_handler", "cfg");
    assert_eq!(entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
}

// ── Step 5 sentinels: closure-value escape as an ordinary RC dataflow use ──
//
// Step 5 of the closure-ownership track: a closure escapes its creation scope
// when its value participates in a use whose live range extends beyond the
// current function or scope (`return closure_expr`, fn-arg pass to an `Fn(...)`
// slot, store into a struct field, channel-send, spawn). Per design.md §
// Closures: parameter modes, capture, and escape (Rule 2 + sub-cases i–iv),
// escape is not a new analysis — it is detected by the RC dataflow walking
// the closure value as an ordinary use.
//
// These sentinels pin the resulting Phase-1 invariant: **the RC predicate's
// verdict is invariant under whether the closure value escapes or stays
// local**. For each escape syntax (return-direct, return-of-let-bound,
// fn-arg-pass, struct-field-store), sub-case (i) clean-escape (captures
// consumed in body, no outer use) produces no RcEntry, and sub-case (ii)
// escape-with-outer-use produces a ClosureCaptureWithOuterUse RcEntry —
// matching the verdict the existing Step-4 sentinels record for closures
// that never explicitly escape. Sub-case (iii) parallel escape and (iv)
// ref-capture borrow error are Step 6 / Step 7's territory respectively.

#[test]
fn step5_escape_via_return_direct_clean() {
    // Sub-case (i): closure consumes capture in body and is the direct
    // return value (no let binding); no outer use of the capture. Single
    // Consume site has no partner U → no RcEntry. The Return arm's
    // Consume of the closure value does not synthesize a use of the
    // captured `cfg` in the outer flow — escape via return is invisible
    // to Phase 1's RC predicate when there is no outer reuse.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn make_handler(cfg: Config) -> Fn() -> () {\n\
                   return || apply(cfg);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    assert!(
        result
            .rc_values
            .get("make_handler")
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "expected no RC entries, got {:?}",
        result.rc_values.get("make_handler"),
    );
}

#[test]
fn step5_escape_via_return_let_bound_with_outer_use() {
    // Sub-case (ii): closure consumes capture, outer use sits between
    // creation and the explicit `return h;` escape. Escape does not add
    // a new use of `cfg`; the (body-consume, outer-log-consume) pair the
    // predicate already pins via the round-12.11 sibling-sink-block split
    // is what fires. The return of `h` after the outer use is structurally
    // a Consume of the closure binding — not the captured `cfg` directly —
    // and has no effect on the RC verdict.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) -> Fn() -> () {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   return h;\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "make_handler", "cfg");
    assert_eq!(entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
}

#[test]
fn step5_escape_via_fn_arg_clean() {
    // Sub-case (i): closure passed as an `Fn() -> ()` parameter — the
    // call consumes the closure value, but the captured `cfg` is not
    // used elsewhere in the outer flow. Single Consume of `cfg` (in the
    // closure body's `apply` call) has no partner U → no RcEntry.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn run_fn(f: Fn() -> ()) { f() }\n\
               fn use_cfg(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   run_fn(h);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    assert!(
        result
            .rc_values
            .get("use_cfg")
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "expected no RC entries, got {:?}",
        result.rc_values.get("use_cfg"),
    );
}

#[test]
fn step5_escape_via_fn_arg_with_outer_use() {
    // Sub-case (ii): closure passed as an `Fn() -> ()` parameter, with
    // an outer log(cfg) before the call. The pair (body-consume of cfg,
    // outer-log-consume of cfg) fires the predicate; the subsequent
    // `run_fn(h)` consumes the closure binding `h` but does not introduce
    // a new use of `cfg` in the outer-flow CFG.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn run_fn(f: Fn() -> ()) { f() }\n\
               fn use_cfg(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   run_fn(h);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "use_cfg", "cfg");
    assert_eq!(entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
}

#[test]
fn step5_escape_via_struct_field_clean() {
    // Sub-case (i): closure stored in a struct-literal field whose
    // declared type is `Fn() -> ()`. The struct literal consumes the
    // closure binding; no outer use of `cfg` exists. Single Consume of
    // `cfg` → no RcEntry. Long-lived-field escape is invisible to
    // Phase 1 absent an outer-use partner.
    let src = "struct Config { name: i64 }\n\
               struct Holder { f: Fn() -> () }\n\
               fn apply(c: Config) { }\n\
               fn make(cfg: Config) -> Holder {\n\
                   let h = || apply(cfg);\n\
                   Holder { f: h }\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    assert!(
        result
            .rc_values
            .get("make")
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "expected no RC entries, got {:?}",
        result.rc_values.get("make"),
    );
}

#[test]
fn step5_escape_via_struct_field_with_outer_use() {
    // Sub-case (ii): closure stored in a struct-literal `Fn() -> ()`
    // field, with outer log(cfg) before the literal. Same verdict as
    // every other (ii) escape route — ClosureCaptureWithOuterUse on `cfg`.
    // The struct-field destination is a Consume site for the closure
    // binding `h`, not for the captured `cfg`, and so the existing
    // (body-consume, outer-log) pair drives the predicate exactly as
    // it does in the never-escape variant.
    let src = "struct Config { name: i64 }\n\
               struct Holder { f: Fn() -> () }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make(cfg: Config) -> Holder {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   Holder { f: h }\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "make", "cfg");
    assert_eq!(entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
}

#[test]
fn step5_escape_verdict_invariant_across_escape_destinations() {
    // The Step-5 invariant in one shot: a fixed closure body with a
    // fixed outer-use shape produces the same RC verdict regardless of
    // where the closure value ends up. Four escape destinations — drop
    // at scope end, return, fn-arg, struct-field — all produce a single
    // ClosureCaptureWithOuterUse RcEntry on `cfg`. If a future change
    // routes one escape kind through a different code path, this guard
    // catches the divergence.
    let scope_end = "struct Config { name: i64 }\n\
                     fn apply(c: Config) { }\n\
                     fn log(c: Config) { }\n\
                     fn f(cfg: Config) {\n\
                         let h = || apply(cfg);\n\
                         log(cfg);\n\
                     }";
    let returned = "struct Config { name: i64 }\n\
                    fn apply(c: Config) { }\n\
                    fn log(c: Config) { }\n\
                    fn f(cfg: Config) -> Fn() -> () {\n\
                        let h = || apply(cfg);\n\
                        log(cfg);\n\
                        return h;\n\
                    }";
    let fn_arg = "struct Config { name: i64 }\n\
                  fn apply(c: Config) { }\n\
                  fn log(c: Config) { }\n\
                  fn run_fn(f: Fn() -> ()) { f() }\n\
                  fn f(cfg: Config) {\n\
                      let h = || apply(cfg);\n\
                      log(cfg);\n\
                      run_fn(h);\n\
                  }";
    let struct_field = "struct Config { name: i64 }\n\
                        struct Holder { f: Fn() -> () }\n\
                        fn apply(c: Config) { }\n\
                        fn log(c: Config) { }\n\
                        fn f(cfg: Config) -> Holder {\n\
                            let h = || apply(cfg);\n\
                            log(cfg);\n\
                            Holder { f: h }\n\
                        }";
    for (label, src) in [
        ("scope_end", scope_end),
        ("returned", returned),
        ("fn_arg", fn_arg),
        ("struct_field", struct_field),
    ] {
        let result = run(src);
        assert!(
            result.errors.is_empty(),
            "{}: expected no errors, got {:?}",
            label,
            result.errors
        );
        let entry = rc_entry(&result, "f", "cfg");
        assert_eq!(
            entry.trigger,
            RcTrigger::ClosureCaptureWithOuterUse,
            "{}: expected ClosureCaptureWithOuterUse, got {:?}",
            label,
            entry.trigger
        );
    }
}

// ── Step 6 sentinels: Rc → Arc promotion through closure-binding par uses ──
//
// Step 6 of the closure-ownership track: when the closure value escapes
// into a parallel region (the v1-realisable form is `par {}` —
// channel-send and `spawn` are deferred surface per `provider_escape.rs`
// and `roadmap.md`), the Rc → Arc promotion pass lifts each Rc-marked
// captured value to Arc. Per design.md § Closures Rule 2 sub-case (iii),
// "the live range of the closure value becomes the live range of each
// captured value for the escape sub-case", and Phase 2's existing par-
// region overlap walker is the live-range intersection check.
//
// Round 12.34 wires the propagation: a per-function `closure_bindings`
// map from binding-name to capture-names is built lazily during the
// par-walker's traversal of `let pat = closure_expr;` forms; the walker's
// Identifier arm — when triggered inside a par block — promotes both
// the binding itself (if RC-marked) and every RC-marked capture of any
// closure bound to that name. These sentinels pin the resulting per-
// shape behavior.

#[test]
fn step6_closure_invoked_inside_par_promotes_capture_to_arc() {
    // Closure created outside par captures cfg (RC-marked from trigger 2),
    // then invoked inside par. Round 12.34 lifts cfg to Arc via the
    // closure-binding lookup in the Identifier arm. Without the fix, the
    // walker would only see `h` as the par-region identifier and miss
    // the captures entirely (h is not in `candidates`).
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   par { h(); }\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "make_handler", "cfg");
    assert_eq!(entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg"),
        "expected 'cfg' to be Arc-promoted via closure h(); got arc={:?}",
        arc,
    );
    assert_eq!(
        result
            .representations
            .get("make_handler.cfg")
            .map(String::as_str),
        Some("shared (Arc)")
    );
}

#[test]
fn step6_closure_passed_as_fn_arg_inside_par_promotes_capture() {
    // Same shape as the invocation case, but the par-region use of the
    // closure binding is as an argument to a function call rather than a
    // direct invocation. Both paths reach the Identifier arm of the
    // walker because `Call.args` and `Call.callee` both recurse into
    // sub-expressions; the par-region promotion fires on either reaching.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn run_fn(f: Fn() -> ()) { f() }\n\
               fn make_handler(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   par { run_fn(h); }\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg"),
        "expected 'cfg' to be Arc-promoted via run_fn(h) inside par; got arc={:?}",
        arc,
    );
}

#[test]
fn step6_multi_capture_closure_inside_par_promotes_all_rc_captures() {
    // One closure captures two non-Copy bindings (cfg, cred), both
    // outer-reused → both RC. The closure binding is invoked inside par.
    // The captures-via-binding propagation must promote BOTH captures,
    // not just one. Pins the per-name iteration in the Identifier arm.
    let src = "struct Config { name: i64 }\n\
               struct Cred { token: i64 }\n\
               fn apply_cfg(c: Config) { }\n\
               fn apply_cred(c: Cred) { }\n\
               fn log_cfg(c: Config) { }\n\
               fn log_cred(c: Cred) { }\n\
               fn make_handler(cfg: Config, cred: Cred) {\n\
                   let h = || { apply_cfg(cfg); apply_cred(cred); };\n\
                   log_cfg(cfg);\n\
                   log_cred(cred);\n\
                   par { h(); }\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    rc_entry(&result, "make_handler", "cred");
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg") && arc.contains("cred"),
        "expected both 'cfg' and 'cred' to be Arc-promoted; got arc={:?}",
        arc,
    );
}

#[test]
fn step6_closure_invoked_outside_par_keeps_capture_rc() {
    // Negative: closure invoked outside par. cfg stays Rc — no parallel
    // region overlap. Pins that the closure-binding propagation only
    // fires inside par, not unconditionally.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   h();\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    assert!(
        result
            .arc_values
            .get("make_handler")
            .is_none_or(|s| !s.contains("cfg")),
        "expected 'cfg' to stay Rc when closure is invoked outside par"
    );
    assert_eq!(
        result
            .representations
            .get("make_handler.cfg")
            .map(String::as_str),
        Some("shared (Rc)")
    );
}

#[test]
fn step6_closure_created_inside_par_still_promotes_capture() {
    // Regression guard: a closure created INSIDE a par block (with the
    // capture defined outside par) was already promoted before round
    // 12.34 because the existing walker descends into closure bodies
    // with the parent's `inside_par` flag (ownership.rs Closure arm).
    // After 12.34 the closure-binding propagation should not interfere
    // with this pre-existing case — h is registered into closure_bindings
    // INSIDE the par walk, and `h()` reaches the Identifier arm with
    // inside_par=true, so cfg promotes via either route. The two routes
    // converge on the same verdict.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   par {\n\
                       let h = || apply(cfg);\n\
                       h();\n\
                   }\n\
                   log(cfg);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg"),
        "expected 'cfg' to be Arc-promoted (closure created inside par); got arc={:?}",
        arc,
    );
}

// ── Phase 2 boundary: Sender.send(closure) ──────────────────────
//
// Theme 2 of wip-list2 (2026-05-08): teach the par-walker to flip
// `inside_parallel_region` when traversing into a `Sender.send(...)`
// argument expression. Captures of any RC-marked closure passed
// through the channel get promoted to Arc by the same machinery that
// handles `par { h(); }`. The `spawn(closure)` boundary stays
// deferred to Phase 6.3 / v1.1 user syntax.

#[test]
fn phase2_send_closure_promotes_capture_to_arc() {
    // Positive base case: closure h captures cfg (RC-marked from
    // trigger 2 — capture + outer use), then the closure binding is
    // sent via `tx.send(h)` against a Channel-destructured Sender.
    // Theme 2 lifts cfg to Arc via the channel-send boundary.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let (tx, rx) = Channel.new();\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   tx.send(h);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    let entry = rc_entry(&result, "make_handler", "cfg");
    assert_eq!(entry.trigger, RcTrigger::ClosureCaptureWithOuterUse);
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg"),
        "expected 'cfg' to be Arc-promoted via tx.send(h); got arc={:?}",
        arc,
    );
}

#[test]
fn phase2_clone_send_closure_promotes_capture_to_arc() {
    // Cloned-sender path: `tx.clone().send(h)`. The receiver of
    // `.send(...)` is `tx.clone()`, which the resolver unwraps one
    // level back to `tx` — itself a Sender — so the boundary still
    // fires. Mirrors the round-8 channel-send escape detection's
    // existing handling of cloned senders.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let (tx, rx) = Channel.new();\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   tx.clone().send(h);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg"),
        "expected 'cfg' to be Arc-promoted through tx.clone().send(h); got arc={:?}",
        arc,
    );
}

#[test]
fn phase2_send_param_typed_sender_promotes_capture() {
    // Param-form: function takes a `tx: Sender[Fn() -> ()]` parameter
    // — the param-seed in `promote_for_function` registers tx into
    // `let_types` at function entry, so `tx.send(h)` inside the body
    // resolves through the parameter annotation rather than a local
    // Channel.new() destructure. Verifies sub-step (d) of the slice
    // plan (function-parameter Sender[T] annotations).
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(tx: Sender[Fn() -> ()], cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   tx.send(h);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg"),
        "expected 'cfg' to be Arc-promoted via tx: Sender[T] parameter; got arc={:?}",
        arc,
    );
}

#[test]
fn phase2_no_par_no_send_keeps_rc() {
    // Negative control: closure h captures cfg (RC-marked) but is
    // only invoked locally — no par, no Sender.send. cfg stays at Rc.
    // Pins that the boundary fires only at the channel-send site, not
    // for arbitrary closure invocation.
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   h();\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    assert!(
        !result.arc_values.contains_key("make_handler"),
        "expected no Arc promotion (no par, no send); got arc={:?}",
        result.arc_values.get("make_handler")
    );
}

#[test]
fn phase2_send_and_par_invocation_consistent_decision() {
    // Closure h2 invoked inside `par { h2(); }` AND a separate
    // closure h1 sent via `tx.send(h1)` — both capture cfg. The
    // monotonic property: any single parallel-region sighting is
    // sufficient to promote cfg to Arc; multiple sightings produce
    // one consistent decision (cfg in arc_values, exactly once).
    let src = "struct Config { name: i64 }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let (tx, rx) = Channel.new();\n\
                   let h1 = || apply(cfg);\n\
                   let h2 = || apply(cfg);\n\
                   log(cfg);\n\
                   tx.send(h1);\n\
                   par { h2(); }\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    let arc = result
        .arc_values
        .get("make_handler")
        .expect("expected an Arc set for make_handler");
    assert!(
        arc.contains("cfg"),
        "expected 'cfg' to be Arc-promoted (consistent decision across two boundaries); got arc={:?}",
        arc,
    );
}

#[test]
fn phase2_send_non_sender_method_does_not_promote() {
    // Negative gate: a method named `send` on a non-Sender type must
    // NOT trigger the channel-send boundary. Verifies the
    // receiver-type gate in `resolve_receiver_is_sender` is
    // load-bearing — otherwise any user-defined `send` method would
    // over-promote captures spuriously. Soundness over precision:
    // false-positive promotion is unsound from the user's perspective
    // (turning Rc → Arc unnecessarily is a perf hit) but it's
    // technically correct; the test pins that the gate distinguishes.
    let src = "struct Config { name: i64 }\n\
               struct Bag { x: i64 }\n\
               impl Bag { fn send(ref self, c: Fn() -> ()) { } }\n\
               fn apply(c: Config) { }\n\
               fn log(c: Config) { }\n\
               fn make_handler(cfg: Config) {\n\
                   let bag = Bag { x: 1 };\n\
                   let h = || apply(cfg);\n\
                   log(cfg);\n\
                   bag.send(h);\n\
               }";
    let result = run(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors, got {:?}",
        result.errors
    );
    rc_entry(&result, "make_handler", "cfg");
    assert!(
        !result.arc_values.contains_key("make_handler"),
        "expected no Arc promotion for non-Sender .send(); got arc={:?}",
        result.arc_values.get("make_handler")
    );
}
