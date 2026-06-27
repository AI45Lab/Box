#!/usr/bin/env bash
#
# install-runtimeclass.sh — provision a Kubernetes node to run RuntimeClass=a3s-box pods.
#
# Run as root ON each node that should host a3s-box MicroVM workloads. It installs:
#   * the a3s-box CLI + helpers (a3s-box, a3s-box-cri, a3s-box-guest-init, a3s-box-shim)
#   * libkrun / libkrunfw (the MicroVM VMM, into the system lib dir + ldconfig)
#   * the containerd runtime-v2 shim (containerd-shim-a3s-box-v2)
# and registers the `io.containerd.a3s-box.v2` runtime with containerd, then restarts it.
#
# After running this on a node, label it from a control-plane so the RuntimeClass
# nodeSelector (a3s-box.io/runtime=true) lets a3s-box pods schedule there:
#
#     kubectl label node <node-name> a3s-box.io/runtime=true
#
# Usage:
#   install-runtimeclass.sh [--version vX.Y.Z] [--repo OWNER/REPO] [--from-dir DIR]
#
#   --version   release tag to install                 (default: v2.6.0)
#   --repo      GitHub repo to download artifacts from (default: AI45Lab/Box)
#   --from-dir  install from a local directory instead of downloading; the dir must
#               contain a3s-box-<version>-linux-<arch>.tar.gz and a containerd shim
#               binary (containerd-shim-a3s-box-v2[-linux-<arch>]).
#
# Idempotent: safe to re-run (re-installs binaries, rewrites the containerd drop-in).
set -euo pipefail

VERSION="v2.6.0"
REPO="AI45Lab/Box"
FROM_DIR=""
WARMUP_IMAGE="busybox:latest"   # first box on a fresh node builds a one-time cache
                                # (~40s+); booting one here primes it so the first
                                # real pod doesn't exceed the shim's boot poll. Best
                                # effort — skipped silently if the image can't pull.

while [ $# -gt 0 ]; do
  case "$1" in
    --version)      VERSION="$2"; shift 2 ;;
    --repo)         REPO="$2";    shift 2 ;;
    --from-dir)     FROM_DIR="$2"; shift 2 ;;
    --warmup-image) WARMUP_IMAGE="$2"; shift 2 ;;
    --no-warmup)    WARMUP_IMAGE=""; shift ;;
    -h|--help)  sed -n '2,33p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# ── preflight ───────────────────────────────────────────────────────────────
[ "$(id -u)" = 0 ] || die "must run as root"
command -v containerd >/dev/null || die "containerd not found on this node"
[ -e /dev/kvm ] || die "/dev/kvm missing — this node has no KVM virtualization; a3s-box cannot run here"

case "$(uname -m)" in
  x86_64)        ARCH=x86_64 ;;
  aarch64|arm64) ARCH=arm64  ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

TARBALL="a3s-box-${VERSION}-linux-${ARCH}.tar.gz"
SHIM_ASSET="containerd-shim-a3s-box-v2-linux-${ARCH}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# ── obtain artifacts ────────────────────────────────────────────────────────
if [ -n "$FROM_DIR" ]; then
  log "Installing from local dir: $FROM_DIR"
  [ -f "$FROM_DIR/$TARBALL" ] || die "missing $TARBALL in $FROM_DIR"
  cp "$FROM_DIR/$TARBALL" "$work/$TARBALL"
  if   [ -f "$FROM_DIR/$SHIM_ASSET" ];                 then cp "$FROM_DIR/$SHIM_ASSET" "$work/shim"
  elif [ -f "$FROM_DIR/containerd-shim-a3s-box-v2" ];  then cp "$FROM_DIR/containerd-shim-a3s-box-v2" "$work/shim"
  else die "missing containerd-shim-a3s-box-v2 in $FROM_DIR"; fi
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
  log "Downloading $TARBALL"
  curl -fsSL "$base/$TARBALL" -o "$work/$TARBALL" || die "download failed: $base/$TARBALL"
  log "Downloading $SHIM_ASSET"
  curl -fsSL "$base/$SHIM_ASSET" -o "$work/shim" || die "download failed: $base/$SHIM_ASSET"
fi

tar xzf "$work/$TARBALL" -C "$work"
src="$work/a3s-box-${VERSION}-linux-${ARCH}"
[ -d "$src" ] || die "unexpected tarball layout (no $src)"

# ── install binaries + libkrun ──────────────────────────────────────────────
log "Installing a3s-box binaries to /usr/local/bin"
install -m0755 "$src/a3s-box" "$src/a3s-box-cri" "$src/a3s-box-guest-init" "$src/a3s-box-shim" /usr/local/bin/

log "Installing libkrun to /usr/lib + ldconfig"
cp -a "$src"/lib/libkrun* /usr/lib/
ldconfig

log "Installing containerd-shim-a3s-box-v2 (/usr/local/bin + /opt/containerd/bin)"
install -m0755 "$work/shim" /usr/local/bin/containerd-shim-a3s-box-v2
install -d /opt/containerd/bin
install -m0755 "$work/shim" /opt/containerd/bin/containerd-shim-a3s-box-v2

install -d /var/lib/a3s-box   # shared A3S_HOME for the shim's a3s-box invocations

# ── register the runtime with containerd ────────────────────────────────────
cfg=/etc/containerd/config.toml
runtime_block() {
  cat <<'TOML'
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.a3s-box]
  runtime_type = 'io.containerd.a3s-box.v2'
  [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.a3s-box.options]
TOML
}

if grep -qE "^imports[[:space:]]*=.*conf\.d" "$cfg" 2>/dev/null; then
  # containerd merges /etc/containerd/conf.d/*.toml — register via a drop-in so we
  # never touch the main config (clean + idempotent).
  install -d /etc/containerd/conf.d
  { echo "version = 3"; echo; runtime_block; } > /etc/containerd/conf.d/a3s-box.toml
  log "Registered runtime via /etc/containerd/conf.d/a3s-box.toml"
elif grep -q "runtimes.a3s-box\]" "$cfg" 2>/dev/null; then
  log "Runtime already present in $cfg — leaving as-is"
else
  # No conf.d imports: append the runtime table to the main config.
  { echo; runtime_block; } >> "$cfg"
  log "Registered runtime in $cfg"
fi

log "Restarting containerd"
systemctl restart containerd
sleep 2

# ── verify ──────────────────────────────────────────────────────────────────
log "Verification"
systemctl is-active --quiet containerd || die "containerd is not active after restart"
"$src/a3s-box" --version >/dev/null 2>&1 || /usr/local/bin/a3s-box --version >/dev/null 2>&1 || die "a3s-box CLI not runnable"
echo "  a3s-box:   $(/usr/local/bin/a3s-box --version 2>/dev/null)"
echo "  libkrun:   $(ldconfig -p | awk '/libkrun\.so/{print $1; exit}')"
echo "  shim:      $(command -v containerd-shim-a3s-box-v2)"
echo "  /dev/kvm:  present"
echo "  containerd: active"

# ── warm up (prime the one-time per-node boot cache) ────────────────────────
if [ -n "$WARMUP_IMAGE" ]; then
  log "Warming up with $WARMUP_IMAGE (primes first-boot cache; --no-warmup to skip)"
  if A3S_HOME=/var/lib/a3s-box timeout 240 /usr/local/bin/a3s-box run \
        --name a3sbox-warmup "$WARMUP_IMAGE" -- true >/dev/null 2>&1; then
    echo "  warm-up OK — first pod will boot fast"
  else
    echo "  warm-up skipped (could not pull $WARMUP_IMAGE) — first pod may cold-start slowly"
  fi
  A3S_HOME=/var/lib/a3s-box /usr/local/bin/a3s-box rm -f a3sbox-warmup >/dev/null 2>&1 || true
fi

printf '\n\033[1;32mDone.\033[0m a3s-box runtime installed on %s.\n' "$(hostname)"
cat <<EOF
Final step — from a control-plane node, label this node so a3s-box pods can schedule:

    kubectl label node $(hostname) a3s-box.io/runtime=true

EOF
