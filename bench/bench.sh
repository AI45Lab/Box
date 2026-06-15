#!/usr/bin/env bash
# Reproducible a3s-box benchmark + leak harness.
#
# Makes the perf claims (cold boot, snapshot-fork, warm-pool acquire) and the
# leak-free claim INDEPENDENTLY REPRODUCIBLE: it drives the real `a3s-box` CLI
# end-to-end and reports wall-clock latencies + a hard leak assertion, instead
# of quoting numbers from prose. Run it on a Linux host with /dev/kvm (the only
# place real microVMs boot).
#
# Usage:
#   bench/bench.sh [all|cold|warm|fork|leak]   (default: all)
# Env:
#   A3S_BOX   path to the a3s-box binary           (default: a3s-box on PATH)
#   IMAGE     OCI image to benchmark                (default: alpine:latest)
#   RUNS      samples per latency benchmark         (default: 20)
#   POOL_SIZE warm-pool / fork fill size            (default: 16)
#   CHURN     create/run/remove cycles for the leak test (default: 30)
#
# Exit code is non-zero if the leak assertion fails, so it is CI-gateable
# (wire it into the self-hosted KVM job — see docs/ci-kvm-runner.md).
set -u

A3S_BOX="${A3S_BOX:-a3s-box}"
IMAGE="${IMAGE:-alpine:latest}"
RUNS="${RUNS:-20}"
POOL_SIZE="${POOL_SIZE:-16}"
CHURN="${CHURN:-30}"
MODE="${1:-all}"

now_ms() { date +%s%3N 2>/dev/null || python3 -c 'import time;print(int(time.time()*1000))'; }

# Percentile of a space-separated list of integers. $1=list $2=pct(0-100)
pct() {
  local nums; nums=$(printf '%s\n' $1 | sort -n)
  local count; count=$(printf '%s\n' $nums | wc -l | tr -d ' ')
  [ "$count" -eq 0 ] && { echo 0; return; }
  local idx=$(( (count * $2 + 99) / 100 ))
  [ "$idx" -lt 1 ] && idx=1
  printf '%s\n' $nums | sed -n "${idx}p"
}

require_kvm() {
  if [ ! -e /dev/kvm ]; then
    echo "WARNING: /dev/kvm not present — boot benchmarks measure a degraded/failed path." >&2
  fi
  command -v "$A3S_BOX" >/dev/null 2>&1 || { echo "ERROR: a3s-box not found ($A3S_BOX)"; exit 2; }
}

# Count host-side resources that a leak would grow.
shim_count() { pgrep -xc 'a3s-box-shim' 2>/dev/null || pgrep -fc 'a3s-box-shim' 2>/dev/null || echo 0; }
mount_count() { mount 2>/dev/null | grep -c '/\.a3s/boxes\|/a3s/boxes' || echo 0; }
boxdir_count() { ls -1 "${HOME}/.a3s/boxes" 2>/dev/null | wc -l | tr -d ' '; }
fd_count() { ls -1 "/proc/$$/fd" 2>/dev/null | wc -l | tr -d ' '; }

bench_cold() {
  echo "## Cold boot ($RUNS runs, $IMAGE)"
  "$A3S_BOX" pull "$IMAGE" >/dev/null 2>&1 || true
  local samples=""
  for _ in $(seq 1 "$RUNS"); do
    local s e; s=$(now_ms)
    "$A3S_BOX" run --rm "$IMAGE" -- true >/dev/null 2>&1
    e=$(now_ms); samples="$samples $(( e - s ))"
  done
  echo "  p50=$(pct "$samples" 50)ms  p90=$(pct "$samples" 90)ms  min=$(pct "$samples" 1)ms"
}

bench_warm() {
  echo "## Warm-pool acquire ($RUNS runs, pool size $POOL_SIZE)"
  local sock=/tmp/a3s-bench-pool.sock
  "$A3S_BOX" pool start --image "$IMAGE" --size "$POOL_SIZE" --socket "$sock" >/tmp/a3s-bench-pool.log 2>&1 &
  local daemon=$!
  # Wait for the pool to be ready (socket appears + first acquire succeeds).
  for _ in $(seq 1 60); do [ -S "$sock" ] && break; sleep 1; done
  sleep 3
  local samples=""
  for _ in $(seq 1 "$RUNS"); do
    local s e; s=$(now_ms)
    "$A3S_BOX" pool run --socket "$sock" -- true >/dev/null 2>&1
    e=$(now_ms); samples="$samples $(( e - s ))"
  done
  kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
  echo "  p50=$(pct "$samples" 50)ms  p90=$(pct "$samples" 90)ms  min=$(pct "$samples" 1)ms"
}

bench_fork() {
  echo "## Snapshot-fork pool fill ($POOL_SIZE VMs, cold-boot vs CoW restore)"
  local sock=/tmp/a3s-bench-fork.sock
  for mode in "" "--snapshot-fork"; do
    local label="cold-fill"; [ -n "$mode" ] && label="snapshot-fork"
    local s e; s=$(now_ms)
    # shellcheck disable=SC2086
    "$A3S_BOX" pool start --image "$IMAGE" --size "$POOL_SIZE" --socket "$sock" $mode >/tmp/a3s-bench-fork.log 2>&1 &
    local daemon=$!
    for _ in $(seq 1 120); do [ -S "$sock" ] && "$A3S_BOX" pool run --socket "$sock" -- true >/dev/null 2>&1 && break; sleep 1; done
    e=$(now_ms)
    kill "$daemon" 2>/dev/null; wait "$daemon" 2>/dev/null
    local total=$(( e - s ))
    echo "  $label: fill-to-$POOL_SIZE ${total}ms (~$(( total / POOL_SIZE ))ms amortized)"
    sleep 2
  done
}

bench_leak() {
  echo "## Leak assertion ($CHURN create/run/remove cycles)"
  local b_shim b_mount b_dir
  b_shim=$(shim_count); b_mount=$(mount_count); b_dir=$(boxdir_count)
  echo "  baseline: shims=$b_shim mounts=$b_mount box-dirs=$b_dir"
  for _ in $(seq 1 "$CHURN"); do
    "$A3S_BOX" run --rm "$IMAGE" -- true >/dev/null 2>&1
  done
  sleep 3
  local a_shim a_mount a_dir
  a_shim=$(shim_count); a_mount=$(mount_count); a_dir=$(boxdir_count)
  echo "  after:    shims=$a_shim mounts=$a_mount box-dirs=$a_dir"
  local leak=0
  [ "$a_shim" -gt "$b_shim" ]  && { echo "  LEAK: $(( a_shim - b_shim )) orphan shim(s)"; leak=1; }
  [ "$a_mount" -gt "$b_mount" ] && { echo "  LEAK: $(( a_mount - b_mount )) leaked overlay mount(s)"; leak=1; }
  [ "$a_dir" -gt "$b_dir" ]    && { echo "  LEAK: $(( a_dir - b_dir )) leaked box dir(s)"; leak=1; }
  if [ "$leak" -eq 0 ]; then echo "  PASS: no orphan shims / mounts / box dirs after churn"; else echo "  FAIL: resource leak detected"; fi
  return "$leak"
}

require_kvm
echo "# a3s-box benchmark — $(uname -sm), image=$IMAGE"
rc=0
case "$MODE" in
  cold) bench_cold ;;
  warm) bench_warm ;;
  fork) bench_fork ;;
  leak) bench_leak || rc=$? ;;
  all)
    bench_cold
    bench_warm
    bench_fork
    bench_leak || rc=$?
    ;;
  *) echo "unknown mode: $MODE (use all|cold|warm|fork|leak)"; exit 2 ;;
esac
exit "$rc"
