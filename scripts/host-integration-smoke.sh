#!/usr/bin/env bash
#
# Run the macOS/Linux validation ladder for a3s-box.
#
# Default mode runs deterministic stub-backed checks that do not need a
# hypervisor. Pass --core, --host, --linux-run, --cri, or --all to run the
# ignored host-backed suites on machines with HVF/KVM and real guest assets.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE="$REPO_ROOT/src"

RUN_PURE=1
RUN_CORE=0
RUN_HOST=0
RUN_LINUX_RUN=0
RUN_CRI=0
RUN_SOAK=0
SOAK_DURATION_SECS="${A3S_BOX_SOAK_DURATION_SECS:-7200}"
SOAK_ITERATIONS="${A3S_BOX_SOAK_ITERATIONS:-0}"
SOAK_INTERVAL_SECS="${A3S_BOX_SOAK_INTERVAL_SECS:-0}"
SOAK_OUTPUT_DIR="${A3S_BOX_SOAK_OUTPUT_DIR:-}"
SOAK_RUN_BENCH="${A3S_BOX_SOAK_RUN_BENCH:-1}"
SOAK_VERIFY_MIN_DURATION_SECS="${A3S_BOX_SOAK_VERIFY_MIN_DURATION_SECS:-0}"
SOAK_VERIFY_MIN_SAMPLES="${A3S_BOX_SOAK_VERIFY_MIN_SAMPLES:-0}"
SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS="${A3S_BOX_SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS:-0}"
SOAK_VERIFY_MAX_SAMPLE_GAP_SECS="${A3S_BOX_SOAK_VERIFY_MAX_SAMPLE_GAP_SECS:-0}"
SOAK_RUN_ID=""
SOAK_EVIDENCE_DIR=""
SOAK_FAILURE_TRAP_ARMED=0
SOAK_FAILED_AT=""
SOAK_FAILED_COMMAND=""
SOAK_SUMMARY_ITERATIONS=0
SOAK_SUMMARY_FAILURES=0
SOAK_STARTED_EPOCH=0

usage() {
    cat <<'EOF'
Usage: scripts/host-integration-smoke.sh [options]

Options:
  --pure         Run stub-backed fmt, clippy, lib tests, and integration compile checks (default).
  --no-pure      Skip the stub-backed baseline checks.
  --core         Run the ignored real MicroVM core_smoke suite.
  --host         Run ignored host_smoke VM, Compose, and optional registry suites.
  --linux-run    Run the Linux-only Dockerfile RUN chroot smoke.
  --cri          Run the ignored crictl CRI smoke with A3S_BOX_CRI_SMOKE=1.
  --all          Run --core, --host, --linux-run, and --cri after the pure checks.
  --soak         Repeat selected real suites and leak/race checks for soak validation.
  --soak-duration SECS
                 Time limit for --soak (default: 7200, or A3S_BOX_SOAK_DURATION_SECS).
  --soak-iterations N
                 Optional iteration cap for --soak (default: 0 = time based only).
  --soak-interval SECS
                 Sleep between soak iterations (default: 0).
  --soak-output DIR
                 Evidence directory for --soak logs and resource samples.
  --soak-no-bench
                 Skip bench/bench.sh leak and race inside --soak.
  --soak-verify-min-duration-secs N
                 Require the final evidence summary duration to be at least N.
  --soak-verify-min-samples N
                 Require at least N resource samples in the final evidence.
  --soak-verify-min-sample-span-secs N
                 Require first-to-last resource sample span to be at least N.
  --soak-verify-max-sample-gap-secs N
                 Require consecutive resource samples to be no more than N seconds apart.
  -h, --help     Show this help.

Common environment:
  A3S_BOX_SMOKE_IMAGE_TAR=/path/to/alpine-oci.tar   Offline core_smoke image.
  A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar   Offline host/core image.
  A3S_BOX_SMOKE_SKIP_PULL=1                         Reuse preloaded core image.
  A3S_BOX_ALLOW_REGISTRY_PULL=1                     Allow live registry pulls.
  A3S_BOX_HOST_SMOKE_IMAGE=ref                      Host smoke image reference.
  A3S_BOX_HOST_SMOKE_TIMEOUT_SECS=300               Host smoke boot timeout.
  A3S_BOX_PUSH_TEST_REF=registry/repo:{tag}          Enable registry push smoke.
  A3S_BOX_CRI_CRICTL=/path/to/crictl                crictl binary for --cri.
  A3S_BOX_CRI_SMOKE_IMAGE=busybox:latest            CRI workload image.
  A3S_BOX_CRI_SMOKE_AGENT_IMAGE=agent:tag            CRI sandbox agent image.
  A3S_BOX_SOAK_DURATION_SECS=7200                   Default --soak duration.
  A3S_BOX_SOAK_ITERATIONS=3                         Optional --soak iteration cap.
  A3S_BOX_SOAK_OUTPUT_DIR=target/a3s-box-soak/run   Evidence output directory.
  A3S_BOX_SOAK_VERIFY_MIN_DURATION_SECS=7200        Gate evidence duration.
  A3S_BOX_SOAK_VERIFY_MIN_SAMPLES=4                 Gate evidence sample count.
  A3S_BOX_SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS=7200     Gate evidence sample span.
  A3S_BOX_SOAK_VERIFY_MAX_SAMPLE_GAP_SECS=0         Optional evidence sample gap gate.

Examples:
  scripts/host-integration-smoke.sh
  A3S_BOX_TEST_ALPINE_TAR=/tmp/alpine.tar scripts/host-integration-smoke.sh --core
  sudo -E scripts/host-integration-smoke.sh --linux-run
  A3S_BOX_TEST_ALPINE_TAR=/tmp/alpine.tar scripts/host-integration-smoke.sh --all
  A3S_BOX_TEST_ALPINE_TAR=/tmp/alpine.tar scripts/host-integration-smoke.sh --no-pure --core --host --soak
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --pure)
            RUN_PURE=1
            ;;
        --no-pure)
            RUN_PURE=0
            ;;
        --core)
            RUN_CORE=1
            ;;
        --host)
            RUN_HOST=1
            ;;
        --linux-run)
            RUN_LINUX_RUN=1
            ;;
        --cri)
            RUN_CRI=1
            ;;
        --all)
            RUN_CORE=1
            RUN_HOST=1
            RUN_LINUX_RUN=1
            RUN_CRI=1
            ;;
        --soak)
            RUN_SOAK=1
            ;;
        --soak-duration)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-duration requires a value" >&2
                exit 2
            fi
            SOAK_DURATION_SECS="$1"
            ;;
        --soak-iterations)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-iterations requires a value" >&2
                exit 2
            fi
            SOAK_ITERATIONS="$1"
            ;;
        --soak-interval)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-interval requires a value" >&2
                exit 2
            fi
            SOAK_INTERVAL_SECS="$1"
            ;;
        --soak-output)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-output requires a value" >&2
                exit 2
            fi
            SOAK_OUTPUT_DIR="$1"
            ;;
        --soak-no-bench)
            SOAK_RUN_BENCH=0
            ;;
        --soak-verify-min-duration-secs)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-verify-min-duration-secs requires a value" >&2
                exit 2
            fi
            SOAK_VERIFY_MIN_DURATION_SECS="$1"
            ;;
        --soak-verify-min-samples)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-verify-min-samples requires a value" >&2
                exit 2
            fi
            SOAK_VERIFY_MIN_SAMPLES="$1"
            ;;
        --soak-verify-min-sample-span-secs)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-verify-min-sample-span-secs requires a value" >&2
                exit 2
            fi
            SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS="$1"
            ;;
        --soak-verify-max-sample-gap-secs)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--soak-verify-max-sample-gap-secs requires a value" >&2
                exit 2
            fi
            SOAK_VERIFY_MAX_SAMPLE_GAP_SECS="$1"
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

log() {
    printf '\n==> %s\n' "$*"
}

one_line() {
    printf '%s' "$1" | tr '\012\011' '  '
}

run() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    "$@" || {
        local rc="$?"
        if [ "$SOAK_FAILURE_TRAP_ARMED" -eq 1 ]; then
            if [ -z "$SOAK_FAILED_AT" ]; then
                SOAK_FAILED_AT="${BASH_SOURCE[1]:-${BASH_SOURCE[0]}}:${BASH_LINENO[0]:-$LINENO}"
            fi
            if [ -z "$SOAK_FAILED_COMMAND" ]; then
                SOAK_FAILED_COMMAND="$(one_line "$*")"
            fi
        fi
        return "$rc"
    }
}

record_soak_failure() {
    local rc="$?"
    if [ "$SOAK_FAILURE_TRAP_ARMED" -eq 1 ]; then
        if [ -z "$SOAK_FAILED_AT" ]; then
            SOAK_FAILED_AT="${BASH_SOURCE[1]:-$0}:${BASH_LINENO[0]:-$LINENO}"
        fi
        if [ -z "$SOAK_FAILED_COMMAND" ]; then
            SOAK_FAILED_COMMAND="$(one_line "$BASH_COMMAND")"
        fi
    fi
    return "$rc"
}

host_os() {
    uname -s
}

host_arch() {
    uname -m
}

stub_dir=""

ensure_stub_libkrun() {
    if [ -n "$stub_dir" ]; then
        return
    fi

    stub_dir="$(mktemp -d "${TMPDIR:-/tmp}/a3s-box-stub-libkrun.XXXXXX")"
    cat >"$stub_dir/krun_stub.c" <<'EOF'
void krun_stub(void) {}
EOF

    case "$(host_os)" in
        Darwin)
            run cc -dynamiclib -o "$stub_dir/libkrun.dylib" "$stub_dir/krun_stub.c"
            ;;
        Linux)
            run cc -shared -fPIC -o "$stub_dir/libkrun.so" "$stub_dir/krun_stub.c"
            ;;
        *)
            echo "stub checks are only supported on macOS and Linux by this runner" >&2
            exit 1
            ;;
    esac
}

run_stub() {
    ensure_stub_libkrun
    printf '+ A3S_DEPS_STUB=1'
    printf ' %q' "$@"
    printf '\n'
    env \
        A3S_DEPS_STUB=1 \
        LIBRARY_PATH="$stub_dir${LIBRARY_PATH:+:$LIBRARY_PATH}" \
        LD_LIBRARY_PATH="$stub_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        DYLD_LIBRARY_PATH="$stub_dir${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
        "$@"
}

run_real() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    env -u A3S_DEPS_STUB "$@"
}

have_cmd() {
    command -v "$1" >/dev/null 2>&1
}

is_non_negative_int() {
    case "$1" in
        ''|*[!0-9]*)
            return 1
            ;;
        *)
            return 0
            ;;
    esac
}

guest_target() {
    case "$(host_arch)" in
        arm64|aarch64)
            echo "aarch64-unknown-linux-musl"
            ;;
        x86_64|amd64)
            echo "x86_64-unknown-linux-musl"
            ;;
        *)
            echo "unsupported"
            ;;
    esac
}

guest_init_exists() {
    local target
    target="$(guest_target)"
    if [ -x "$WORKSPACE/target/$target/debug/a3s-box-guest-init" ] ||
        [ -x "$WORKSPACE/target/$target/release/a3s-box-guest-init" ]; then
        return 0
    fi
    if [ "$(host_os)" = "Linux" ]; then
        [ -x "$WORKSPACE/target/debug/a3s-box-guest-init" ] ||
            [ -x "$WORKSPACE/target/release/a3s-box-guest-init" ]
        return
    fi
    return 1
}

offline_image_tar() {
    if [ -n "${A3S_BOX_TEST_ALPINE_TAR:-}" ]; then
        echo "$A3S_BOX_TEST_ALPINE_TAR"
        return
    fi
    if [ -n "${A3S_BOX_SMOKE_IMAGE_TAR:-}" ]; then
        echo "$A3S_BOX_SMOKE_IMAGE_TAR"
    fi
}

prepare_offline_image_env() {
    local tar_path
    tar_path="$(offline_image_tar)"
    if [ -z "$tar_path" ]; then
        return
    fi
    if [ ! -f "$tar_path" ]; then
        echo "configured OCI archive does not exist: $tar_path" >&2
        exit 1
    fi

    export A3S_BOX_TEST_ALPINE_TAR="${A3S_BOX_TEST_ALPINE_TAR:-$tar_path}"
    export A3S_BOX_SMOKE_IMAGE_TAR="${A3S_BOX_SMOKE_IMAGE_TAR:-$tar_path}"
}

require_image_source() {
    local suite="$1"
    prepare_offline_image_env
    if [ -n "$(offline_image_tar)" ]; then
        return
    fi
    if [ "${A3S_BOX_ALLOW_REGISTRY_PULL:-}" = "1" ]; then
        return
    fi

    cat >&2 <<EOF
$suite requires a runnable Linux image source.
Set A3S_BOX_TEST_ALPINE_TAR or A3S_BOX_SMOKE_IMAGE_TAR to an OCI archive for
reproducible offline smoke testing. To intentionally use live registry pulls,
set A3S_BOX_ALLOW_REGISTRY_PULL=1.
EOF
    exit 1
}

build_guest_init() {
    case "$(host_os)" in
        Linux)
            run_real cargo build -p a3s-box-guest-init
            ;;
        Darwin)
            local target
            target="$(guest_target)"
            if [ "$target" = "unsupported" ]; then
                echo "unsupported macOS architecture for guest init cross-build: $(host_arch)" >&2
                exit 1
            fi
            if have_cmd cargo-zigbuild; then
                run_real cargo zigbuild -p a3s-box-guest-init --target "$target"
            elif guest_init_exists; then
                log "Using existing Linux guest init binary"
            else
                cat >&2 <<EOF
Linux guest init binary is missing.
Build it for the Linux guest target, then rerun:
  rustup target add $target
  cargo build -p a3s-box-guest-init --target $target
If direct cross-build linking fails, install cargo-zigbuild and use:
  cargo install cargo-zigbuild
  cargo zigbuild -p a3s-box-guest-init --target $target
Expected artifact: $WORKSPACE/target/$target/debug/a3s-box-guest-init or
  $WORKSPACE/target/$target/release/a3s-box-guest-init
EOF
                exit 1
            fi
            ;;
        *)
            echo "real host integration is only supported on macOS and Linux by this runner" >&2
            exit 1
            ;;
    esac
}

build_real_binaries() {
    log "Building real host binaries"
    run_real cargo build -p a3s-box-cli -p a3s-box-shim
    build_guest_init
}

run_pure_suite() {
    log "Running stub-backed baseline checks"
    run cargo fmt --all -- --check
    run_stub cargo clippy --workspace --all-targets --all-features -- -D warnings
    run_stub cargo test --workspace --lib
    run_stub cargo test --workspace --tests
}

run_core_suite() {
    require_image_source "core smoke"
    build_real_binaries
    log "Running real MicroVM core smoke"
    run_real cargo test -p a3s-box-cli --test core_smoke -- --ignored --nocapture --test-threads=1
}

run_host_suite() {
    require_image_source "host smoke"
    build_real_binaries
    log "Running host VM command matrix"
    run_real cargo test -p a3s-box-cli --test host_smoke test_real_vm_command_matrix -- --ignored --nocapture --test-threads=1
    log "Running host Compose smoke"
    run_real cargo test -p a3s-box-cli --test host_smoke test_real_compose_smoke -- --ignored --nocapture --test-threads=1

    if [ -n "${A3S_BOX_PUSH_TEST_REF:-}" ]; then
        log "Running registry push smoke"
        run_real cargo test -p a3s-box-cli --test host_smoke test_real_packages_service_push -- --ignored --nocapture --test-threads=1
    else
        log "Skipping registry push smoke; set A3S_BOX_PUSH_TEST_REF to enable it"
    fi
}

run_linux_run_suite() {
    if [ "$(host_os)" != "Linux" ]; then
        log "Skipping Linux Dockerfile RUN smoke on non-Linux host"
        return
    fi

    if [ "$(id -u)" != "0" ]; then
        log "Skipping Linux Dockerfile RUN smoke; rerun with sudo -E for chroot coverage"
        return
    fi

    if [ -z "${A3S_BOX_TEST_ALPINE_TAR:-}" ]; then
        log "Skipping Linux Dockerfile RUN smoke; set A3S_BOX_TEST_ALPINE_TAR"
        return
    fi

    build_real_binaries
    log "Running Linux Dockerfile RUN chroot smoke"
    run_real cargo test -p a3s-box-cli --test host_smoke test_linux_build_run_chroot_smoke -- --ignored --nocapture --test-threads=1
}

run_cri_suite() {
    build_real_binaries
    log "Building CRI server"
    run_real cargo build -p a3s-box-cri
    log "Running crictl CRI smoke"
    printf '+ A3S_BOX_CRI_SMOKE=1 cargo test -p a3s-box-cri --test crictl_smoke -- --ignored --nocapture --test-threads=1\n'
    env -u A3S_DEPS_STUB A3S_BOX_CRI_SMOKE=1 \
        cargo test -p a3s-box-cri --test crictl_smoke -- --ignored --nocapture --test-threads=1
}

shim_count() {
    local count
    count="$(pgrep -xc 'a3s-box-shim' 2>/dev/null || pgrep -fc 'a3s-box-shim' 2>/dev/null || true)"
    echo "${count:-0}"
}

mount_count() {
    mount 2>/dev/null | awk '/\/\.a3s\/boxes|\/a3s\/boxes/ { n++ } END { print n + 0 }'
}

boxdir_count() {
    local boxes_dir
    boxes_dir="$(a3s_home_dir)/boxes"
    find "$boxes_dir" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' '
}

host_socket_root() {
    case "$(host_os)" in
        Darwin)
            echo "/private/tmp/a3s-box-sockets"
            ;;
        *)
            echo "${TMPDIR:-/tmp}/a3s-box-sockets"
            ;;
    esac
}

socketdir_count() {
    local root
    root="$(host_socket_root)"
    find "$root" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' '
}

a3s_home_dir() {
    echo "${A3S_HOME:-$HOME/.a3s}"
}

a3s_home_bytes() {
    local home
    home="$(a3s_home_dir)"
    if [ ! -d "$home" ]; then
        echo 0
        return
    fi
    du -sk "$home" 2>/dev/null | awk '{print $1 * 1024}' || echo 0
}

write_resource_sample() {
    local phase="$1"
    local file="$SOAK_EVIDENCE_DIR/resource-samples.tsv"

    if [ ! -f "$file" ]; then
        printf 'timestamp\tphase\tshims\tmounts\tbox_dirs\tsocket_dirs\ta3s_home_bytes\n' >"$file"
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$phase" \
        "$(shim_count)" \
        "$(mount_count)" \
        "$(boxdir_count)" \
        "$(socketdir_count)" \
        "$(a3s_home_bytes)" >>"$file"
}

write_soak_metadata() {
    local file="$SOAK_EVIDENCE_DIR/metadata.txt"
    {
        echo "run_id=$SOAK_RUN_ID"
        echo "started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "repo_root=$REPO_ROOT"
        echo "workspace=$WORKSPACE"
        echo "git_sha=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)"
        echo "git_status_short:"
        git -C "$REPO_ROOT" status --short 2>/dev/null || true
        echo "host:"
        uname -a
        echo "rust:"
        rustc --version 2>/dev/null || true
        cargo --version 2>/dev/null || true
        echo "a3s_box:"
        "${A3S_BOX:-a3s-box}" --version 2>/dev/null || true
        echo "selected_suites=core=$RUN_CORE host=$RUN_HOST linux_run=$RUN_LINUX_RUN cri=$RUN_CRI bench=$SOAK_RUN_BENCH"
        echo "soak_duration_secs=$SOAK_DURATION_SECS"
        echo "soak_iterations=$SOAK_ITERATIONS"
        echo "soak_interval_secs=$SOAK_INTERVAL_SECS"
        echo "soak_verify_min_duration_secs=$SOAK_VERIFY_MIN_DURATION_SECS"
        echo "soak_verify_min_samples=$SOAK_VERIFY_MIN_SAMPLES"
        echo "soak_verify_min_sample_span_secs=$SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS"
        echo "soak_verify_max_sample_gap_secs=$SOAK_VERIFY_MAX_SAMPLE_GAP_SECS"
        echo "environment:"
        env | grep -E '^A3S_BOX|^A3S_HOME=|^IMAGE=|^CHURN=|^RACE=|^RUNS=|^POOL_SIZE=' | sort || true
    } >"$file"
}

capture_cli_snapshot() {
    local label="$1"
    local bin="${A3S_BOX:-a3s-box}"
    if ! have_cmd "$bin"; then
        echo "a3s-box binary not found: $bin" >"$SOAK_EVIDENCE_DIR/${label}-cli-snapshot.txt"
        return
    fi

    {
        echo "# a3s-box ps -a"
        "$bin" ps -a 2>&1 || true
        echo
        echo "# a3s-box images"
        "$bin" images 2>&1 || true
        echo
        echo "# a3s-box volume ls"
        "$bin" volume ls 2>&1 || true
        echo
        echo "# a3s-box snapshot ls"
        "$bin" snapshot ls 2>&1 || true
    } >"$SOAK_EVIDENCE_DIR/${label}-cli-snapshot.txt"
}

run_bench_leak() {
    A3S_BOX="${A3S_BOX:-$WORKSPACE/target/debug/a3s-box}" run_real "$REPO_ROOT/bench/bench.sh" leak
}

run_bench_race() {
    A3S_BOX="${A3S_BOX:-$WORKSPACE/target/debug/a3s-box}" run_real "$REPO_ROOT/bench/bench.sh" race
}

run_soak_step() {
    local iteration="$1"
    local name="$2"
    shift 2

    local log_file="$SOAK_EVIDENCE_DIR/iteration-${iteration}-${name}.log"
    log "Soak iteration $iteration: $name (log: $log_file)"

    set +e
    ( "$@" ) >"$log_file" 2>&1
    local rc=$?
    set -e

    if [ "$rc" -eq 0 ]; then
        echo "  PASS: $name"
    else
        if [ "$SOAK_FAILURE_TRAP_ARMED" -eq 1 ]; then
            if [ -z "$SOAK_FAILED_AT" ]; then
                SOAK_FAILED_AT="iteration-${iteration}-${name}"
            fi
            if [ -z "$SOAK_FAILED_COMMAND" ]; then
                SOAK_FAILED_COMMAND="$(one_line "$*")"
            fi
        fi
        echo "  FAIL: $name exited with $rc" >&2
        tail -80 "$log_file" >&2 || true
    fi

    return "$rc"
}

run_soak_iteration() {
    local iteration="$1"
    local rc=0

    write_resource_sample "iteration-${iteration}-before"

    if [ "$RUN_CORE" -eq 1 ]; then
        run_soak_step "$iteration" core run_core_suite || rc=1
    fi
    if [ "$RUN_HOST" -eq 1 ]; then
        run_soak_step "$iteration" host run_host_suite || rc=1
    fi
    if [ "$RUN_LINUX_RUN" -eq 1 ]; then
        run_soak_step "$iteration" linux-run run_linux_run_suite || rc=1
    fi
    if [ "$RUN_CRI" -eq 1 ]; then
        run_soak_step "$iteration" cri run_cri_suite || rc=1
    fi
    if [ "$SOAK_RUN_BENCH" -eq 1 ]; then
        run_soak_step "$iteration" bench-leak run_bench_leak || rc=1
        run_soak_step "$iteration" bench-race run_bench_race || rc=1
    fi

    capture_cli_snapshot "iteration-${iteration}"
    write_resource_sample "iteration-${iteration}-after"
    return "$rc"
}

write_soak_summary() {
    local result="$1"
    local iterations="$2"
    local failures="$3"
    local exit_code="${4:-}"
    local now duration
    now="$(date +%s)"
    if [ "$SOAK_STARTED_EPOCH" -gt 0 ]; then
        duration=$((now - SOAK_STARTED_EPOCH))
    else
        duration=0
    fi

    {
        echo "finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "result=$result"
        echo "duration_secs=$duration"
        echo "iterations=$iterations"
        echo "failed_iterations=$failures"
        if [ -n "$exit_code" ]; then
            echo "exit_code=$exit_code"
            echo "failed_at=${SOAK_FAILED_AT:-unknown}"
            echo "failed_command=${SOAK_FAILED_COMMAND:-unknown}"
        fi
        echo "evidence_dir=$SOAK_EVIDENCE_DIR"
    } >"$SOAK_EVIDENCE_DIR/summary.txt"
}

run_soak_verifier() {
    local args=("$REPO_ROOT/deploy/scripts/verify-soak-evidence.sh" --kind host)
    local output="$SOAK_EVIDENCE_DIR/verify.out"
    local rc

    if [ "$SOAK_VERIFY_MIN_DURATION_SECS" -gt 0 ]; then
        args+=(--min-duration-secs "$SOAK_VERIFY_MIN_DURATION_SECS")
    fi
    if [ "$SOAK_VERIFY_MIN_SAMPLES" -gt 0 ]; then
        args+=(--min-samples "$SOAK_VERIFY_MIN_SAMPLES")
    fi
    if [ "$SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS" -gt 0 ]; then
        args+=(--min-sample-span-secs "$SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS")
    fi
    if [ "$SOAK_VERIFY_MAX_SAMPLE_GAP_SECS" -gt 0 ]; then
        args+=(--max-sample-gap-secs "$SOAK_VERIFY_MAX_SAMPLE_GAP_SECS")
    fi

    set +e
    run "${args[@]}" "$SOAK_EVIDENCE_DIR" >"$output" 2>&1
    rc="$?"
    set -e
    cat "$output"
    return "$rc"
}

handle_soak_exit() {
    local rc="$1"
    if [ "$rc" -eq 0 ] || [ "$SOAK_FAILURE_TRAP_ARMED" -eq 0 ]; then
        return
    fi

    set +e
    trap - ERR
    trap - EXIT
    if [ -n "$SOAK_EVIDENCE_DIR" ] && [ -d "$SOAK_EVIDENCE_DIR" ]; then
        write_resource_sample "final" || true
        capture_cli_snapshot "final" || true
        write_soak_summary "fail" "$SOAK_SUMMARY_ITERATIONS" "$SOAK_SUMMARY_FAILURES" "$rc"
        log "Host soak failed: exit_code=$rc evidence=$SOAK_EVIDENCE_DIR"
    fi
    exit "$rc"
}

run_soak_suite() {
    if ! is_non_negative_int "$SOAK_DURATION_SECS" ||
        ! is_non_negative_int "$SOAK_ITERATIONS" ||
        ! is_non_negative_int "$SOAK_INTERVAL_SECS" ||
        ! is_non_negative_int "$SOAK_RUN_BENCH" ||
        ! is_non_negative_int "$SOAK_VERIFY_MIN_DURATION_SECS" ||
        ! is_non_negative_int "$SOAK_VERIFY_MIN_SAMPLES" ||
        ! is_non_negative_int "$SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS" ||
        ! is_non_negative_int "$SOAK_VERIFY_MAX_SAMPLE_GAP_SECS"; then
        echo "soak duration, iterations, interval, bench flag, and verifier gates must be non-negative integers" >&2
        exit 2
    fi
    if [ "$SOAK_DURATION_SECS" -eq 0 ] && [ "$SOAK_ITERATIONS" -eq 0 ]; then
        echo "--soak requires a positive duration or iteration cap" >&2
        exit 2
    fi
    if [ "$SOAK_RUN_BENCH" -ne 0 ] && [ "$SOAK_RUN_BENCH" -ne 1 ]; then
        echo "soak bench flag must be 0 or 1" >&2
        exit 2
    fi
    if [ "$RUN_CORE" -eq 0 ] &&
        [ "$RUN_HOST" -eq 0 ] &&
        [ "$RUN_LINUX_RUN" -eq 0 ] &&
        [ "$RUN_CRI" -eq 0 ] &&
        [ "$SOAK_RUN_BENCH" -eq 0 ]; then
        echo "--soak has no selected work; choose --core, --host, --linux-run, --cri, or leave bench enabled" >&2
        exit 2
    fi

    SOAK_RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
    SOAK_EVIDENCE_DIR="${SOAK_OUTPUT_DIR:-$WORKSPACE/target/a3s-box-soak/$SOAK_RUN_ID}"
    export SOAK_RUN_ID SOAK_EVIDENCE_DIR
    mkdir -p "$SOAK_EVIDENCE_DIR"
    SOAK_FAILURE_TRAP_ARMED=1
    trap 'record_soak_failure' ERR
    trap 'handle_soak_exit "$?"' EXIT

    log "Writing soak evidence to $SOAK_EVIDENCE_DIR"
    write_soak_metadata
    write_resource_sample "start"
    capture_cli_snapshot "start"

    local start end iteration failures
    start="$(date +%s)"
    SOAK_STARTED_EPOCH="$start"
    if [ "$SOAK_DURATION_SECS" -gt 0 ]; then
        end=$((start + SOAK_DURATION_SECS))
    else
        end=0
    fi
    iteration=0
    failures=0

    while :; do
        if [ "$SOAK_ITERATIONS" -gt 0 ] && [ "$iteration" -ge "$SOAK_ITERATIONS" ]; then
            break
        fi
        if [ "$iteration" -gt 0 ] && [ "$end" -gt 0 ] && [ "$(date +%s)" -ge "$end" ]; then
            break
        fi

        iteration=$((iteration + 1))
        if ! run_soak_iteration "$iteration"; then
            failures=$((failures + 1))
        fi
        SOAK_SUMMARY_ITERATIONS="$iteration"
        SOAK_SUMMARY_FAILURES="$failures"

        if [ "$SOAK_ITERATIONS" -gt 0 ] && [ "$iteration" -ge "$SOAK_ITERATIONS" ]; then
            break
        fi
        if [ "$end" -gt 0 ] && [ "$(date +%s)" -ge "$end" ]; then
            break
        fi
        if [ "$SOAK_INTERVAL_SECS" -gt 0 ]; then
            sleep "$SOAK_INTERVAL_SECS"
        fi
    done

    write_resource_sample "final"
    capture_cli_snapshot "final"
    if [ "$failures" -eq 0 ]; then
        write_soak_summary "pass" "$iteration" "$failures"
    else
        write_soak_summary "fail" "$iteration" "$failures"
    fi

    log "Soak completed: iterations=$iteration failures=$failures evidence=$SOAK_EVIDENCE_DIR"
    run_soak_verifier
    [ "$failures" -eq 0 ]
}

cd "$WORKSPACE"

case "$(host_os)" in
    Darwin|Linux)
        ;;
    *)
        echo "This runner targets macOS and Linux. Detected: $(host_os)" >&2
        exit 1
        ;;
esac

if [ "$RUN_SOAK" -eq 1 ]; then
    if [ "$RUN_PURE" -eq 1 ]; then
        run_pure_suite
    fi
    run_soak_suite
else
    if [ "$RUN_PURE" -eq 1 ]; then
        run_pure_suite
    fi

    if [ "$RUN_CORE" -eq 1 ]; then
        run_core_suite
    fi

    if [ "$RUN_HOST" -eq 1 ]; then
        run_host_suite
    fi

    if [ "$RUN_LINUX_RUN" -eq 1 ]; then
        run_linux_run_suite
    fi

    if [ "$RUN_CRI" -eq 1 ]; then
        run_cri_suite
    fi
fi

log "Host integration runner completed"
