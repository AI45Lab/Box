# A3S Box

<p align="center">
  <strong>A kernel per workload — at container speed.</strong>
</p>

<p align="center">
  <em>A Docker-like runtime that runs each Linux OCI workload inside its own libkrun MicroVM. VM-grade isolation — a real kernel per box, with optional hardware TEE — brought to container-class startup and density by native Copy-on-Write snapshot-fork.</em>
</p>

---

## Why A3S Box

**The tradeoff every runtime forces on you.** Containers are fast and dense, but they share the host kernel — one kernel bug or escape crosses every tenant on the box. Virtual machines isolate with their own kernel, but they are slow and heavy to start and to scale. You pick *speed* or *isolation*.

**A3S Box collapses that tradeoff.** Every box is a real MicroVM with its own Linux kernel — yet native Copy-on-Write **snapshot-fork** clones a *booted* template instead of cold-booting each one, so a VM starts and scales like a container. Strong isolation stops being a thing you pay for in latency and footprint.

Measured on a `/dev/kvm` host (not aspirational):

| | A3S Box | Why it matters |
| --- | --- | --- |
| **Isolation** | A real Linux kernel per workload, optional AMD SEV-SNP confidential computing | A guest kernel bug stays in the guest — unlike a shared-kernel container escape |
| **Cold start** | ~200 ms | Already VM-fast, before forking |
| **Snapshot-fork** | ~110 ms per fork · 100 forks in **under ~1 s** (~8 ms amortized) · ~13 MB RSS each | VM density and startup at *container* scale |
| **Warm pool** | a pre-booted box served in ~73 ms (~23× vs cold), CoW-filled | Sub-100 ms acquire for bursty/agent workloads |
| **Developer surface** | `run` / `build` / `exec` / `logs` / `compose`, OCI images, Kubernetes CRI, a Rust SDK (programmable CI pipelines) | No new mental model — your Docker workflow, unchanged |

In one line: **the isolation of a VM, the startup and density of a container, the ergonomics of Docker.** That is the core of A3S Box. Everything below is an honest account of how far each surface is actually built.

## Current status

A3S Box is built toward production use, but it is not a full Docker, containerd, or Kubernetes replacement yet. The local CLI runtime is the primary product surface. Kubernetes CRI, hardware TEE, and Windows support exist in code paths but should be treated as integration surfaces that need host-specific validation before production use.

As of **v2.4.0**, three adversarial audits — production-operability (24 findings), untrusted-input security (4, including a critical registry-digest path-traversal), and concurrency/atomicity (4) — have been closed, every fix verified on real microVMs. The merged tree is validated end-to-end: a composed-main CI integration run on a real `/dev/kvm` host, a **2-hour / 4584-operation endurance soak with zero resource leak**, and complex **stateful** workloads (named-volume persistence across stop/start and restart, a stateful database surviving a restart, and a web server). Net: the local CLI runtime is suitable for **controlled production** with trusted-to-semi-trusted workloads; adversarial multi-tenant deployment at large scale still benefits from independent scale testing and an external security review.

| Area | Status today |
| --- | --- |
| Local CLI runtime | Implemented for macOS Apple Silicon/HVF and Linux/KVM style hosts. Real macOS HVF core smoke has passed with an offline Alpine OCI archive. |
| OCI images | Pull, load, save, tag, inspect, history, remove, and local cache resolution are implemented. Push and cosign signing/verification paths exist and require registry access for end-to-end validation. |
| Dockerfile build | Honest subset. `FROM`, metadata instructions, `COPY`/`ADD`, and shell-form `RUN` are implemented. `RUN` is isolated with Linux `chroot` and requires root-capable Linux; macOS fails by default unless explicitly unsafe host execution is enabled. |
| Lifecycle and exec | `run`, `create`, `start`, `stop`, `restart`, `rm`, `wait`, foreground/detached runs, non-PTY exec, PTY exec, logs, stats, and inspect are implemented. |
| Warm pool and snapshot-fork | A warm pool serves pre-booted sandboxes over a socket. Native snapshot-fork (Copy-on-Write microVM cloning) snapshots one booted template and restores many forks from it, each mapping the template RAM `MAP_PRIVATE`. Verified on `/dev/kvm`: ~4× faster than a cold boot per fork, 100 forks in under ~1 s (~8 ms amortized each). Requires `/dev/kvm`; opt in with `pool start --snapshot-fork` or the `KRUN_SNAPSHOT_*` / `KRUN_RESTORE_FROM` env. |
| Networking | Default TSI networking, TCP `host:guest` publishing, user-defined bridge networks, network inspect/connect/disconnect/rm, and `/etc/hosts` peer discovery are implemented with documented platform boundaries. |
| Compose | A useful local subset is implemented: image, command, entrypoint, env, env_file, ports, volumes, depends_on, networks, DNS, tmpfs, workdir, hostname, extra_hosts, labels, healthcheck, restart, CPU/memory, capabilities, and privileged mode. |
| TEE | AMD SEV-SNP-oriented attestation, RA-TLS, sealing, and secret injection flows exist, plus simulation mode for development. Hardware-backed operation depends on SEV-SNP-capable hosts and libkrun support. TDX is not a productized path. |
| Kubernetes CRI | Reachable by `crictl`/kubelet over its Unix socket. Verified on a `/dev/kvm` host: pod + container lifecycle (`RunPodSandbox` → `CreateContainer` → `StartContainer` → `Stop`/`Remove`), `exec` over Kubernetes SPDY/3.1 `remotecommand` (TTY and non-TTY, stdin/stdout/stderr, exit codes), and container log capture to `log_path`. Not yet conformant: `attach` and the stricter `critest` specs (log format, Linux SecurityContext, seccomp/AppArmor, namespaces, mount propagation). Linux-only; not the core completion target. |
| Windows | Native WHPX backend through libkrun. The Windows package runs directly on Windows with Windows Hypervisor Platform enabled; it does not require WSL. Windows CRI is intentionally out of scope. |

## What A3S Box is

A3S Box is a **MicroVM runtime**. It takes a Linux OCI image, prepares a root filesystem, boots a small VM with libkrun, and runs the image process under guest-init. It is designed for stronger isolation than a namespace-only container while keeping a Docker-like developer workflow.

A3S Box is not:

- a full Docker daemon;
- a general-purpose Kubernetes runtime with all CRI edge cases completed;
- a full Dockerfile/buildx implementation;
- a network policy engine yet;
- a TEE guarantee on hardware that cannot produce and verify real attestation evidence.

## Verified core behavior

The ignored `core_smoke` suite covers the core CLI path on a real MicroVM host:

- pull/load image into an isolated `A3S_HOME`;
- detached and foreground `run`;
- non-TTY `exec`, PTY, `attach`, `logs`, `stop`, `wait`, and `rm`;
- TCP published ports with host loopback HTTP reachability;
- bridge network endpoint allocation, peer `/etc/hosts`, connect/disconnect, and force removal cleanup;
- named volumes, `cp`, `diff`, `export`, `commit`, `snapshot`, restart-policy monitor recovery, and Compose health/volume flow;
- warm pool (`pool start`/`pool run`): pre-warmed sandboxes served over a socket, with backpressure and multi-image lazy pools; `--deferred` runs each command as the box's real main for full box semantics (real exit code + json-file console logs) with no cold boot; `--snapshot-fork` fills the pool by Copy-on-Write restore from one booted template instead of cold booting each sandbox.

The most recent local record: all 14 ignored `core_smoke` tests passed on macOS
HVF with an offline Alpine OCI archive, and the ignored `host_smoke` VM command
matrix plus Compose smoke passed with the same archive.

For **v2.4.0**, the merged tree was additionally validated on a real Linux
`/dev/kvm` host: the composed-main CI integration suite passed; a **2-hour
endurance soak of 4584 real-microVM operations** (high-frequency
create/run/remove churn plus a full run → exec → snapshot → stop → rm lifecycle
every tenth op) finished with **zero leak** — orphan shims, overlay mounts, box
directories, and disk all returned to baseline; and complex **stateful**
containers passed: a named volume's data survived stop/start, a Redis instance's
key survived a `restart` (`SET` → `SAVE` → `restart` → `GET`), and an nginx box
served HTTP.

## Install

```bash
# macOS / Linux via Homebrew tap
brew install a3s-lab/tap/a3s-box

# From source
git clone https://github.com/AI45Lab/Box.git
cd Box/src
cargo build --release
```

On macOS, use Apple Silicon. On Linux, use a host with KVM/libkrun support. On Windows, enable Windows Hypervisor Platform for the native WHPX backend:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
```

Run `a3s-box info` first; it reports virtualization, platform, bridge backend, port-publishing support, and TEE availability.

## Quick start

```bash
# Run a command in a MicroVM
a3s-box run --name hello alpine:latest -- echo "hello from a3s-box"

# Interactive shell
a3s-box run -it --name dev alpine:latest -- /bin/sh

# Detached service with resources and a published TCP port
a3s-box run -d --name web --cpus 2 --memory 1g -p 8080:80 nginx:alpine

# Inspect, exec, logs, and stop
a3s-box ps
a3s-box exec web -- nginx -v
a3s-box logs -f web
a3s-box stop web
a3s-box rm web
```

## Command surface

A3S Box exposes 56 top-level commands. They are Docker-like, not Docker-identical.

| Category | Commands |
| --- | --- |
| Lifecycle | `run`, `create`, `start`, `stop`, `restart`, `rm`, `kill`, `pause`, `unpause`, `wait`, `rename`, `prune` |
| Execution | `exec`, `attach`, `top`, `shell` |
| Images | `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `history`, `image-prune`, `save`, `load`, `commit` |
| Filesystem | `cp`, `export`, `diff` |
| Networking | `network`, `port` |
| Volumes | `volume` |
| Snapshots | `snapshot` |
| Compose | `compose` |
| TEE | `attest`, `seal`, `unseal`, `inject-secret` |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `audit` |
| System | `system-prune`, `container-update`, `monitor`, `pool`, `login`, `logout`, `version`, `info`, `help` |

Box references accept name, full ID, or unique short ID prefix.

## Lifecycle and execution

```bash
a3s-box run [OPTIONS] IMAGE [-- CMD...]
a3s-box create [OPTIONS] IMAGE [-- CMD...]
a3s-box start BOX [BOX...]
a3s-box stop BOX [BOX...]
a3s-box restart BOX [BOX...]
a3s-box rm [-f] BOX [BOX...]
a3s-box wait BOX [BOX...]
```

Important supported options:

- `--name`, `--label`, `--restart no|always|on-failure[:N]|unless-stopped`;
- `--cpus`, `--memory`, `--timeout`, `--pids-limit`, `--cpuset-cpus`, `--ulimit`, CPU quota/shares, memory reservation/swap;
- `-e/--env`, `--env-file`, `--entrypoint`, `-u/--user`, `-w/--workdir`, `--hostname`, `--add-host`;
- `--health-cmd`, `--health-interval`, `--health-timeout`, `--health-retries`, `--health-start-period`, `--no-healthcheck`;
- `--stop-signal`, `--stop-timeout`, `--persistent`, `--log-driver json-file|none`;
- `--cap-add`, `--cap-drop`, `--security-opt seccomp=default|seccomp=unconfined|no-new-privileges`, `--privileged`.

Unsupported or guarded options fail early instead of being silently stored: host devices, GPUs, AppArmor labels, SELinux labels, custom seccomp profiles, unsupported users, invalid workdirs, unsupported port syntax, and unsupported network policies.

## Images and builds

```bash
a3s-box pull alpine:latest
a3s-box pull --verify-key cosign.pub ghcr.io/org/image:v1
a3s-box images
a3s-box images --filter reference='alpine*' --filter label=tier=web
a3s-box inspect alpine:latest          # polymorphic: resolves a container or an image
a3s-box image-inspect alpine:latest
a3s-box tag alpine:latest local-alpine:dev
a3s-box save -o alpine.tar alpine:latest
a3s-box load -i alpine.tar --tag local-alpine:dev
a3s-box push registry.example/org/image:v1
```

Docker Hub aliases share cache resolution, so `alpine`, `alpine:latest`, and `docker.io/library/alpine:latest` can resolve to the same local image when unambiguous. Digest-only references resolve locally when the digest matches exactly or by unique prefix.

Build support is intentionally explicit:

```bash
a3s-box build -t app:dev .
a3s-box build -t app:dev -f Containerfile .
a3s-box build -t app:dev --build-arg VERSION=1.2.3 --platform linux/amd64 .
a3s-box build -t builder --target builder --no-cache .   # stop at a stage, skip the cache
```

Supported Dockerfile subset: `FROM` including `scratch`, shell-form `RUN`, shell-form `COPY`/`ADD` (incl. `COPY --from=<stage>`, `COPY`/`ADD --chown=user[:group]`), `WORKDIR`, `ENV`, `ENTRYPOINT`, `CMD`, `EXPOSE`, `LABEL`, `USER`, `ARG`, `SHELL`, `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD` metadata triggers, and `VOLUME`. A context-root `.dockerignore` is honored.

Build flags: `-t/--tag`, `-f/--file`, `--build-arg`, `--platform`, `--target <stage>` (build only up to a stage), `--no-cache` (rebuild every layer), `-q/--quiet`.

Boundaries:

- `RUN` uses isolated Linux `chroot`, requires root-capable Linux, validates shell/workdir preconditions, and has a Linux-only ignored smoke test;
- macOS `RUN` fails by default; `A3S_BOX_UNSAFE_HOST_RUN=1` enables unsafe host-side experiments only;
- `--platform` records one target platform; multi-platform image indexes are not implemented.

Builds use a Docker/BuildKit-style **layer cache**: each instruction extends a
rolling chain key (its text plus, for `COPY`/`ADD`, the content hash of the
source files), and a layer-producing step whose chain key was seen before is
reused instead of re-run. A changed instruction or input rebuilds that layer
and everything after it. The cache lives at `~/.a3s/buildcache` and is size-capped
(default 2 GiB, override with `A3S_BOX_BUILDCACHE_MAX_BYTES`; oldest blobs evicted first).

## Filesystems, volumes, and snapshots

```bash
a3s-box volume create data
a3s-box run -d --name app -v data:/data alpine:latest -- sleep 3600
a3s-box cp ./file.txt app:/data/file.txt
a3s-box diff app
a3s-box export app -o rootfs.tar
a3s-box commit app -t app:snapshot
a3s-box snapshot create app checkpoint-1
a3s-box snapshot restore checkpoint-1 --name restored-app
a3s-box snapshot prune --keep 5          # bound disk: keep the 5 newest
```

The `snapshot` command produces configuration/filesystem-oriented Box snapshots, not a live RAM checkpoint. The live RAM Copy-on-Write facility is a separate, lower-level mechanism described in [Warm pool and snapshot-fork](#warm-pool-and-snapshot-fork).

`snapshot restore` is **copy-on-write**: the restored box shares the snapshot's rootfs as a read-only overlay lower with its own per-box upper, so forking a warmed snapshot is near-instant, space-cheap (a few MB per fork), and isolated — this is what the [SDK](#sdk) pipeline API forks per step. (On a non-overlay host it falls back to a full copy.) `snapshot create` still deep-copies the box rootfs into the store, so a scheduled snapshot workflow can fill the disk: `snapshot prune --keep N` / `--max-bytes B` evicts the oldest beyond a cap, and `A3S_BOX_MAX_SNAPSHOTS` / `A3S_BOX_MAX_SNAPSHOT_BYTES` auto-prune on every `create` (unset = unbounded). Because a restored box keeps referencing its snapshot, `snapshot rm` / `prune` refuse to delete a snapshot still in use by a box (`--force` overrides).

## SDK

`a3s-box-sdk` is the Rust SDK for A3S Box, published to crates.io. Today it provides a **programmable CI/CD pipeline** API (`a3s_box_sdk::pipeline`): a pipeline is a Rust program and each step runs in its **own MicroVM** (one kernel per step), forking a warmed snapshot via copy-on-write `snapshot restore`. It is a dependency-free wrapper over the `a3s-box` CLI — the DAG is your code, not YAML.

```rust
use a3s_box_sdk::pipeline::{warm_base, WarmBase, FileCache, Step};

let cache = FileCache::new(".ci-cache")?;             // skip a step when inputs are unchanged
let mut base = warm_base(                              // clone + install deps ONCE, then snapshot
    WarmBase::new("node:20", "git clone $REPO /w && cd /w && npm ci").cache(&cache),
)?;
base.step(Step::new("test", "cd /w && npm test"))?;   // nonzero exit -> Err (fail-fast)
base.step(Step::new("build", "cd /w && npm run build"))?;
base.dispose();
```

The former MicroVM workload-execution SDK (`ExecutionRegistry`/`VmExecutor`, for embedding Box into higher-level runtimes such as a3s-lambda) is now the **`a3s-box-lambda`** crate.

## Warm pool and snapshot-fork

A **warm pool** keeps a set of sandboxes pre-booted and serves them over a Unix
socket, so a request is answered by an already-running microVM instead of a cold
boot. It supports backpressure, multi-image lazy pools, and a `--deferred` mode
that runs each request as the box's real main process (real exit code +
json-file console logs).

```bash
a3s-box pool start --image alpine:latest --size 8     # pre-warm 8 sandboxes
a3s-box pool start --image alpine:latest --size 8 --snapshot-fork   # CoW fill
a3s-box pool start --image alpine:latest --metrics-addr 127.0.0.1:9101   # + Prometheus /metrics
a3s-box pool run alpine:latest -- echo hi             # served from the pool
a3s-box pool status
a3s-box pool stop
```

`pool start --metrics-addr` serves a Prometheus `/metrics` endpoint with warm-pool hit/miss, VM-boot, and cache metrics for the long-running daemon (alongside `monitor --metrics-addr`'s box-state metrics + `/healthz`).

**Snapshot-fork** (`--snapshot-fork`, Linux `/dev/kvm` only) is native
Copy-on-Write microVM cloning. The pool cold-boots one template sandbox,
snapshots its file-backed guest RAM together with KVM vCPU and virtio device
state, and then restores the rest of the pool from that snapshot. Each fork maps
the template RAM `MAP_PRIVATE`, so it pays only for the pages it dirties. On a
`/dev/kvm` host this is ~4× faster than a cold boot per fork, completes 100
forks in under ~1 s (~8 ms amortized each, ~13 MB RSS per fork), and `exec`
runs real commands over virtio-fs inside the restored guest. It is off by
default.

The same mechanism is available below the pool through environment variables:
`KRUN_SNAPSHOT_MEM_FILE` and `KRUN_SNAPSHOT_SOCK` capture a snapshot from a
booted template, and `KRUN_RESTORE_FROM` restores a fork from it. Per-VM
`BoxConfig`/`InstanceSpec` fields (`snapshot_mem_file`, `snapshot_sock`,
`restore_from`) take precedence over the env when set.

## Pruning stopped boxes

```bash
a3s-box prune --force            # remove every created/stopped/dead box
a3s-box container-prune --force  # alias
```

`prune` is the box-only counterpart to `system-prune` (which also removes images
and networks). Running and paused boxes are never touched.

## Networking

A3S Box has three network modes:

| Mode | What it does | Current boundary |
| --- | --- | --- |
| TSI default | Guest socket operations are proxied through the host. Use this for simple outbound access. | No user-defined peer network, and **no in-guest loopback** — a container cannot reach its own services over `localhost`/`127.0.0.1` (e.g. a `localhost` health check or `exec curl localhost` fails). Use a bridge network when you need working localhost. |
| Bridge | Creates a real guest network interface for user-defined networks and peer discovery. | Linux uses `passt` with outbound NAT. macOS uses built-in `netproxy` for peer networking and published TCP ports; macOS bridge outbound NAT is unsupported. |
| None | No network. | Useful for intentionally isolated workloads. |

```bash
a3s-box network create backend --subnet 10.89.0.0/24
a3s-box run -d --name api --network backend -p 8080:80 myapi:latest
a3s-box network inspect backend
a3s-box network connect backend stopped-box
a3s-box network disconnect backend stopped-box
a3s-box network rm --force backend
a3s-box network prune --force   # remove all networks not used by any box
a3s-box port api
```

Published ports support TCP only in `host_port:guest_port[/tcp]` form. UDP, host-IP binds such as `127.0.0.1:8080:80`, single-port shorthand, and ranges are rejected during CLI or Compose validation. `network connect` and `network disconnect` apply to inactive boxes; live hot-plug is not implemented. Strict/custom network policy modes are rejected until packet filtering is implemented.

## Compose subset

```bash
a3s-box compose -f compose.yaml config
a3s-box compose -f compose.yaml up -d
a3s-box compose -f compose.yaml ps
a3s-box compose -f compose.yaml logs -f
a3s-box compose -f compose.yaml down
```

Supported Compose keys: `image`, `command`, `entrypoint`, `environment`, `env_file`, `ports`, `volumes`, `depends_on` with `service_started` or `service_healthy`, `networks`, `dns`, `tmpfs`, `working_dir`, `hostname`, `extra_hosts`, `labels`, `healthcheck`, `restart`, `cpus`, `mem_limit`, `cap_add`, `cap_drop`, and `privileged`.

## TEE workflows

```bash
# Hardware path: requires SEV-SNP-capable Linux host and libkrun support
a3s-box run -d --name secure --tee myimage:latest -- sleep 3600

# Development path: simulated reports and secrets flow
a3s-box run -d --name dev --tee --tee-simulate myimage:latest -- sleep 3600
a3s-box attest dev --ratls --allow-simulated
a3s-box inject-secret dev --secret API_KEY=secret --set-env --allow-simulated
a3s-box seal dev --data "value" --context app/key --policy measurement-and-chip
a3s-box unseal dev --context app/key
```

TEE features include SNP report parsing/verification, RA-TLS certificate extensions, AES-256-GCM sealing with HKDF-SHA256, and RA-TLS secret injection. Treat simulation as a developer workflow only; it does not prove hardware isolation. TDX is not productized.

## Kubernetes CRI

The CRI server is reachable by standard gRPC clients — `crictl`, the kubelet, and `critest` — over its Unix domain socket, and runs the core pod + container lifecycle and `exec` end to end. It is Linux-only and not yet fully `critest`-conformant.

Verified on a `/dev/kvm` host via `crictl`:

- CRI v1 RuntimeService/ImageService over the Unix socket. A vendored `h2` patch (`third_party/h2`, wired via `[patch.crates-io]`) relaxes the percent-encoded socket-path `:authority` that `grpc-go >= 1.57` sends, which upstream `h2` otherwise rejects with `PROTOCOL_ERROR` before any RPC runs.
- Pod sandbox + container lifecycle: `runp` → `create` → `start` → `ps` → `stop` → `rm` → `stopp` → `rmp`.
- `exec` over the Kubernetes SPDY/3.1 `remotecommand` protocol — `kubectl exec` / `crictl exec`, TTY and non-TTY, stdin/stdout/stderr, and exit-code propagation.
- Container stdout/stderr captured to the CRI `log_path` and readable via `crictl logs`.
- RuntimeClass image overrides.

Not yet complete: `attach`, and the stricter `critest` conformance specs (log format, Linux SecurityContext, seccomp/AppArmor, namespace sharing, mount propagation). Track conformance in `docs/cri-conformance.md`.

For an explicit cluster evaluation:

```bash
helm install a3s-box deploy/helm/a3s-box/ -n a3s-box-system --create-namespace
```

Windows CRI is intentionally unsupported.

## Architecture

```text
Host
  a3s-box CLI
    state: boxes, images, volumes, networks, audit log under A3S_HOME
    runtime: image store, rootfs builder, VmManager, network backend, TEE client
      |
      | shim process + libkrun
      v
Guest MicroVM
  guest-init (PID 1)
    exec server 4089
    PTY server 4090
    attestation server 4091
    user workload process
```

Vsock/control services:

| Port | Service |
| ---: | --- |
| 4088 | gRPC control / health (guest↔host) |
| 4089 | exec server |
| 4090 | PTY server |
| 4091 | attestation / RA-TLS |
| 4092 | optional sidecar vsock port |

These are **vsock** ports (guest↔host), not host TCP endpoints. For
host-scrapable Prometheus metrics + a health probe, run the monitor with
`a3s-box monitor --metrics-addr 127.0.0.1:9100` (serves `/metrics` and
`/healthz`) — see [`docs/monitor-service.md`](docs/monitor-service.md).

Crates:

| Crate | Purpose |
| --- | --- |
| `core` | Shared config, errors, events, port/network/volume/PTY/DNS/workload types |
| `runtime` | VM lifecycle, image store, rootfs preparation, Compose, networking, TEE clients |
| `cli` | `a3s-box` command line |
| `shim` | libkrun bridge subprocess |
| `guest/init` | guest PID 1 and guest services |
| `netproxy` | macOS user-space bridge proxy and published TCP forwarding |
| `cri` | experimental CRI server |
| `sdk` | Rust execution registry abstractions for Box workloads |

## Development and validation

Run checks from `crates/box/src`, not the monorepo root.

```bash
cd crates/box/src
cargo fmt --all
cargo test -p a3s-box-runtime --lib --quiet
cargo test -p a3s-box-cli --test command_coverage --quiet
cargo test -p a3s-box-cli --test host_smoke --quiet
cargo test -p a3s-box-cli --test core_smoke --quiet
```

Or run the macOS/Linux validation ladder from `crates/box`:

```bash
cd crates/box
scripts/host-integration-smoke.sh
```

Opt-in real runtime smoke:

```bash
cd crates/box
A3S_BOX_SMOKE_IMAGE_TAR=/path/to/alpine.tar \
A3S_BOX_SMOKE_TIMEOUT_SECS=300 \
scripts/host-integration-smoke.sh --core
```

Opt-in Linux Dockerfile `RUN` smoke:

```bash
cd crates/box
A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine.tar \
sudo -E scripts/host-integration-smoke.sh --linux-run --no-pure
```

The Linux `RUN` smoke must run as root on a root-capable Linux builder.
See `docs/host-integration.md` for the macOS HVF, Linux KVM, host command
matrix, and CRI smoke procedures.

## Environment variables

| Variable | Description |
| --- | --- |
| `A3S_HOME` | Data directory. Default: `~/.a3s`. |
| `A3S_IMAGE_CACHE_SIZE` | Image cache size. Default: `10g`. |
| `A3S_TEE_SIMULATE` | Enables simulated TEE report behavior. |
| `A3S_REGISTRY_PROTOCOL` | Registry protocol override for local/insecure registry tests. |
| `A3S_BOX_CRI_AGENT_IMAGE` | Default CRI sandbox agent/rootfs image. |
| `A3S_BOX_SMOKE_IMAGE_TAR` | OCI archive used by the ignored core MicroVM smoke suite. |
| `A3S_BOX_TEST_ALPINE_TAR` | Shared offline Alpine OCI archive for core and host smoke suites. |
| `A3S_BOX_ALLOW_REGISTRY_PULL` | Set to `1` to let the host integration runner use live registry pulls when no OCI archive is provided. |
| `A3S_BOX_HOST_SMOKE_TIMEOUT_SECS` | Boot timeout override for ignored host smoke tests. |
| `A3S_BOX_UNSAFE_HOST_RUN` | Opt into unsafe macOS host execution for Dockerfile `RUN` experiments. |
| `A3S_BOX_BUILDCACHE_MAX_BYTES` | Cap on the total size of cached build layers at `~/.a3s/buildcache` (oldest evicted first). Default: 2 GiB. |
| `A3S_BOX_MAX_LAYER_BYTES` | Cap on total decompressed bytes per OCI image layer during `pull` (decompression-bomb guard). Default: 16 GiB. |
| `A3S_BOX_MAX_BUILD_EXTRACT_BYTES` | Cap on total decompressed bytes when a build `ADD`/`COPY` auto-extracts a local tar archive (decompression-bomb guard). Default: 4 GiB. |
| `A3S_BOX_MAX_SNAPSHOTS` | Auto-prune on every `snapshot create` to keep at most N newest snapshots per box (unset = unbounded). |
| `A3S_BOX_MAX_SNAPSHOT_BYTES` | Auto-prune on every `snapshot create` to keep snapshots under a total byte cap (unset = unbounded). |
| `A3S_BOX_SECCOMP_PROFILE_ROOT` | Root directory a CRI `localhostProfile` seccomp path is confined to (paths outside it, or containing `..`, are rejected). Default: `/var/lib/kubelet/seccomp`. |
| `A3S_REGISTRY_MIRRORS` | Registry mirror map (`host=mirror,host=mirror`); pulls fetch layers/manifests from the mirror while keeping the canonical image reference. |
| `KRUN_SNAPSHOT_MEM_FILE` | Path the booted template writes its file-backed guest RAM to when capturing a snapshot-fork template. |
| `KRUN_SNAPSHOT_SOCK` | Control socket the template listens on for the `snapshot <path>` command (Linux `/dev/kvm` only). |
| `KRUN_RESTORE_FROM` | Path to a snapshot the microVM restores from as a Copy-on-Write fork instead of cold booting. |
| `RUST_LOG` | Rust tracing log level. |

## License

MIT
