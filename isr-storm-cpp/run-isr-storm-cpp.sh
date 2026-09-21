#!/usr/bin/env bash
# ISR retry-storm A/B on the C++ SDK, contrasting the two AppendWriter functions.
#
# Observed table B is written by isr-storm-cpp in the mode under test:
#   arm 1 (before): --mode wait      Append(row, WriteResult&) + background Wait,
#                                     no admission backpressure (unbounded inflight).
#   arm 2 (after):  --mode callback   Append(row, WriteCallback) with native
#                                     WriteCallbackOptions backpressure (262144/30s).
# Stressor table A is the same binary in --mode forget (fire-and-forget) plus a
# DDL recreate kick, which shrinks ISR cluster-wide and hits B with the storm.
#
# Usage:
#   ./run-isr-storm-cpp.sh build          # cmake configure + build
#   ./run-isr-storm-cpp.sh recreate       # drop+recreate A and B with this schema
#   ./run-isr-storm-cpp.sh ab             # run both arms end to end, then tar logs
#   ./run-isr-storm-cpp.sh run wait       # run a single arm
#   ./run-isr-storm-cpp.sh stop-all
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="${BUILD_DIR:-$SCRIPT_DIR/build}"
SDK_SOURCE="${FLUSS_SDK_SOURCE:-$(cd "$SCRIPT_DIR/../fluss-rust" && pwd)}"
BIN="${BIN:-$BUILD_DIR/isr-storm-cpp}"
RUN_DIR="${RUN_DIR:-$SCRIPT_DIR/isr-run-cpp}"

# Best-effort SDK-side logging: warn-level surfaces client retry/reject warnings
# into each arm's log alongside the binary's own ERRLOG breakdown. Override with
# RUST_LOG=info for more detail. Harmless if the SDK emits no tracing output.
export RUST_LOG="${RUST_LOG:-warn}"

# --- cluster / connection ---
# Read the same isr-storm.conf format as the Rust runner (key = value, # for
# comments; keys: bootstrap, sasl.username, sasl.password, sasl.mechanism).
# Invoke identically, e.g. CONFIG=/root/917-cb/.../isr-storm.conf ./run... ab
# Explicit env vars still override the file.
CONFIG="${CONFIG:-$SCRIPT_DIR/isr-storm.conf}"
cfg_get() { # cfg_get <key-regex>
  [[ -f "$CONFIG" ]] || return 0
  sed -e 's/#.*$//' "$CONFIG" | grep -E "^[[:space:]]*$1[[:space:]]*=" | head -1 \
    | sed -E "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//; s/[[:space:]]*\$//; s/^\"//; s/\"\$//"
}
CFG_BOOTSTRAP="$(cfg_get bootstrap)"
CFG_SASL_USER="$(cfg_get 'sasl\.username')"
CFG_SASL_PASS="$(cfg_get 'sasl\.password')"
CFG_SASL_MECH="$(cfg_get 'sasl\.mechanism')"

BOOTSTRAP="${BOOTSTRAP:-${CFG_BOOTSTRAP:-127.0.0.1:9123}}"
SASL_USERNAME="${SASL_USERNAME:-$CFG_SASL_USER}"
SASL_PASSWORD="${SASL_PASSWORD:-$CFG_SASL_PASS}"
SASL_MECHANISM="${SASL_MECHANISM:-${CFG_SASL_MECH:-PLAIN}}"
# SASL turns on as soon as a username is present (matches the Rust runner).
if [[ -n "$SASL_USERNAME" ]]; then
  SECURITY_PROTOCOL="${SECURITY_PROTOCOL:-sasl}"
else
  SECURITY_PROTOCOL="${SECURITY_PROTOCOL:-PLAINTEXT}"
fi
DATABASE="${DATABASE:-fluss}"
TABLE_A="${TABLE_A:-bench_a}"
TABLE_B="${TABLE_B:-bench_b}"

# --- topology / load (identical to the executed run-isr-150mb-ab.sh round) ---
# At RF=2 (zero ISR slack) B's own high load plus the fire-and-forget A stressor
# shrink ISR to 1; no separate large table and no DDL recreate were used.
A_PROCS="${A_PROCS:-4}"
B_PROCS="${B_PROCS:-8}"
CONCURRENCY="${CONCURRENCY:-5}"       # threads per process
MAX_INFLIGHT="${MAX_INFLIGHT:-5}"     # per-bucket inflight requests
RF="${RF:-2}"                         # zero-slack: any follower lag -> ISR=1
BUCKETS="${BUCKETS:-32}"              # same table shape for A and B
PAYLOAD="${PAYLOAD:-1024}"           # row size in bytes (random per row; see cpp)
ACKS="${ACKS:-all}"
IDEMPOTENCE="${IDEMPOTENCE:-true}"
# B target throughput in MB/s (10^6 bytes); per-proc rate derived like the
# 150MB script: B_MBPS*1e6 / PAYLOAD / B_PROCS. 200MB @1024B -> 24414 rows/s/proc.
B_MBPS="${B_MBPS:-200}"
B_RATE="${B_RATE:-$(( B_MBPS * 1000000 / PAYLOAD / B_PROCS ))}"
# A stressor: fire-and-forget, unthrottled, zero backoff = the storm driver.
A_RATE="${A_RATE:-0}"
A_RETRY_BACKOFF_MS="${A_RETRY_BACKOFF_MS:-0}"

# --- backpressure knobs ---
BUFFER_MEMORY="${BUFFER_MEMORY:-268435456}"           # 256 MiB
BUFFER_WAIT_TIMEOUT_MS="${BUFFER_WAIT_TIMEOUT_MS:-1000}"
# B (observed) retry backoff, held constant across the wait/callback arms.
RETRY_BACKOFF_MS="${RETRY_BACKOFF_MS:-100}"
RETRY_MAX_BACKOFF_MS="${RETRY_MAX_BACKOFF_MS:-1000}"
LATENCY_THRESHOLD_MS="${LATENCY_THRESHOLD_MS:-30000}"
# callback arm native admission (SDK defaults)
CB_MAX_PENDING="${CB_MAX_PENDING:-262144}"
CB_ENQUEUE_TIMEOUT_MS="${CB_ENQUEUE_TIMEOUT_MS:-30000}"
WAIT_WORKERS="${WAIT_WORKERS:-4}"

# --- timing (seconds), same phase lengths as run-isr-150mb-ab.sh ---
SETTLE_SECONDS="${SETTLE_SECONDS:-120}"   # warmup: B settling at high load
STORM_SECONDS="${STORM_SECONDS:-300}"     # A stressor lifetime (self-exits)
RECOVER_SECONDS="${RECOVER_SECONDS:-180}" # watch B recover after A exits
SETTLE_BETWEEN="${SETTLE_BETWEEN:-90}"    # gap between the two arms

mkdir -p "$RUN_DIR"

common_args() {
  local sasl=""
  if [[ -n "$SASL_USERNAME" ]]; then
    sasl="--sasl-username $SASL_USERNAME --sasl-password $SASL_PASSWORD --sasl-mechanism $SASL_MECHANISM"
  fi
  echo "--bootstrap $BOOTSTRAP \
    --security-protocol $SECURITY_PROTOCOL \
    $sasl \
    --database $DATABASE \
    --concurrency $CONCURRENCY --buckets $BUCKETS --replication-factor $RF \
    --max-inflight-per-bucket $MAX_INFLIGHT \
    --payload-bytes $PAYLOAD --acks $ACKS --idempotence $IDEMPOTENCE \
    --buffer-memory $BUFFER_MEMORY --buffer-wait-timeout-ms $BUFFER_WAIT_TIMEOUT_MS \
    --retry-backoff-ms $RETRY_BACKOFF_MS --retry-max-backoff-ms $RETRY_MAX_BACKOFF_MS \
    --latency-threshold-ms $LATENCY_THRESHOLD_MS"
}

require_bin() {
  [[ -x "$BIN" ]] || { echo "binary not found: $BIN (run: $0 build)" >&2; exit 1; }
}

do_build() {
  echo "configuring (FLUSS_SDK_SOURCE=$SDK_SOURCE) ..."
  cmake -S "$SCRIPT_DIR" -B "$BUILD_DIR" -DFLUSS_SDK_SOURCE="$SDK_SOURCE"
  cmake --build "$BUILD_DIR" -j
  echo "built: $BIN"
}

# recreate_tables: drop+recreate A and B with THIS binary's schema (id/payload).
# Required because a table left over from a different writer (e.g. the Rust
# example's c1/c2 schema) makes every Append fail synchronously (submit_err).
recreate_tables() {
  require_bin
  local t
  for t in "$TABLE_B" "$TABLE_A"; do
    echo "  recreating $DATABASE.$t (buckets=$BUCKETS rf=$RF) ..."
    # shellcheck disable=SC2046
    "$BIN" $(common_args) --table "$t" --recreate
  done
}

# start_b <mode> <outdir>
start_b() {
  local mode="$1" out="$2"
  require_bin
  mkdir -p "$out"
  echo "  starting $B_PROCS B writers (mode=$mode) on $DATABASE.$TABLE_B ..."
  local run_secs=$((SETTLE_SECONDS + STORM_SECONDS + RECOVER_SECONDS + 30))
  local extra=""
  [[ "$mode" == "callback" ]] && extra="--callback-max-pending $CB_MAX_PENDING --callback-enqueue-timeout-ms $CB_ENQUEUE_TIMEOUT_MS"
  [[ "$mode" == "wait" ]] && extra="--wait-workers $WAIT_WORKERS"
  for i in $(seq 1 "$B_PROCS"); do
    # shellcheck disable=SC2046
    nohup "$BIN" $(common_args) --table "$TABLE_B" --mode "$mode" \
      --target-rate "$B_RATE" --run-seconds "$run_secs" \
      --run-id "$mode" --process-id "$i" $extra \
      >"$out/b-$i.log" 2>&1 &
    echo $! >>"$out/b.pids"
  done
}

# fire_a <outdir>: the fire-and-forget stressor on bench_a (self-exits after
# STORM_SECONDS). Same table shape as B, retry backoff=0 to drive the storm.
# The executed 150MB round used no DDL recreate: at RF=2 the load alone shrinks
# ISR. Set A_RECREATE=true to add an optional DDL kick.
fire_a() {
  local out="$1"
  require_bin
  local a_over="--retry-backoff-ms $A_RETRY_BACKOFF_MS"
  if [[ "${A_RECREATE:-false}" == "true" ]]; then
    echo "  recreate $DATABASE.$TABLE_A (optional DDL kick) ..."
    # shellcheck disable=SC2046
    "$BIN" $(common_args) --table "$TABLE_A" $a_over --recreate >"$out/a-recreate.log" 2>&1 || true
  fi
  echo "  starting $A_PROCS A stressors (forget, backoff=${A_RETRY_BACKOFF_MS}ms) for ${STORM_SECONDS}s ..."
  for i in $(seq 1 "$A_PROCS"); do
    # shellcheck disable=SC2046
    nohup "$BIN" $(common_args) --table "$TABLE_A" $a_over --mode forget \
      --target-rate "$A_RATE" --run-seconds "$STORM_SECONDS" \
      --run-id storm --process-id "$i" \
      >"$out/a-$i.log" 2>&1 &
    echo $! >>"$out/a.pids"
  done
}

stop_tag() {
  local out="$1" tag="$2" f="$1/$2.pids"
  [[ -f "$f" ]] || return 0
  while read -r pid; do kill "$pid" 2>/dev/null || true; done <"$f"
  rm -f "$f"
}

# run <mode>
run_arm() {
  local mode="$1"
  local out="$RUN_DIR/$mode"
  rm -rf "$out"; mkdir -p "$out"
  echo "== arm: $mode =="
  start_b "$mode" "$out"
  echo "  settling ${SETTLE_SECONDS}s ..."; sleep "$SETTLE_SECONDS"
  fire_a "$out"
  echo "  storm running ${STORM_SECONDS}s ..."; sleep "$STORM_SECONDS"
  stop_tag "$out" a
  echo "  recovering ${RECOVER_SECONDS}s ..."; sleep "$RECOVER_SECONDS"
  stop_tag "$out" b
  echo "== arm $mode done (logs in $out) =="
}

case "${1:-}" in
  build) do_build ;;
  recreate) recreate_tables ;;
  run) run_arm "${2:?usage: $0 run <wait|callback|forget>}" ;;
  ab)
    do_build
    recreate_tables
    run_arm wait
    echo "settle ${SETTLE_BETWEEN}s between arms ..."; sleep "$SETTLE_BETWEEN"
    run_arm callback
    ts="$(date +%Y%m%d-%H%M%S)"
    tar -C "$RUN_DIR/.." -czf "$SCRIPT_DIR/isr-storm-cpp-$ts.tar.gz" "$(basename "$RUN_DIR")"
    echo "packaged: $SCRIPT_DIR/isr-storm-cpp-$ts.tar.gz"
    ;;
  stop-all)
    for d in "$RUN_DIR"/*; do [[ -d "$d" ]] && { stop_tag "$d" a; stop_tag "$d" b; }; done
    ;;
  *)
    echo "usage: $0 {build|recreate|ab|run <mode>|stop-all}" >&2
    exit 1
    ;;
esac
