# Host Integration Smoke Guide

This guide defines the macOS and Linux validation path for a3s-box. Run these
commands from the crate repository root (`crates/box`), not from the monorepo
root.

## Validation ladder

| Level | Host requirements | Command |
| --- | --- | --- |
| Stub baseline | macOS or Linux with Rust, C compiler, and protoc | `scripts/host-integration-smoke.sh` |
| Core MicroVM smoke | macOS Apple Silicon/HVF or Linux KVM, libkrun, Linux guest init, runnable image | `scripts/host-integration-smoke.sh --core` |
| Host command matrix | Same as core smoke; optional registry credentials for push coverage | `scripts/host-integration-smoke.sh --host` |
| Linux Dockerfile `RUN` | Linux, root, chroot-capable filesystem, local Alpine OCI archive | `sudo -E scripts/host-integration-smoke.sh --linux-run --no-pure` |
| CRI smoke | macOS or Linux MicroVM host, `crictl`, CRI images | `scripts/host-integration-smoke.sh --cri` |

The default command runs formatting, clippy, unit tests, and integration test
compilation with `A3S_DEPS_STUB=1`. It does not require a hypervisor and should
be safe on developer laptops and CI workers. Host-backed `--core` and `--host`
runs require an OCI archive by default; set `A3S_BOX_ALLOW_REGISTRY_PULL=1` only
when you intentionally want live registry pulls.

## macOS core smoke

Use Apple Silicon. Intel macOS is not a supported runtime target.

```bash
cd crates/box

# Optional but recommended for offline/reproducible runs.
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_IMAGE_TAR="$A3S_BOX_TEST_ALPINE_TAR"
export A3S_BOX_SMOKE_SKIP_PULL=1
export A3S_BOX_SMOKE_TIMEOUT_SECS=300

scripts/host-integration-smoke.sh --core
```

If you do not have an offline archive and want to pull from the registry during
the run, add:

```bash
export A3S_BOX_ALLOW_REGISTRY_PULL=1
```

If the Linux guest init binary is missing, install `cargo-zigbuild` and let the
runner build `a3s-box-guest-init` for `aarch64-unknown-linux-musl`:

```bash
cargo install cargo-zigbuild
rustup target add aarch64-unknown-linux-musl
scripts/host-integration-smoke.sh --core
```

## Linux core smoke

Use a host with `/dev/kvm` available to the current user. For offline runs, use
the same OCI archive variables as macOS.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_IMAGE_TAR="$A3S_BOX_TEST_ALPINE_TAR"
export A3S_BOX_SMOKE_SKIP_PULL=1

scripts/host-integration-smoke.sh --core
```

Use `A3S_BOX_ALLOW_REGISTRY_PULL=1` instead of the archive variables only for
network-backed validation; offline archive runs are the release gate default.

If `/dev/kvm` is permission denied, add the user to the `kvm` group and start a
new login session:

```bash
sudo usermod -aG kvm "$USER"
```

## Linux Dockerfile `RUN` smoke

Dockerfile `RUN` uses an isolated Linux chroot path. It is intentionally
Linux-only and requires root. The smoke test must use a local Alpine OCI
archive because it validates the chroot build path, not registry access.

```bash
cd crates/box
sudo -E env A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar \
  scripts/host-integration-smoke.sh --linux-run --no-pure
```

macOS does not run Dockerfile `RUN` by default. The unsafe host execution path
is only for local experiments and requires `A3S_BOX_UNSAFE_HOST_RUN=1`; it is
not part of the product smoke matrix.

## Host command matrix

The host matrix extends the core smoke with VM lifecycle commands, Compose,
copy, stats, snapshots, network operations, image tagging/saving, local build,
and optional registry push coverage.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_HOST_SMOKE_TIMEOUT_SECS=300

scripts/host-integration-smoke.sh --host
```

Enable registry push coverage only against a disposable tag template:

```bash
export A3S_BOX_PUSH_TEST_REF='registry.example/a3s/box-push-test:{tag}'
export A3S_BOX_PUSH_USERNAME='...'
export A3S_BOX_PUSH_PASSWORD='...'
scripts/host-integration-smoke.sh --host
```

## CRI smoke

The CRI smoke is experimental and intentionally opt-in. It starts the
`a3s-box-cri` server, drives it through `crictl`, and launches a pod sandbox
with two containers.

```bash
cd crates/box
export A3S_BOX_CRI_CRICTL=/path/to/crictl
export A3S_BOX_CRI_SMOKE_IMAGE=busybox:latest
export A3S_BOX_CRI_SMOKE_AGENT_IMAGE=ghcr.io/a3s-box/code:v0.1.0

scripts/host-integration-smoke.sh --cri
```

Use `A3S_BOX_CRI_SMOKE_SKIP_PULL=1` and `A3S_BOX_CRI_SMOKE_IMAGE_DIR` when the
image store is preloaded and the run must stay offline.

## Result recording

When a host-backed run passes, record:

- host OS and architecture;
- `a3s-box info` output;
- exact command and environment variables;
- image archive digest or registry image digests;
- test summary line from Cargo.

Keep macOS HVF and Linux KVM records separate because bridge networking and
Dockerfile `RUN` behavior intentionally differ by platform.
