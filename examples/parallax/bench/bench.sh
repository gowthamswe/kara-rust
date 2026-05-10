#!/bin/sh
# Slice E (2026-05-09) — Parallax bench driver. Updated 2026-05-10
# (G2 + G3 + G4) to sweep connection counts, run multiple measurement
# rounds per (impl, conn) pair, and report a richer percentile spectrum.
#
# Builds + runs the four reference impls (kara, rust, go, node) and
# probes each with `wrk` for throughput + latency-distribution.
# Sequential per-impl runs on the same machine — F4 fairness control.
#
# Usage:
#   bench.sh                          # full bench, all four impls
#   bench.sh --dry-run                # print what would run; touch nothing
#   bench.sh --impls=k,r              # comma-separated subset
#                                     # (k=kara, r=rust, g=go, n=node)
#   bench.sh --connections=100,1000   # connection-count sweep (default
#                                     # 100,1000,5000). One row per
#                                     # (impl, conn) pair in the output.
#   bench.sh --runs=5                 # measurement rounds per
#                                     # (impl, conn) pair (default 3).
#                                     # Output reports median req/s with
#                                     # min..max range across rounds.
#   bench.sh --warmup=5               # one-time per-server warmup
#                                     # before any measurement (default
#                                     # 3s). Per-(conn,run) fresh
#                                     # warmup is implicit in wrk.
#   bench.sh --measure=15             # measure-window length per
#                                     # round (default 10s).
#
# **Toolchain probing.** Each impl checks for its required toolchain
# (cargo for kara + rust; go for go; node for node; wrk always
# required for measurements). Missing toolchain → `skip: <lang> not
# installed` to stderr; the impl is skipped, the bench continues for
# the others.
#
# **Output format.** wrk's `--latency` produces fixed percentiles
# (p50/p75/p90/p99) plus a `Latency ... Max` line. We parse all five
# (p50, p75, p90, p99, max) plus `Requests/sec:`. Per `(impl, conn)`
# pair, N runs produce N values per metric; we report median req/s
# with [min..max] range, and the median of each percentile across
# the N rounds. Higher percentiles (p99.9, p99.99) require wrk2 or
# a Lua HdrHistogram script — out of scope here; tracked at
# `docs/investigations/bench_robustness.md § G4`.
#
# **No throughput-number assertions in CI.** Per the slice plan, the
# numbers are the artifact, not a regression gate. `tests/parallax_
# bench.rs::test_bench_script_dry_run` invokes us with `--dry-run`
# only.

set -eu

# ── Resolve script directory regardless of caller's CWD ────────────
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BENCH_DIR="$SCRIPT_DIR"
REPO_ROOT=$(CDPATH= cd -- "$BENCH_DIR/../../.." && pwd)

# ── Defaults ───────────────────────────────────────────────────────
DRY_RUN=0
IMPLS_FILTER="k,r,g,n"
# Default warmup = 0 (no warmup). With $RUNS=3 measure rounds and
# median-of-runs aggregation, the first-round JIT/cold-start outlier
# is naturally excluded — warmup adds time without improving the
# reported median. Also avoids a Node-specific failure mode: at
# -c100, a 3 s warmup pins 100 keep-alive connections in TIME_WAIT
# on the macOS loopback for ~30 s after closing, starving the
# subsequent measure rounds of ephemeral ports for fresh
# connections. Users who want explicit warmup characterization can
# pass --warmup=N.
WARMUP_SEC=0
MEASURE_SEC=10
WRK_THREADS=4
CONNECTIONS_LIST="100,1000,5000"
RUNS=3
PORT_TIMEOUT_SEC=30

# ── Parse args ─────────────────────────────────────────────────────
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --impls=*) IMPLS_FILTER="${arg#--impls=}" ;;
    --warmup=*) WARMUP_SEC="${arg#--warmup=}" ;;
    --measure=*) MEASURE_SEC="${arg#--measure=}" ;;
    --connections=*) CONNECTIONS_LIST="${arg#--connections=}" ;;
    --runs=*) RUNS="${arg#--runs=}" ;;
    -h|--help)
      sed -n '3,49p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2
      exit 2
      ;;
  esac
done

want_impl() {
  case "$IMPLS_FILTER" in
    *"$1"*) return 0 ;;
    *) return 1 ;;
  esac
}

# ── Toolchain check helpers ─────────────────────────────────────────
have() {
  command -v "$1" >/dev/null 2>&1
}

# ── Per-impl runners ────────────────────────────────────────────────
# Each runner builds its impl and returns 0 on success, non-zero on
# skip. The actual server launch + wrk loop happens after the runner
# returns — see run_impl.

prepare_kara() {
  if ! have cargo; then
    echo "skip: kara not built (cargo not installed)" >&2
    return 1
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "[dry-run] kara: would build karac + libkarac_runtime + server.kara binary" >&2
    return 0
  fi
  (cd "$REPO_ROOT" && cargo build --release --features llvm -p karac >/dev/null 2>&1) || {
    echo "skip: kara build failed (cargo build --release --features llvm)" >&2
    return 1
  }
  (cd "$REPO_ROOT" && cargo build --release -p karac-runtime >/dev/null 2>&1) || {
    echo "skip: kara runtime build failed" >&2
    return 1
  }
  KARAC_BIN="$REPO_ROOT/target/release/karac"
  KARA_SRC="$BENCH_DIR/kara/server.kara"
  KARA_EXE="$BENCH_DIR/kara/.bin/server"
  mkdir -p "$BENCH_DIR/kara/.bin"
  KARAC_RUNTIME="$REPO_ROOT/target/release/libkarac_runtime.a" \
    "$KARAC_BIN" build "$KARA_SRC" >/dev/null 2>&1 || {
    echo "skip: kara compile failed (karac build server.kara)" >&2
    return 1
  }
  if [ -f "$REPO_ROOT/server" ]; then
    mv "$REPO_ROOT/server" "$KARA_EXE"
  elif [ -f "./server" ]; then
    mv "./server" "$KARA_EXE"
  elif [ -f "$BENCH_DIR/kara/server" ]; then
    mv "$BENCH_DIR/kara/server" "$KARA_EXE"
  fi
  [ -x "$KARA_EXE" ] || {
    echo "skip: kara exe missing at $KARA_EXE" >&2
    return 1
  }
  return 0
}

prepare_rust() {
  if ! have cargo; then
    echo "skip: rust not built (cargo not installed)" >&2
    return 1
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "[dry-run] rust: would cargo build --release in bench/rust/" >&2
    return 0
  fi
  (cd "$BENCH_DIR/rust" && cargo build --release >/dev/null 2>&1) || {
    echo "skip: rust build failed (cargo build --release)" >&2
    return 1
  }
  RUST_EXE="$BENCH_DIR/rust/target/release/parallax-bench-rust"
  [ -x "$RUST_EXE" ] || {
    echo "skip: rust exe missing at $RUST_EXE" >&2
    return 1
  }
  return 0
}

prepare_go() {
  if ! have go; then
    echo "skip: go not installed" >&2
    return 1
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "[dry-run] go: would go build in bench/go/" >&2
    return 0
  fi
  GO_EXE="$BENCH_DIR/go/parallax-bench-go"
  (cd "$BENCH_DIR/go" && go build -o "$GO_EXE" .) >/dev/null 2>&1 || {
    echo "skip: go build failed (go build)" >&2
    return 1
  }
  [ -x "$GO_EXE" ] || {
    echo "skip: go exe missing at $GO_EXE" >&2
    return 1
  }
  return 0
}

prepare_node() {
  if ! have node; then
    echo "skip: node not installed" >&2
    return 1
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "[dry-run] node: would run node $BENCH_DIR/node/server.js" >&2
    return 0
  fi
  [ -f "$BENCH_DIR/node/server.js" ] || {
    echo "skip: node server.js missing" >&2
    return 1
  }
  return 0
}

# ── Server launcher + port discovery ────────────────────────────────
SERVER_PID=""
launch_and_get_port() {
  cmd="$1"
  log="$2"
  rm -f "$log"
  $cmd >"$log" 2>&1 &
  SERVER_PID=$!
  i=0
  while [ "$i" -lt "$PORT_TIMEOUT_SEC" ]; do
    port=$(grep -m1 -E '^BOUND_PORT=' "$log" 2>/dev/null | head -n1 | sed -E 's/^BOUND_PORT=//')
    if [ -n "${port:-}" ]; then
      echo "$port"
      return 0
    fi
    sleep 1
    i=$((i + 1))
  done
  echo "0"
  return 1
}

kill_server() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
  fi
}

# ── wrk runner + percentile parser ──────────────────────────────────
# Runs one wrk measurement round. Echoes a single space-separated row:
#   `<rps> <p50_ms> <p75_ms> <p90_ms> <p99_ms> <max_ms>`
# (latencies normalized to milliseconds — wrk's output uses us/ms/s
# suffixes, we convert to ms in awk.) On parse failure echoes
# `NA NA NA NA NA NA` so the aggregator's `awk` math doesn't choke.
run_wrk_one() {
  port="$1"
  conns="$2"
  url="http://127.0.0.1:$port/dashboard/1"
  out=$(wrk -t"$WRK_THREADS" -c"$conns" -d"${MEASURE_SEC}s" --latency "$url" 2>&1) || true
  echo "$out" | awk '
    function to_ms(v,    n, u) {
      n = v + 0
      u = v
      sub(/^[0-9.]+/, "", u)
      if (u == "us") return n / 1000.0
      if (u == "ms") return n
      if (u == "s")  return n * 1000.0
      if (u == "m")  return n * 60000.0
      return n  # unitless or unrecognized — treat as ms-equivalent
    }
    /^Requests\/sec:/ { rps = $2 + 0 }
    # Match the per-thread `Latency` *stats* row (fields are Avg, Stdev,
    # Max, +/- Stdev), not the `Latency Distribution` header that
    # immediately follows. The trailing `[0-9]` rules out the header
    # case which has `Distribution` in field 2.
    /^[[:space:]]+Latency[[:space:]]+[0-9]/ { lat_max = to_ms($4) }
    /^[[:space:]]+50%[[:space:]]/  { p50 = to_ms($2) }
    /^[[:space:]]+75%[[:space:]]/  { p75 = to_ms($2) }
    /^[[:space:]]+90%[[:space:]]/  { p90 = to_ms($2) }
    /^[[:space:]]+99%[[:space:]]/  { p99 = to_ms($2) }
    END {
      printf "%s %s %s %s %s %s\n",
        (rps    ? rps    : "NA"),
        (p50    ? p50    : "NA"),
        (p75    ? p75    : "NA"),
        (p90    ? p90    : "NA"),
        (p99    ? p99    : "NA"),
        (lat_max ? lat_max : "NA")
    }
  '
}

# ── Multi-run aggregator ────────────────────────────────────────────
# Runs run_wrk_one $RUNS times for a (port, conns) pair; aggregates
# across runs. Echoes a single row:
#   `<conns> <rps_med> <rps_min> <rps_max> <p50_med> <p75_med> <p90_med> <p99_med> <max_med>`
# All latencies in milliseconds.
run_wrk_aggregated() {
  port="$1"
  conns="$2"
  raw=""
  i=0
  while [ "$i" -lt "$RUNS" ]; do
    line=$(run_wrk_one "$port" "$conns") || line="NA NA NA NA NA NA"
    raw="${raw}${line}
"
    i=$((i + 1))
  done
  printf '%s' "$raw" | awk -v conns="$conns" '
    function median(a, n,    i, j, t) {
      for (i = 1; i <= n; i++)
        for (j = i + 1; j <= n; j++)
          if (a[i] > a[j]) { t = a[i]; a[i] = a[j]; a[j] = t }
      if (n % 2 == 1) return a[(n + 1) / 2]
      else return (a[n / 2] + a[n / 2 + 1]) / 2
    }
    function minof(a, n,    i, m) { m = a[1]; for (i = 2; i <= n; i++) if (a[i] < m) m = a[i]; return m }
    function maxof(a, n,    i, m) { m = a[1]; for (i = 2; i <= n; i++) if (a[i] > m) m = a[i]; return m }
    # Track valid runs separately for rps vs full-percentile data —
    # at high connection counts wrk under load can return a
    # `Requests/sec:` line without any Latency Distribution rows
    # (server saturating, lots of socket errors, distribution table
    # suppressed). Including those partial rows in percentile
    # medians yields 0s that dominate the median; we keep them in
    # the rps aggregate (the run still produced a throughput
    # number) but exclude them from percentile aggregates.
    $1 != "NA" {
      rn++; rps[rn] = $1
    }
    $1 != "NA" && $2 != "NA" && $3 != "NA" && $4 != "NA" && $5 != "NA" && $6 != "NA" {
      pn++
      p50[pn] = $2; p75[pn] = $3; p90[pn] = $4; p99[pn] = $5; lmax[pn] = $6
    }
    END {
      if (rn == 0) {
        printf "%s NA NA NA NA NA NA NA NA\n", conns
        exit
      }
      printf "%s %.2f %.2f %.2f", conns, median(rps, rn), minof(rps, rn), maxof(rps, rn)
      if (pn == 0) {
        printf " NA NA NA NA NA\n"
      } else {
        printf " %.2f %.2f %.2f %.2f %.2f\n",
          median(p50, pn), median(p75, pn), median(p90, pn), median(p99, pn),
          median(lmax, pn)
      }
    }
  '
}

# ── Run one impl across all connection counts ───────────────────────
# $1 = impl tag (k|r|g|n), $2 = display name, $3 = prepare fn,
# $4 = run command (path to exe / interp). Builds the impl, launches
# the server once, runs a one-time warmup, then sweeps connection
# counts × runs and emits one result row per (impl, conn) to stdout.
# Rows have the form:
#   `<name>|<conns>|<rps_med>|<rps_min>|<rps_max>|<p50>|<p75>|<p90>|<p99>|<max>`
run_impl() {
  tag="$1"
  name="$2"
  prepare="$3"
  cmd="$4"
  if ! want_impl "$tag"; then
    return 0
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    "$prepare" || true
    # Emit a placeholder row per connection count so --dry-run output
    # exercises the same shape as the real output.
    for c in $(echo "$CONNECTIONS_LIST" | tr ',' ' '); do
      echo "$name|$c|DRY|DRY|DRY|DRY|DRY|DRY|DRY|DRY"
    done
    return 0
  fi
  if ! "$prepare"; then
    for c in $(echo "$CONNECTIONS_LIST" | tr ',' ' '); do
      echo "$name|$c|SKIP|SKIP|SKIP|SKIP|SKIP|SKIP|SKIP|SKIP"
    done
    return 0
  fi
  log=$(mktemp)
  trap 'kill_server; rm -f "$log"' EXIT INT TERM
  port=$(launch_and_get_port "$cmd" "$log") || {
    for c in $(echo "$CONNECTIONS_LIST" | tr ',' ' '); do
      echo "$name|$c|BIND_FAIL|BIND_FAIL|BIND_FAIL|BIND_FAIL|BIND_FAIL|BIND_FAIL|BIND_FAIL|BIND_FAIL" >&2
    done
    kill_server
    rm -f "$log"
    trap - EXIT INT TERM
    return 0
  }
  if ! have wrk; then
    for c in $(echo "$CONNECTIONS_LIST" | tr ',' ' '); do
      echo "$name|$c|WRK_MISSING|WRK_MISSING|WRK_MISSING|WRK_MISSING|WRK_MISSING|WRK_MISSING|WRK_MISSING|WRK_MISSING" >&2
    done
    kill_server
    rm -f "$log"
    trap - EXIT INT TERM
    return 0
  fi
  # Optional one-time per-server warmup at the smallest connection
  # count. Default WARMUP_SEC=0 (skipped); see DRY_RUN-section
  # comment on default rationale.
  if [ "$WARMUP_SEC" -gt 0 ]; then
    first_conn=$(echo "$CONNECTIONS_LIST" | cut -d',' -f1)
    url="http://127.0.0.1:$port/dashboard/1"
    wrk -t"$WRK_THREADS" -c"$first_conn" -d"${WARMUP_SEC}s" "$url" >/dev/null 2>&1 || true
  fi
  for c in $(echo "$CONNECTIONS_LIST" | tr ',' ' '); do
    agg=$(run_wrk_aggregated "$port" "$c") || agg="$c NA NA NA NA NA NA NA NA"
    # Convert space-separated to pipe-separated and prepend impl name.
    echo "$name|$(echo "$agg" | tr ' ' '|')"
  done
  kill_server
  rm -f "$log"
  trap - EXIT INT TERM
}

# ── Main ────────────────────────────────────────────────────────────
echo "Parallax bench harness — kara, rust, go, node"
echo "  bench dir: $BENCH_DIR"
if [ "$DRY_RUN" -eq 1 ]; then
  echo "  mode: DRY RUN (no servers spawned, no wrk)"
fi
if have wrk; then
  echo "  wrk: $(wrk --version 2>&1 | head -1)"
fi
echo "  impls filter: $IMPLS_FILTER"
echo "  connections sweep: $CONNECTIONS_LIST"
echo "  runs per (impl, conn): $RUNS"
echo "  warmup: ${WARMUP_SEC}s (one-time per-server), measure: ${MEASURE_SEC}s × $RUNS rounds"
echo

results=""

KARA_EXE_HOLDER="$BENCH_DIR/kara/.bin/server"
out=$(run_impl "k" "kara" prepare_kara "$KARA_EXE_HOLDER")
results="$results
$out"

RUST_EXE_HOLDER="$BENCH_DIR/rust/target/release/parallax-bench-rust"
out=$(run_impl "r" "rust" prepare_rust "$RUST_EXE_HOLDER")
results="$results
$out"

GO_EXE_HOLDER="$BENCH_DIR/go/parallax-bench-go"
out=$(run_impl "g" "go" prepare_go "$GO_EXE_HOLDER")
results="$results
$out"

NODE_CMD_HOLDER="node $BENCH_DIR/node/server.js"
out=$(run_impl "n" "node" prepare_node "$NODE_CMD_HOLDER")
results="$results
$out"

echo
echo "Results — req/s reported as median across $RUNS rounds, [min..max] in brackets;"
echo "          latencies are median across rounds in milliseconds."
echo
printf "  %-6s | %-6s | %-26s | %-8s | %-8s | %-8s | %-8s | %-9s\n" \
  "impl" "-c" "req/s (med [min..max])" "p50 ms" "p75 ms" "p90 ms" "p99 ms" "max ms"
printf "  %s\n" "------+--------+----------------------------+----------+----------+----------+----------+----------"
printf "%s\n" "$results" | while IFS='|' read -r name conns rps_med rps_min rps_max p50 p75 p90 p99 lmax; do
  [ -z "$name" ] && continue
  if [ "$rps_med" = "DRY" ] || [ "$rps_med" = "SKIP" ] || [ "$rps_med" = "NA" ]; then
    printf "  %-6s | %-6s | %-26s | %-8s | %-8s | %-8s | %-8s | %-9s\n" \
      "$name" "$conns" "$rps_med" "$rps_med" "$rps_med" "$rps_med" "$rps_med" "$rps_med"
  else
    rps_summary=$(printf "%.0f [%.0f..%.0f]" "$rps_med" "$rps_min" "$rps_max")
    printf "  %-6s | %-6s | %-26s | %-8s | %-8s | %-8s | %-8s | %-9s\n" \
      "$name" "$conns" "$rps_summary" "$p50" "$p75" "$p90" "$p99" "$lmax"
  fi
done

echo
if [ "$DRY_RUN" -eq 1 ]; then
  echo "DRY RUN complete — no benchmark numbers produced."
  echo "Re-run without --dry-run to measure."
fi
