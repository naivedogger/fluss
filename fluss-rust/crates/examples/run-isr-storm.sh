#!/usr/bin/env bash
# Orchestrates the ISR retry-storm A/B test:
#   - Table B: many processes writing continuously (the "innocent bystander").
#   - Table A: fewer processes; we delete + recreate A mid-run and resume the
#     same traffic, then watch the blast radius on B.
#
# All knobs are env vars with sane defaults; override on the command line, e.g.
#   RETRY_BACKOFF_MS=0 ./run-isr-storm.sh b-start
#
# Split-role A/B (recommended). Backpressure protects the job that has it, so a
# single backpressured job may not reproduce the storm. Keep the stressor on
# table A fire-and-forget and put the config under test on the observed table B.
# The stressor is transient: fire it once mid-run for ~5 min so it self-exits
# (its own RUN_SECONDS), then watch B recover. Env is read per invocation, so
# launch the two roles with different env:
#   # arm 1 (baseline observed): long run, unbounded measure-only + zero backoff
#   AWAIT_COMPLETIONS=true MAX_IN_FLIGHT_APPENDS=0 RETRY_BACKOFF_MS=0 \
#     TARGET_RATE=<healthy> RUN_SECONDS=900 ./run-isr-storm.sh b-start
#   # let B settle at baseline, then fire the stressor for 5 min (self-exits):
#   AWAIT_COMPLETIONS=false RETRY_BACKOFF_MS=0 RUN_SECONDS=300 ./run-isr-storm.sh a-start
#   ./run-isr-storm.sh recreate   # optional DDL kick during the burst
#   # arm 2 (fixed observed): bounded backpressure + backoff on, same stressor
#   AWAIT_COMPLETIONS=true MAX_IN_FLIGHT_APPENDS=10000 RETRY_BACKOFF_MS=100 \
#     TARGET_RATE=<healthy> RUN_SECONDS=900 ./run-isr-storm.sh b-start
#   AWAIT_COMPLETIONS=false RETRY_BACKOFF_MS=0 RUN_SECONDS=300 ./run-isr-storm.sh a-start
# Compare B's per-second p99/max/>Ns during the stressor window and how fast
# they fall back after A exits; the end-of-run cumulative ratio is diluted by
# the long healthy period, so keep B short or read the monitor timeline.
#
# Subcommands:
#   b-start     launch B writers (continuous)
#   a-start     launch A writers
#   a-stop      stop only the A writers
#   recreate    drop + recreate table A and wait until it is writable
#   trigger     a-stop -> recreate -> a-start  (the full delete/recreate cycle)
#   stop-all    stop every writer this script started
#   status      show running pids
set -euo pipefail

# --- Resolve paths -----------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# fluss-rust workspace root is two levels up from crates/examples.
WS_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Prefer a release binary; fall back to debug. Override with BIN=/path.
if [[ -z "${BIN:-}" ]]; then
  if [[ -x "$WS_ROOT/target/release/examples/example-isr-retry-storm" ]]; then
    BIN="$WS_ROOT/target/release/examples/example-isr-retry-storm"
  else
    BIN="$WS_ROOT/target/debug/examples/example-isr-retry-storm"
  fi
fi

CONFIG="${CONFIG:-$SCRIPT_DIR/isr-storm.conf}"
RUN_DIR="${RUN_DIR:-$SCRIPT_DIR/isr-run}"
mkdir -p "$RUN_DIR"

# --- Test parameters (override via env) --------------------------------------
DATABASE="${DATABASE:-fluss}"
TABLE_A="${TABLE_A:-bench_a}"
TABLE_B="${TABLE_B:-bench_b}"
A_PROCS="${A_PROCS:-4}"
B_PROCS="${B_PROCS:-8}"
CONCURRENCY="${CONCURRENCY:-5}"       # threads per process
MAX_INFLIGHT="${MAX_INFLIGHT:-5}"
IDEMPOTENCE="${IDEMPOTENCE:-true}"
ACKS="${ACKS:-all}"
BUCKETS="${BUCKETS:-32}"
RF="${RF:-3}"
PAYLOAD="${PAYLOAD:-256}"
# Per-process target rows/sec (0 = unthrottled). Cap this to keep the baseline
# healthy so a 4-node cluster is not saturated before the ISR event.
TARGET_RATE="${TARGET_RATE:-0}"
RUN_SECONDS="${RUN_SECONDS:-3600}"
READY_TIMEOUT="${READY_TIMEOUT:-120}"
# The A/B knob: 0 = pre-fix zero-backoff storm, 100 = fixed exponential backoff.
RETRY_BACKOFF_MS="${RETRY_BACKOFF_MS:-100}"
MAX_BACKOFF_MS="${MAX_BACKOFF_MS:-1000}"
# Client-backpressure mode. true = each writer registers a completion callback
# per append, bounds outstanding un-acked appends to MAX_IN_FLIGHT_APPENDS, and
# records end-to-end (submit -> ack) latency incl. the share over
# LATENCY_THRESHOLD_MS. false = fire-and-forget (no backpressure, no latency).
AWAIT_COMPLETIONS="${AWAIT_COMPLETIONS:-false}"
MAX_IN_FLIGHT_APPENDS="${MAX_IN_FLIGHT_APPENDS:-10000}"
LATENCY_THRESHOLD_MS="${LATENCY_THRESHOLD_MS:-30000}"
# Byte-based buffer backpressure (the built-in mechanism). BUFFER_MEMORY bounds
# outstanding un-acked bytes per Connection; when full, append blocks up to
# BUFFER_WAIT_TIMEOUT_MS then returns BufferExhausted (shed, shows up as thr/s).
# To exercise buffer backpressure without the count semaphore, set
# MAX_IN_FLIGHT_APPENDS=0 and tune these: shrink BUFFER_MEMORY to trigger it
# sooner, raise BUFFER_WAIT_TIMEOUT_MS to block-and-pace instead of shed.
BUFFER_MEMORY="${BUFFER_MEMORY:-268435456}"          # 256 MiB
BUFFER_WAIT_TIMEOUT_MS="${BUFFER_WAIT_TIMEOUT_MS:-1000}"

common_args() {
  local await_flag=""
  if [[ "$AWAIT_COMPLETIONS" == "true" ]]; then
    await_flag="--await-completions"
  fi
  echo "--config $CONFIG \
    --database $DATABASE \
    --concurrency $CONCURRENCY \
    --max-inflight $MAX_INFLIGHT \
    --idempotence $IDEMPOTENCE \
    --acks $ACKS \
    --buckets $BUCKETS \
    --replication-factor $RF \
    --payload-bytes $PAYLOAD \
    --target-rate $TARGET_RATE \
    --run-seconds $RUN_SECONDS \
    --ready-timeout-secs $READY_TIMEOUT \
    --retry-backoff-ms $RETRY_BACKOFF_MS \
    --retry-max-backoff-ms $MAX_BACKOFF_MS \
    --max-in-flight-appends $MAX_IN_FLIGHT_APPENDS \
    --latency-threshold-ms $LATENCY_THRESHOLD_MS \
    --buffer-memory $BUFFER_MEMORY \
    --buffer-wait-timeout-ms $BUFFER_WAIT_TIMEOUT_MS \
    $await_flag \
    --metrics-port 0"
}

require_bin() {
  if [[ ! -x "$BIN" ]]; then
    echo "binary not found: $BIN" >&2
    echo "build it first:  (cd $WS_ROOT && cargo build --release -p fluss-examples --example example-isr-retry-storm)" >&2
    exit 1
  fi
}

# start_writers <table> <count> <tag>
start_writers() {
  local table="$1" count="$2" tag="$3"
  require_bin
  echo "starting $count writers on $DATABASE.$table (backoff=${RETRY_BACKOFF_MS}ms await_completions=${AWAIT_COMPLETIONS}) ..."
  for i in $(seq 1 "$count"); do
    local log="$RUN_DIR/${tag}-${i}.log"
    # shellcheck disable=SC2046
    nohup "$BIN" $(common_args) --table "$table" >"$log" 2>&1 &
    echo $! >>"$RUN_DIR/${tag}.pids"
    echo "  [$tag-$i] pid=$! log=$log"
  done
}

stop_writers() {
  local tag="$1"
  local f="$RUN_DIR/${tag}.pids"
  [[ -f "$f" ]] || { echo "no $tag writers tracked"; return 0; }
  echo "stopping $tag writers ..."
  while read -r pid; do
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      echo "  killed pid=$pid"
    fi
  done <"$f"
  rm -f "$f"
}

recreate_a() {
  require_bin
  echo "drop + recreate $DATABASE.$TABLE_A, waiting until writable ..."
  # shellcheck disable=SC2046
  "$BIN" $(common_args) --table "$TABLE_A" --recreate
}

case "${1:-}" in
  b-start)  start_writers "$TABLE_B" "$B_PROCS" b ;;
  a-start)  start_writers "$TABLE_A" "$A_PROCS" a ;;
  a-stop)   stop_writers a ;;
  recreate) recreate_a ;;
  trigger)
    stop_writers a
    recreate_a
    start_writers "$TABLE_A" "$A_PROCS" a
    ;;
  stop-all) stop_writers a; stop_writers b ;;
  status)
    for tag in a b; do
      f="$RUN_DIR/${tag}.pids"
      [[ -f "$f" ]] || continue
      echo "== $tag =="
      while read -r pid; do
        kill -0 "$pid" 2>/dev/null && echo "  running pid=$pid" || echo "  dead    pid=$pid"
      done <"$f"
    done
    ;;
  *)
    echo "usage: $0 {b-start|a-start|a-stop|recreate|trigger|stop-all|status}" >&2
    exit 1
    ;;
esac
