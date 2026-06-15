# Real-microVM CI gate (self-hosted KVM runner)

The hosted CI jobs (`fmt`, `clippy`, `test`, `build-check`, `build-windows`) all
run in **stub mode**: `A3S_DEPS_STUB=1` links a one-line `void krun_stub(){}`
shared object instead of real libkrun, so **no microVM is ever booted**. That is
deliberate — GitHub-hosted runners have no `/dev/kvm`, so they *cannot* boot a
microVM — but it means a runtime regression (boot, `exec`, virtio-fs, the CRI
pod/container lifecycle, snapshot-fork) compiles green and ships uncaught. Real
runtime correctness has historically ridden on out-of-band manual KVM runs.

The `integration-kvm` job in [`ci.yml`](../.github/workflows/ci.yml) closes that
gap: it links **real** libkrun and runs the `#[ignore]` integration suite plus
the `crictl` CRI smoke test against an actual microVM. It is **inert until you
arm it**, so it never blocks a PR on a repo without a KVM runner.

## Arming the gate (one-time)

### 1. Register a self-hosted runner that has `/dev/kvm`

On a Linux host with `/dev/kvm` (a bare-metal box or a VM with nested
virtualization), follow *Settings → Actions → Runners → New self-hosted runner*
and register it with these **labels**:

```
self-hosted, linux, kvm
```

The host needs: `/dev/kvm` accessible to the runner user, a Rust toolchain
(`rustup` with the `x86_64-unknown-linux-musl` target), `protobuf-compiler`,
`musl-tools`, a C toolchain (for the libkrun build), and — for the CRI smoke —
`crictl` on `PATH` and outbound network (or a registry mirror, see below).

### 2. Flip the repository variable

*Settings → Secrets and variables → Actions → Variables → New repository
variable*:

| Variable | Value | Purpose |
|----------|-------|---------|
| `KVM_CI` | `true` | **Required.** Activates the `integration-kvm` job. |
| `KVM_CI_AGENT_IMAGE` | e.g. `docker.m.daocloud.io/library/alpine:latest` | Sandbox agent image for the CRI smoke (optional; sane default). |
| `KVM_CI_REGISTRY_MIRRORS` | e.g. `registry.k8s.io=k8s.m.daocloud.io,gcr.io=gcr.m.daocloud.io` | Registry mirrors for restricted-egress hosts (optional). |

Once `KVM_CI=true` and a runner with the `kvm` label is online, every push to
`main`, every PR, every `v*` tag, and manual `workflow_dispatch` runs the real
microVM gate after the cheap `fmt`/`clippy`/`test` jobs pass.

## What it runs

1. Verifies `/dev/kvm` is present (fails loudly otherwise).
2. Builds the real binaries (`unset A3S_DEPS_STUB`) — `a3s-box`, `a3s-box-cri`,
   `a3s-box-shim`, plus the static musl `a3s-box-guest-init`.
3. `core_smoke` — boots a real microVM and execs over virtio-fs.
4. `crictl_smoke` (`A3S_BOX_CRI_SMOKE=1`) — the full CRI pod/container lifecycle
   (`RunPodSandbox → CreateContainer → StartContainer → exec → Stop → Remove`)
   driven by real `crictl`.

For the deeper `critest` conformance suite (73/7/17) and the snapshot-fork /
warm-pool benchmarks, see [`cri-conformance.md`](./cri-conformance.md) and the
`bench/` harness.

## Why a self-hosted runner (and not a hosted one)

Standard GitHub-hosted runners do not expose `/dev/kvm`; nested virtualization
is unavailable, so libkrun cannot create a VM. A self-hosted runner on a host
with KVM (the same kind of box used for manual conformance runs) is the only way
to exercise the real runtime in CI.
