# A3S Box

<p align="center">
  <strong>VM Runtime — Standalone CLI &amp; K8s RuntimeClass</strong>
</p>

<p align="center">
  <em>General-purpose MicroVM runtime for hardware-isolated workloads — Docker-like CLI for standalone use, K8s RuntimeClass for cluster deployment. AMD SEV-SNP TEE when hardware supports, VM isolation always. Application-agnostic: doesn't know what runs inside.</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#cli-usage">CLI Usage</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#tee-confidential-computing">TEE</a> •
  <a href="#testing">Testing</a> •
  <a href="#roadmap">Roadmap</a>
</p>

---

## Overview

**A3S Box** is a general-purpose MicroVM runtime with two deployment modes: a Docker-like CLI (`a3s-box run`) for standalone use, and a K8s RuntimeClass (`a3s-box-shim`) for cluster deployment. Each workload runs in its own MicroVM with a dedicated Linux kernel (~200ms cold start), OCI image support, and optional AMD SEV-SNP hardware memory encryption.

A3S Box is **application-agnostic** — it doesn't know or care what runs inside. Any OCI-packaged process can be sandboxed: web servers, databases, AI agents, or security proxies. When TEE hardware is available (AMD SEV-SNP), workloads get hardware-enforced memory encryption automatically; otherwise they still get VM-level isolation.

## Features

### VM Runtime
- **~200ms Cold Start** — Sub-second MicroVM boot via libkrun (Apple HVF / Linux KVM)
- **OCI Images** — Pull, push, build, tag, inspect, prune from any OCI registry with local LRU cache
- **Dockerfile Build** — Full `a3s-box build` with multi-stage builds and all Dockerfile instructions
- **Warm Pool** — Pre-booted idle MicroVMs for instant allocation (`min_idle` / `max_size` / `idle_ttl`)
- **Rootfs Caching** — Content-addressable cache with SHA256 keys and TTL/size pruning
- **Cross-Platform** — macOS (Apple Silicon) and Linux (x86_64/ARM64), no root required

### Docker-Compatible CLI (47 commands)
- **Lifecycle**: `run`, `create`, `start`, `stop`, `pause`, `unpause`, `restart`, `rm`, `kill`, `rename`
- **Exec & PTY**: `exec` (with `-it`, `-u`, `-e`, `-w`), `attach -it`, `run -it`, `top`
- **Images**: `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `image-prune`, `save`, `load`, `export`, `commit`, `diff`
- **Networking**: `network create/ls/rm/inspect/connect/disconnect`, bridge driver, IPAM, DNS discovery
- **Volumes**: `volume create/ls/rm/inspect/prune`, named volumes, anonymous volumes, tmpfs
- **Observability**: `ps`, `logs`, `inspect`, `stats`, `events`, `cp`
- **System**: `system-prune`, `container-update`, `version`, `info`, `monitor`, `login`, `logout`

### Security & Isolation
- **Namespace Isolation** — Separate mount, PID, IPC, UTS namespaces within each VM
- **Resource Limits** — CPU shares/quota/pinning, memory reservation/swap, PID limits, ulimits (cgroup v2)
- **Security Options** — Capabilities (`--cap-add/drop`), read-only rootfs, privileged mode, device mapping, GPU access
- **Restart Policies** — `always`, `on-failure:N`, `unless-stopped` with exponential backoff
- **Health Checks** — Configurable commands with interval, timeout, retries, start period
- **Logging** — JSON logging driver with rotation, or `--log-driver none`

### TEE (Confidential Computing)
- **AMD SEV-SNP** — Hardware-enforced memory encryption
- **Remote Attestation** — SNP report generation, ECDSA-P384 verification, certificate chain validation (VCEK→ASK→ARK)
- **RA-TLS** — SNP report embedded in X.509 certificate extensions, verified during TLS handshake
- **Secret Injection** — Inject secrets via RA-TLS into `/run/secrets/` (tmpfs, mode 0400)
- **Sealed Storage** — AES-256-GCM with HKDF-SHA256, three policies: MeasurementAndChip, MeasurementOnly, ChipOnly
- **Simulation Mode** — Full TEE workflow on any machine via `A3S_TEE_SIMULATE=1`

### Embedded Sandbox SDK
- **No Daemon** — Create, exec, and stop MicroVM sandboxes directly from Rust code, no CLI or daemon required
- **Simple API** — `BoxSdk::new()` → `sdk.create(options)` → `sandbox.exec("cmd", &["args"])` → `sandbox.stop()`
- **OCI Images** — Specify any OCI image (`alpine:latest`, `python:3.12-slim`, etc.)
- **Configurable** — vCPUs, memory, environment variables, host mounts, working directory, TEE mode
- **PTY Support** — Open interactive terminal sessions via `sandbox.pty()`

### Kubernetes Integration
- **CRI Runtime** — RuntimeService + ImageService for kubelet
- **Deployment** — DaemonSet, RuntimeClass, Kustomize base, RBAC

## Quick Start

### Prerequisites

- **macOS ARM64** (Apple Silicon) or **Linux x86_64/ARM64**
- Rust 1.75+

> macOS Intel is NOT supported.

### Build

```bash
git clone https://github.com/a3s-lab/box.git && cd box
git submodule update --init --recursive
cd src && cargo build --release
```

macOS requires `brew install lld llvm`. Linux requires `apt install build-essential pkg-config libssl-dev`.

| Mode | Command | Use Case |
|------|---------|----------|
| Full Build | `cargo build` | Development with VM support |
| Stub Mode | `A3S_DEPS_STUB=1 cargo build` | CI/testing without VM |

## CLI Usage

```bash
# Run a box
a3s-box run -d --name dev --cpus 2 --memory 1g alpine:latest -- sleep 3600
a3s-box run -it alpine:latest -- /bin/sh          # Interactive shell

# Image management
a3s-box pull alpine:latest
a3s-box build -t myapp:v1 .
a3s-box images
a3s-box push myregistry.io/myapp:v1

# Execute commands
a3s-box exec dev -- ls -la
a3s-box exec -it -u root -e FOO=bar dev -- /bin/sh

# File copy
a3s-box cp ./config.yaml dev:/etc/app/
a3s-box cp dev:/var/log/ ./logs/

# Networking & volumes
a3s-box network create mynet
a3s-box run -d --name web --network mynet -v data:/app/data nginx:alpine
a3s-box volume ls

# Observability
a3s-box ps -a --filter label=env=dev
a3s-box logs dev -f
a3s-box stats
a3s-box events --json

# TEE attestation & secrets
a3s-box run -d --name secure --tee --tee-simulate alpine:latest -- sleep 3600
a3s-box attest secure --ratls --allow-simulated
a3s-box seal secure --data "API_KEY=secret" --context keys --policy measurement-and-chip
a3s-box inject-secret secure --secret "DB_PASS=s3cret" --set-env --allow-simulated

# Lifecycle
a3s-box stop dev && a3s-box rm dev
a3s-box system-prune -f
```

Boxes can be referenced by name, full ID, or unique ID prefix (Docker-compatible resolution).

### Command Reference

| Command | Description |
|---------|-------------|
| `run` | Pull + create + start (`-d`, `--rm`, `-l`, `--restart`, `--health-cmd`, `--cap-add/drop`, `--privileged`, `--read-only`, `--device`, `--gpus`, `--init`, `--env-file`, `--add-host`, `--platform`, `--tee`) |
| `create` | Create without starting (same flags as `run`) |
| `start/stop/restart/kill` | Lifecycle management (multi-target) |
| `pause/unpause` | SIGSTOP/SIGCONT |
| `rm` | Remove boxes (`-f` force) |
| `rename` | Rename a box |
| `exec` | Run command in box (`-it`, `-u`, `-e`, `-w`) |
| `attach` | Attach PTY to running box |
| `top` | Show processes |
| `ps` | List boxes (`-a`, `-q`, `--filter`, `--format`) |
| `logs` | View logs (`-f`, `--tail N`) |
| `inspect` | Detailed JSON info |
| `stats` | Live resource usage |
| `cp` | Copy files/dirs between host and box |
| `diff` | Show filesystem changes (A/C/D) |
| `commit` | Create image from changes (`-m`, `-a`, `-c`) |
| `events` | Stream system events (`--filter`, `--json`) |
| `container-update` | Hot-update resources (`--cpus`, `--memory`, `--restart`) |
| `images` | List cached images |
| `pull/push` | Registry operations |
| `build` | Dockerfile build |
| `rmi` | Remove images |
| `tag` | Create image alias |
| `image-inspect/image-prune` | Image metadata and cleanup |
| `save/load/export` | Image import/export |
| `network` | `create/ls/rm/inspect/connect/disconnect` |
| `volume` | `create/ls/rm/inspect/prune` |
| `system-prune` | Remove stopped boxes + unused images |
| `login/logout` | Registry authentication |
| `attest` | TEE attestation (`--ratls`, `--policy`, `--nonce`, `--raw`, `--quiet`) |
| `seal/unseal` | Sealed storage operations |
| `inject-secret` | Inject secrets via RA-TLS |
| `monitor` | Background restart daemon |
| `version/info/update` | System information |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Host Process                             │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                    a3s-box-runtime                         │  │
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────┐  │  │
│  │  │ VmManager   │ │ OciImage    │ │  RootfsBuilder      │  │  │
│  │  │ (lifecycle) │ │ (registry)  │ │  (composition)      │  │  │
│  │  └─────────────┘ └─────────────┘ └─────────────────────┘  │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │ vsock                             │
└──────────────────────────────┼──────────────────────────────────┘
                               │
┌──────────────────────────────┼──────────────────────────────────┐
│                              ▼                                   │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │              /sbin/init (guest-init, PID 1)               │  │
│  │  - Mount filesystems (/proc, /sys, /dev, virtio-fs)       │  │
│  │  - Exec server (4089), PTY server (4090)                  │  │
│  │  - Attestation server (4091, TEE only)                    │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │                                   │
│  ┌───────────────────────────▼───────────────────────────────┐  │
│  │                 Process (Namespace 1)                      │  │
│  │  - Isolated mount, PID, IPC, UTS namespaces               │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │ /usr/bin/nsexec                   │
│  ┌───────────────────────────▼───────────────────────────────┐  │
│  │               Subprocess (Namespace 2)                     │  │
│  │  - Further isolated from parent process                    │  │
│  └───────────────────────────────────────────────────────────┘  │
│                        Guest VM (MicroVM)                        │
└──────────────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Binary | Purpose | Tests |
|-------|--------|---------|------:|
| `cli` | `a3s-box` | Docker-like CLI | 367 + 7 integration |
| `core` | — | Config, error types, events | 160 |
| `runtime` | — | VM lifecycle, OCI, attestation | 486 |
| `guest/init` | `a3s-box-guest-init` | Guest PID 1, exec/PTY/attestation servers | Linux-only |
| `shim` | `a3s-box-shim` | libkrun bridge | — |
| `cri` | `a3s-box-cri` | Kubernetes CRI runtime | 28 |

### Vsock Port Allocation

| Port | Service | Protocol |
|-----:|---------|----------|
| 4088 | gRPC agent control | Protobuf |
| 4089 | Exec server | Binary framing |
| 4090 | PTY server | Binary framing |
| 4091 | Attestation server | RA-TLS |

## TEE (Confidential Computing)

### Configuration

```rust
use a3s_box_core::config::{BoxConfig, TeeConfig, SevSnpGeneration};

let config = BoxConfig {
    tee: TeeConfig::SevSnp {
        workload_id: "my-secure-workload".to_string(),
        generation: SevSnpGeneration::Milan,  // or Genoa
    },
    ..Default::default()
};
```

### Hardware Requirements

- AMD EPYC 7003 (Milan) or 9004 (Genoa) with SEV-SNP
- Linux kernel 5.19+ with SEV-SNP patches
- `/dev/sev` and `/dev/sev-guest` accessible
- Cloud: Azure DCasv5/ECasv5

> AMD Ryzen, Intel CPUs, and Apple Silicon do NOT support SEV-SNP.

### Simulation Mode

For development without SEV-SNP hardware:

```bash
export A3S_TEE_SIMULATE=1
a3s-box run -d --name dev --tee --tee-simulate alpine:latest -- sleep 3600
a3s-box attest dev --ratls --allow-simulated
a3s-box seal dev --data "secret" --context ctx --policy measurement-and-chip --allow-simulated
a3s-box inject-secret dev --secret "KEY=val" --set-env --allow-simulated
```

Simulation generates fake attestation reports with deterministic keys. Not suitable for production:
- ECDSA report signature verification bypassed (no hardware signature in simulation)
- No hardware memory encryption
- Sealed data NOT portable to real hardware (different key derivation inputs)

## Testing

### Unit Tests — 1,152 passed

| Crate | Tests | Coverage |
|-------|------:|----------|
| `a3s-box-cli` | 367 | State management, name resolution, output formatting, restart policies |
| `a3s-box-core` | 171 | Config validation, error types, event serialization, TEE protocol types, TEE self-detection |
| `a3s-box-runtime` | 517 | OCI parsing, rootfs, health checking, attestation, RA-TLS, sealed storage, heartbeat, Prometheus metrics |
| `a3s-box-cri` | 34 | CRI sandbox/container lifecycle, config mapping |
| `a3s-box-guest-init` | 52 | Exec server, attest server frame I/O, secret validation |
| `a3s-box-sdk` | 11 | SDK init, config building, exec result conversion, serde roundtrip |

All unit tests run without VM, network, or hardware dependencies (`A3S_DEPS_STUB=1` for CI).

```bash
just test                         # All unit tests
cargo test -p a3s-box-cli --lib   # CLI only (367 tests)
cargo test -p a3s-box-runtime     # Runtime only (506 tests)
```

### Integration Tests — 7 tests

All `#[ignore]` — require built binary, hardware virtualization, and network access.

| Test | Flow |
|------|------|
| `test_alpine_full_lifecycle` | pull → run → ps → inspect → exec → logs → stop → rm |
| `test_exec_commands` | run → exec (cat, ls, env, write+read file) → cleanup |
| `test_env_and_labels` | run with `-e`/`-l` → verify env vars inside guest → cleanup |
| `test_nginx_image_pull_and_run` | pull nginx → run with port mapping → check HTTP → cleanup |
| `test_tee_seal_unseal_lifecycle` | run `--tee-simulate` → attest → seal → unseal → verify wrong context fails |
| `test_tee_secret_injection` | run `--tee-simulate` → inject 2 secrets → verify `/run/secrets/*` |
| `test_tee_seal_policies` | seal/unseal roundtrip for each policy (measurement-and-chip, measurement-only, chip-only) |

```
Host                                          Guest VM (MicroVM)
┌──────────────────────┐                     ┌──────────────────────────┐
│  cargo test          │                     │  /sbin/init (PID 1)      │
│  └─ a3s-box attest ──┼── RA-TLS (4091) ──►│  └─ attest_server        │
│  └─ a3s-box seal   ──┼── RA-TLS (4091) ──►│     (SNP report in X.509)│
│  └─ a3s-box unseal ──┼── RA-TLS (4091) ──►│                          │
│  └─ a3s-box inject ──┼── RA-TLS (4091) ──►│  └─ /run/secrets/*       │
│  └─ a3s-box exec   ──┼── vsock  (4089) ──►│  └─ exec_server          │
└──────────────────────┘                     └──────────────────────────┘
```

### Running Integration Tests

```bash
cd crates/box/src
cargo build -p a3s-box-cli

# macOS only: set library paths
export DYLD_LIBRARY_PATH="$(ls -td target/debug/build/libkrun-sys-*/out/libkrun/lib | head -1):$(ls -td target/debug/build/libkrun-sys-*/out/libkrunfw/lib | head -1)"

# VM lifecycle tests
cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture

# TEE tests (single-threaded)
cargo test -p a3s-box-cli --test tee_integration -- --ignored --nocapture --test-threads=1
```

**Limitations:** Requires HVF/KVM (no CI without nested virt). TEE tests use simulation mode. First run downloads images. Each test boots a real MicroVM. Sealed data from simulation is not portable to real hardware.

## A3S Ecosystem

A3S Box is the **infrastructure layer** of the A3S ecosystem. It provides VM isolation for any workload — it does not know what runs inside.

```
┌────────────────────────────────────────────────────────────┐
│                     A3S Ecosystem                          │
│                                                            │
│  ┌──────────────────────────────────────────────────────┐  │
│  │   a3s-gateway (K8s Ingress Controller, optional)     │  │
│  │   Routes traffic to Pods — application-agnostic      │  │
│  └────────────────────┬─────────────────────────────────┘  │
│                       │                                    │
│  ┌────────────────────▼─────────────────────────────────┐  │
│  │              a3s-box (this project)                   │  │
│  │      VM Runtime — Standalone CLI & K8s RuntimeClass   │  │
│  │          TEE when hardware supports, VM always        │  │
│  │                                                       │  │
│  │  ┌─────────────────────────────────────────────────┐  │  │
│  │  │   Guest workload (any OCI image)                │  │  │
│  │  │   e.g. SafeClaw + A3S Code, or any other app    │  │  │
│  │  └─────────────────────────────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

> A3S Box is application-agnostic. It provides the same VM isolation whether the guest is SafeClaw, a web server, or a database.

| Project | Layer | Relationship to Box |
|---------|-------|---------------------|
| **box** (this) | Infrastructure | VM runtime — standalone CLI or K8s RuntimeClass |
| [gateway](https://github.com/a3s-lab/gateway) | Ingress | Routes external traffic to Pods running in a3s-box VMs |
| [code](https://github.com/a3s-lab/code) | Agent Service | Can run inside a3s-box VM as a guest process |
| [safeclaw](https://github.com/a3s-lab/safeclaw) | Security Proxy | Can run inside a3s-box VM alongside a3s-code |

## Roadmap

### Completed ✅

| Phase | What |
|-------|------|
| Foundation | MicroVM runtime, libkrun, HVF/KVM detection, vsock communication |
| OCI & Isolation | Image parser, rootfs composition, guest init (PID 1), namespace isolation |
| CLI (47 commands) | Full Docker-compatible CLI, state management, name resolution, Dockerfile build |
| CRI Runtime | Kubernetes RuntimeService + ImageService, deployment manifests |
| Docker Parity | Networking (bridge, IPAM, DNS), volumes (named, anonymous, tmpfs), resource limits, security hardening, logging, PTY, commit/diff/events |
| TEE Core | SEV-SNP detection, configuration, shim integration |
| Remote Attestation | SNP report parsing, ECDSA-P384 verification, certificate chain, KDS client, RA-TLS, simulation mode |
| Sealed Storage | HKDF-SHA256 key derivation, AES-256-GCM, three sealing policies, seal/unseal CLI |
| Secret Injection | RA-TLS channel, `/run/secrets/`, env var support |
| Performance | Rootfs caching, layer cache, warm pool with TTL and auto-replenish |
| Host SDK & Transport | `a3s-transport` Frame protocol, exec/PTY/attest servers migrated, `FrameReader`/`FrameWriter` async I/O, shared port constants and TEE request types |
| Embedded Sandbox SDK | `a3s-box-sdk` crate: `BoxSdk` → `Sandbox` lifecycle, exec/PTY from Rust code, no daemon required, OCI image support, configurable resources/env/mounts |

### In Progress 🚧

**Production Hardening**
- [ ] VM snapshot/restore (save running state to SSD, restore < 500ms)
- [ ] Network isolation policies
- [ ] Audit logging

**TEE Hardening**
- [x] Bind TLS public key hash to `report_data` (RA-TLS key binding)
- [x] Certificate chain ECDSA signature verification (VCEK→ASK→ARK)
- [x] Attestation report age checking (replay protection)
- [ ] KBS (Key Broker Service) integration
- [ ] Periodic re-attestation
- [ ] Version-based rollback protection for sealed storage
- [ ] Real hardware testing on AMD SEV-SNP (Azure DCasv5 / bare-metal EPYC)

### Planned 📋

**Host SDK & Transport**
- [x] `a3s-transport` crate: unified `Transport` trait with framing protocol
- [x] `VsockTransport` / `MockTransport` implementations
- [x] Guest-side TEE self-detection API via `a3s-box-core`: `detect_tee()`, `TeeCapability`, `TeeType`
- [x] Migrate exec/PTY/attest servers to shared framing protocol
- [x] Migrate health check from HTTP to Frame Heartbeat protocol

**Observability & Scaling**
- [x] Prometheus metrics (VM boot time, memory, CPU, exec, image pull, warm pool)
- [ ] OpenTelemetry spans (VM lifecycle: create → boot → ready)
- [ ] Autoscaler with warm pool pressure-based scaling
- [ ] Kubernetes Operator (BoxAutoscaler CRD)

**Knative Serving — Instance Executor**

Box acts as the "hands" of Knative-style serverless serving — it executes instance lifecycle operations on demand. Supports two deployment modes:
- **Standalone**: Gateway calls Box Scale API directly, Box manages MicroVMs on the host
- **K8s**: kubelet calls Box via CRI (already implemented), K8s manages replicas, Box provides the MicroVM runtime

- [ ] **Scale API (standalone mode)**: Expose an internal API for Gateway to request instance scale-up/scale-down (`POST /scale {service, replicas}`) — create, start, or stop MicroVMs on demand
- [ ] **Instance readiness signaling**: Report instance state transitions (Creating → Booting → Ready → Busy → Stopping) to Gateway via callback or event, so Gateway knows when to start forwarding traffic
- [ ] **VM snapshot/restore for cold start**: Save running VM state to SSD, restore in < 500ms — critical for scale-from-zero latency in both standalone and K8s modes
- [ ] **Warm pool auto-scaling**: Dynamically adjust warm pool `min_idle` based on Gateway's scaling pressure signals — pre-warm more VMs when traffic is trending up
- [ ] **Instance health reporting**: Continuously report per-instance health (CPU, memory, in-flight requests) to Gateway for autoscaler decision-making
- [ ] **Graceful scale-down**: Drain in-flight requests before stopping a VM — coordinate with Gateway to stop routing new requests, wait for completion, then terminate
- [ ] **Instance self-registration (standalone mode)**: On boot, each Box instance registers its endpoint with Gateway's service discovery — enables multi-node standalone deployments without K8s

**Docker Parity (remaining)**
- [ ] Multi-container orchestration (compose-like YAML)
- [ ] Buildx multi-platform builds
- [ ] Secrets management (`--secret`)
- [ ] CRI streaming API (Exec, Attach, PortForward)
- [ ] Image signing (cosign/notation)
- [ ] Seccomp profiles, no-new-privileges

> Items that belong to other projects (not Box):
> - **SafeClaw**: security proxy logic (injection detection, taint tracking, output sanitization, audit pipeline)
> - **a3s-code**: agent configuration from OCI labels, pre-built guest image, Python SDK

## Development

### Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `A3S_DEPS_STUB` | Stub mode (skip libkrun) | — |
| `A3S_IMAGE_CACHE_SIZE` | Image cache size (`500m`, `20g`, `1t`) | `10g` |
| `A3S_TEE_SIMULATE` | TEE simulation mode | — |
| `RUST_LOG` | Log level | `info` |

### Commands

```bash
just build          # Build all
just release        # Release build
just test           # All unit tests
just fmt            # Format
just lint           # Clippy
just ci             # Full CI checks
```

### Project Structure

```
box/
├── src/
│   ├── cli/            # Docker-like CLI (a3s-box binary, 47 commands)
│   ├── core/           # Config, error types, events
│   ├── runtime/        # VM lifecycle, OCI, health checking, attestation
│   ├── shim/           # VM subprocess shim (libkrun bridge)
│   ├── cri/            # CRI runtime for Kubernetes
│   └── guest/init/     # Guest PID 1, exec/PTY/attestation servers
├── docs/               # Documentation
└── CLAUDE.md           # Development guidelines
```

### Troubleshooting

`invalid linker name '-fuse-ld=lld'` → `brew install lld`

`Vendored sources not found` → `git submodule update --init --recursive`

Testing without VM → `A3S_DEPS_STUB=1 cargo check -p a3s-box-runtime`

## Documentation

| Document | Description |
|----------|-------------|
| [CRI Implementation Plan](./docs/cri-implementation-plan.md) | Kubernetes CRI integration |
| [Rootfs Explained](./docs/rootfs-explained.md) | Root filesystem in MicroVMs |
| [Hooks Design](./docs/hooks-design.md) | Extensibility hooks |

## License

MIT

---

<p align="center">
  Built by <a href="https://github.com/a3s-lab">A3S Lab</a>
</p>