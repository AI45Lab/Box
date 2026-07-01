#!/usr/bin/env bash
#
# Run the macOS/Linux validation ladder for a3s-box.
#
# Default mode runs deterministic stub-backed checks that do not need a
# hypervisor. Pass --core, --host, --linux-run, --cri, or --all to run the
# ignored host-backed suites on machines with HVF/KVM and real guest assets.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE="$REPO_ROOT/src"

RUN_PURE=1
RUN_CORE=0
RUN_HOST=0
RUN_LINUX_RUN=0
RUN_CRI=0

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

Examples:
  scripts/host-integration-smoke.sh
  A3S_BOX_TEST_ALPINE_TAR=/tmp/alpine.tar scripts/host-integration-smoke.sh --core
  sudo -E scripts/host-integration-smoke.sh --linux-run
  A3S_BOX_TEST_ALPINE_TAR=/tmp/alpine.tar scripts/host-integration-smoke.sh --all
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

run() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    "$@"
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
    [ -x "$WORKSPACE/target/$target/debug/a3s-box-guest-init" ] ||
        [ -x "$WORKSPACE/target/$target/release/a3s-box-guest-init" ] ||
        [ -x "$WORKSPACE/target/debug/a3s-box-guest-init" ] ||
        [ -x "$WORKSPACE/target/release/a3s-box-guest-init" ]
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
Install cargo-zigbuild and rerun, or build it manually:
  cargo install cargo-zigbuild
  rustup target add $target
  cargo zigbuild -p a3s-box-guest-init --target $target
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

cd "$WORKSPACE"

case "$(host_os)" in
    Darwin|Linux)
        ;;
    *)
        echo "This runner targets macOS and Linux. Detected: $(host_os)" >&2
        exit 1
        ;;
esac

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

log "Host integration runner completed"
