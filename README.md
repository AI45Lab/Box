# A3S Box

<p align="center">
  <strong>MicroVM Runtime — Docker-like CLI &amp; Kubernetes RuntimeClass</strong>
</p>

<p align="center">
  <em>Run any OCI image in a hardware-isolated MicroVM. ~200ms cold start. Docker-compatible CLI for standalone use, CRI shim for Kubernetes. AMD SEV-SNP confidential computing when hardware supports, VM isolation always.</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#cli-reference">CLI Reference</a> •
  <a href="#sdk">SDK</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#tee-confidential-computing">TEE</a> •
  <a href="#testing">Testing</a>
</p>

---

## Overview

A3S Box boots OCI images inside MicroVMs powered by libkrun (Apple HVF on macOS, KVM on Linux). Each workload gets its own Linux kernel, namespace isolation, and optional AMD SEV-SNP hardware memory encryption — all with ~200ms cold start.

Two deployment modes:
- **Standalone CLI** (`a3s-box run`) — Docker-compatible commands for local development and production
- **Kubernetes RuntimeClass** (`a3s-box-shim`) — CRI runtime for kubelet, deploy via DaemonSet + RuntimeClass

A3S Box is application-agnostic. It doesn't know what runs inside — web servers, databases, AI agents, or anything else packaged as an OCI image.

## Features

### VM Runtime
- **~200ms Cold Start** — MicroVM boot via libkrun (Apple HVF / Linux KVM)
- **OCI Images** — Pull, push, build, tag, inspect, prune from any OCI registry with local LRU cache; manifest digest exposed on every pulled image
- **Dockerfile Build** — `a3s-box build` with multi-stage builds, all Dockerfile instructions, `ADD <url>` HTTP download, `ONBUILD` trigger inheritance
- **Multi-Platform Build** — `--platform linux/amd64,linux/arm64` with OCI Image Index output
- **Compose** — Multi-container orchestration via YAML (`compose up/down/ps/config`), dependency-ordered boot, shared networks
- **Snapshot/Restore** — Configuration-based VM snapshots (`snapshot create/restore/ls/rm/inspect`), rootfs preservation
- **Rootfs Caching** — Content-addressable cache with SHA256 keys and TTL/size pruning
- **Cross-Platform** — macOS (Apple Silicon) and Linux (x86_64/ARM64), no root required

### Docker-Compatible CLI (52 commands)
- **Lifecycle**: `run`, `create`, `start`, `stop`, `pause`, `unpause`, `restart`, `rm`, `kill`, `rename`, `wait`
- **Exec & PTY**: `exec` (with `-it`, `-u`, `-e`, `-w`), `attach -it`, `run -it`, `top`
- **Images**: `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `image-prune`, `history`, `save`, `load`, `export`, `commit`, `diff`
- **Networking**: `network create/ls/rm/inspect/connect/disconnect`, bridge driver, IPAM, DNS discovery
- **Volumes**: `volume create/ls/rm/inspect/prune`, named volumes, anonymous volumes, tmpfs
- **Snapshots**: `snapshot create/restore/ls/rm/inspect`
- **Observability**: `ps`, `logs`, `inspect`, `stats`, `events`, `cp`, `df`
- **System**: `system-prune`, `container-update`, `version`, `info`, `monitor`, `login`, `logout`, `audit`

### Security & Isolation
- **Namespace Isolation** — Separate mount, PID, IPC, UTS, user, and cgroup namespaces within each VM
- **Resource Limits** — CPU shares/quota/pinning, memory reservation/swap, PID limits, ulimits (cgroup v2)
- **Security Options** — Capabilities (`--cap-add/drop`) with bounding + ambient set clearing, seccomp BPF filter with architecture validation (`--security-opt seccomp=`), no-new-privileges, read-only rootfs, privileged mode, device mapping, GPU access
- **Image Signing** — Cosign-compatible signature verification via CLI (`--verify-key`, `--verify-issuer`, `--verify-identity`): key-based and keyless modes (crypto verification pending, policy enforcement active)
- **Network Isolation** — Per-network isolation policies (`--isolation none/strict/custom`), ingress/egress rules with port/protocol filtering, policy enforcement on connect
- **Audit Logging** — Persistent JSON-lines audit trail with rotation, structured events (who/what/when/outcome), queryable via `a3s-box audit` with filters
- **Restart Policies** — `always`, `on-failure:N`, `unless-stopped` with exponential backoff and max restart count enforcement
- **Health Checks** — Configurable commands with interval, timeout, retries, start period; monitor auto-restarts unhealthy boxes
- **Logging** — JSON logging driver with gzip-compressed rotation, syslog driver (UDP/TCP, RFC 3164), or `--log-driver none`

### TEE (Confidential Computing)
- **AMD SEV-SNP** — Hardware-enforced memory encryption
- **Intel TDX** — Trust Domain Extensions (config support, runtime pending)
- **Remote Attestation** — SNP report generation, ECDSA-P384 verification, certificate chain validation (VCEK→ASK→ARK)
- **RA-TLS** — SNP report embedded in X.509 certificate extensions, verified during TLS handshake
- **Secret Injection** — Inject secrets via RA-TLS into `/run/secrets/` (tmpfs, mode 0400)
- **Sealed Storage** — AES-256-GCM with HKDF-SHA256, three policies: MeasurementAndChip, MeasurementOnly, ChipOnly, version-based rollback protection
- **KBS Integration** — Key Broker Service client (RATS challenge-response), resource path routing, session tokens
- **Re-attestation** — Periodic TEE verification with configurable interval, failure threshold, grace period
- **Simulation Mode** — Full TEE workflow on any machine via `A3S_TEE_SIMULATE=1`

### Observability
- **Prometheus Metrics** — 19 metrics auto-activated on every box boot: VM boot duration/count, CPU/memory, exec total/duration/errors, image pull/build, rootfs cache, warm pool size/capacity/hits
- **Tracing Spans** — OpenTelemetry-compatible spans for VM lifecycle (`vm_boot`, `prepare_layout`, `vm_start`, `wait_for_ready`), exec, and destroy

### Kubernetes Integration
- **CRI Runtime** — RuntimeService + ImageService for kubelet
- **Deployment** — DaemonSet, RuntimeClass, Helm chart, RBAC

## Quick Start

### Prerequisites

- **macOS ARM64** (Apple Silicon) or **Linux x86_64/ARM64**

> macOS Intel is NOT supported.

### Install via Homebrew (Recommended)

```bash
brew tap a3s-lab/tap https://github.com/A3S-Lab/homebrew-tap
brew install a3s-box
```

This installs `a3s-box`, `a3s-box-shim`, and `a3s-box-guest-init`.

```bash
# Update to latest version
brew update && brew upgrade a3s-box

# Uninstall
brew uninstall a3s-box
```

### Build from Source

Requires Rust 1.75+.

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

## CLI Reference

### Usage Examples

```bash
# Run a box
a3s-box run -d --name dev --cpus 2 --memory 1g alpine:latest -- sleep 3600
a3s-box run -it alpine:latest -- /bin/sh          # Interactive shell

# Image management
a3s-box pull alpine:latest
a3s-box pull --verify-key cosign.pub alpine:latest # Verify signature
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
a3s-box network create mynet --isolation strict
a3s-box run -d --name web --network mynet -v data:/app/data nginx:alpine
a3s-box volume ls

# Observability
a3s-box ps -a --filter label=env=dev
a3s-box logs dev -f
a3s-box stats
a3s-box events --json
a3s-box audit --action run --outcome success

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

### Command Table

| Command | Description |
|---------|-------------|
| `run` | Pull + create + start (`-d`, `--rm`, `-l`, `--restart`, `--health-cmd`, `--cap-add/drop`, `--privileged`, `--read-only`, `--device`, `--gpus`, `--init`, `--env-file`, `--add-host`, `--platform`, `--tee`) |
| `create` | Create without starting (same flags as `run`) |
| `start/stop/restart/kill` | Lifecycle management (multi-target) |
| `pause/unpause` | SIGSTOP/SIGCONT |
| `rm` | Remove boxes (`-f` force) |
| `rename` | Rename a box |
| `wait` | Block until boxes stop |
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
| `df` | Show disk usage |
| `images` | List cached images |
| `pull` | Pull image (`--verify-key`, `--verify-issuer`, `--verify-identity`) |
| `push` | Push image to registry |
| `build` | Dockerfile build (`--platform` for multi-arch) |
| `rmi` | Remove images |
| `tag` | Create image alias |
| `image-inspect` | Image metadata |
| `image-prune` | Remove unused images |
| `history` | Show image layer history |
| `save/load` | Export/import image archives |
| `export` | Export box filesystem to tar |
| `network` | `create/ls/rm/inspect/connect/disconnect` |
| `volume` | `create/ls/rm/inspect/prune` |
| `snapshot` | `create/restore/ls/rm/inspect` |
| `compose` | `up/down/ps/config` |
| `system-prune` | Remove stopped boxes + unused images |
| `login/logout` | Registry authentication |
| `attest` | TEE attestation (`--ratls`, `--policy`, `--nonce`, `--raw`, `--quiet`) |
| `seal/unseal` | Sealed storage operations |
| `inject-secret` | Inject secrets via RA-TLS |
| `audit` | Query audit log (`--action`, `--box`, `--outcome`) |
| `monitor` | Background restart daemon |
| `version/info` | System information |

## SDK

### Embedded Rust SDK

Create, exec, and stop MicroVM sandboxes directly from Rust code — no CLI or daemon required.

```rust
use a3s_box_sdk::{BoxSdk, SandboxOptions};

let sdk = BoxSdk::new()?;
let sandbox = sdk.create(SandboxOptions {
    image: "python:3.12-slim".into(),
    cpus: 2,
    memory_mb: 1024,
    ..Default::default()
}).await?;

let result = sandbox.exec("python", &["-c", "print('hello')"]).await?;
println!("{}", result.stdout);

sandbox.stop().await?;
```

Capabilities:
- Streaming exec via `sandbox.exec_stream()` with async event iterator
- File transfer via `sandbox.upload()` / `sandbox.download()`
- Port forwarding via `SandboxOptions::port_forwards`
- Persistent workspaces that survive sandbox restarts
- Per-exec metrics (duration, stdout/stderr byte counts)
- Interactive PTY via `sandbox.pty()`
- Pause/resume via `sandbox.pause()` / `sandbox.resume()`
- Optional embedded shim (`--features embed-shim`): compiles and bundles `a3s-box-shim` into the binary, auto-extracts to `~/.a3s/bin/` on first use

### Multi-Language SDKs

| SDK | Package | Version | Tests |
|-----|---------|---------|------:|
| Python | `pip install a3s-box` | 0.5.0 | 25 |
| TypeScript | `npm install @a3s-lab/box` | 0.5.0 | 21 |
| Rust | `a3s-box-sdk` crate | 0.5.0 | 24 |

All SDKs provide: async API, streaming exec, file transfer, sandbox lifecycle management.

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
│  │  - Isolated mount, PID, IPC, UTS, user, cgroup namespaces │  │
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

| Crate | Binary | Purpose | Version | Tests |
|-------|--------|---------|---------|------:|
| `cli` | `a3s-box` | Docker-like CLI (52 commands) | 0.5.0 | 361 |
| `core` | — | Config, error types, events | 0.5.0 | 331 |
| `runtime` | — | VM lifecycle, OCI, attestation | 0.5.0 | 711 |
| `guest/init` | `a3s-box-guest-init` | Guest PID 1, exec/PTY/attestation servers | 0.5.0 | 25 |
| `shim` | `a3s-box-shim` | libkrun bridge | 0.5.0 | 14 |
| `cri` | `a3s-box-cri` | Kubernetes CRI runtime | 0.5.0 | 33 |
| `sdk` | — | Embedded sandbox SDK | 0.5.0 | 24 |

218 source files, ~1,499 unit tests, 7 integration tests.

### Vsock Port Allocation

| Port | Service | Protocol |
|-----:|---------|----------|
| 4088 | gRPC agent control | Protobuf |
| 4089 | Exec server | Binary framing |
| 4090 | PTY server | Binary framing |
| 4091 | Attestation server | RA-TLS |

### Library-Only Modules

The following modules are implemented and tested but exist as library code for external consumers (Gateway, Operator). They are not directly exposed via CLI:

- **Scale API** — `ScaleRequest`/`ScaleResponse` types, instance readiness signaling, service health aggregation, graceful drain
- **K8s Operator** — `BoxAutoscaler` CRD types, ratio-based autoscaling, multi-metric evaluation, stabilization windows
- **Warm Pool** — Pre-booted idle MicroVM pool with TTL, auto-replenish, pressure-based autoscaler

## TEE (Confidential Computing)

### Configuration

```rust
use a3s_box_core::config::{BoxConfig, TeeConfig, SevSnpGeneration};

// AMD SEV-SNP
let config = BoxConfig {
    tee: TeeConfig::SevSnp {
        workload_id: "my-secure-workload".to_string(),
        generation: SevSnpGeneration::Milan,  // or Genoa
        simulate: false,
    },
    ..Default::default()
};

// Intel TDX (config support, runtime pending)
let config = BoxConfig {
    tee: TeeConfig::Tdx {
        workload_id: "my-workload".to_string(),
        simulate: false,
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
- ECDSA report signature verification bypassed
- No hardware memory encryption
- Sealed data NOT portable to real hardware

### Pending Validation 🔬

All TEE code is implemented and unit-tested. Hardware validation on real AMD SEV-SNP silicon (Azure DCasv5 / bare-metal EPYC) is pending.

## Testing

### Unit Tests — 1,499 passed

| Crate | Tests | Coverage |
|-------|------:|----------|
| `a3s-box-cli` | 361 | State management, name resolution, output formatting, restart policies, compose, audit, snapshot, network isolation, max restart count |
| `a3s-box-core` | 331 | Config validation, error types, event serialization, TEE types (SEV-SNP + TDX), security config (AppArmor/SELinux warnings), compose types, network policies (validation), scale API types, operator CRD types, IPv6 IPAM, volume quota |
| `a3s-box-runtime` | 711 | OCI parsing, rootfs, health checking, attestation, RA-TLS, sealed storage, Prometheus metrics, tracing spans, image signing (honest verification), compose orchestrator, audit log, snapshot store, KBS client, re-attestation, rollback protection, syslog driver, gzip log compression, manifest digest, ONBUILD trigger parsing |
| `a3s-box-cri` | 33 | CRI sandbox/container lifecycle, config mapping (SEV-SNP + TDX) |
| `a3s-box-guest-init` | 25 | Exec server, attest server frame I/O, secret validation, namespace security (user + cgroup), seccomp arch validation |
| `a3s-box-sdk` | 24 | SDK init, config building, exec result conversion, port forwards, workspaces, serde roundtrip, pause/resume |
| `a3s-box-shim` | 14 | Shim config, cgroup, cpuset, ulimit, TEE config |

All unit tests run without VM, network, or hardware dependencies (`A3S_DEPS_STUB=1` for CI).

```bash
just test                         # All unit tests
cargo test -p a3s-box-cli --lib   # CLI only
cargo test -p a3s-box-runtime     # Runtime only
```

### Integration Tests — 7 tests

Require built binary, hardware virtualization, and network access (`#[ignore]`).

| Test | Flow |
|------|------|
| `test_alpine_full_lifecycle` | pull → run → ps → inspect → exec → logs → stop → rm |
| `test_exec_commands` | run → exec (cat, ls, env, write+read file) → cleanup |
| `test_env_and_labels` | run with `-e`/`-l` → verify env vars inside guest → cleanup |
| `test_nginx_image_pull_and_run` | pull nginx → run with port mapping → check HTTP → cleanup |
| `test_tee_seal_unseal_lifecycle` | run `--tee-simulate` → attest → seal → unseal → verify wrong context fails |
| `test_tee_secret_injection` | run `--tee-simulate` → inject 2 secrets → verify `/run/secrets/*` |
| `test_tee_seal_policies` | seal/unseal roundtrip for each policy |

```bash
cd crates/box/src
cargo build -p a3s-box-cli

# macOS: set library paths
export DYLD_LIBRARY_PATH="$(ls -td target/debug/build/libkrun-sys-*/out/libkrun/lib | head -1):$(ls -td target/debug/build/libkrun-sys-*/out/libkrunfw/lib | head -1)"

cargo test -p a3s-box-cli --test nginx_integration -- --ignored --nocapture
cargo test -p a3s-box-cli --test tee_integration -- --ignored --nocapture --test-threads=1
```

## A3S Ecosystem

A3S Box is the infrastructure layer. It provides VM isolation for any workload.

```
┌────────────────────────────────────────────────────────────┐
│                     A3S Ecosystem                          │
│                                                            │
│  ┌──────────────────────────────────────────────────────┐  │
│  │   a3s-gateway (Ingress Controller, optional)         │  │
│  └────────────────────┬─────────────────────────────────┘  │
│                       │                                    │
│  ┌────────────────────▼─────────────────────────────────┐  │
│  │              a3s-box (this project)                   │  │
│  │      MicroVM Runtime — CLI & K8s RuntimeClass         │  │
│  │                                                       │  │
│  │  ┌─────────────────────────────────────────────────┐  │  │
│  │  │   Guest workload (any OCI image)                │  │  │
│  │  └─────────────────────────────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

| Project | Layer | Relationship to Box |
|---------|-------|---------------------|
| **box** (this) | Infrastructure | MicroVM runtime — standalone CLI or K8s RuntimeClass |
| [gateway](https://github.com/a3s-lab/gateway) | Ingress | Routes traffic to Pods running in a3s-box VMs |
| [code](https://github.com/a3s-lab/code) | Agent | Can run inside a3s-box VM as a guest process |
| [safeclaw](https://github.com/a3s-lab/safeclaw) | Security Proxy | Can run inside a3s-box VM alongside a3s-code |

## Kubernetes Deployment

### Helm

```bash
# Install
helm install a3s-box deploy/helm/a3s-box/ -n a3s-box-system --create-namespace

# Custom values
helm install a3s-box deploy/helm/a3s-box/ -n a3s-box-system --create-namespace \
  --set image.tag=v0.5.0 \
  --set config.logLevel=debug \
  --set config.imageCacheSize=21474836480 \
  --set resources.limits.memory=1Gi

# Uninstall
helm uninstall a3s-box -n a3s-box-system
```

Key values:

| Value | Description | Default |
|-------|-------------|---------|
| `image.repository` | CRI image | `ghcr.io/a3s-lab/a3s-box-cri` |
| `image.tag` | Image tag | `latest` |
| `nodeSelector` | Node selection | `a3s-box.io/runtime: "true"` |
| `config.imageCacheSize` | Image cache bytes | `10737418240` (10 GB) |
| `config.logLevel` | Log level | `info` |
| `overhead.memory` | Per-pod VM overhead | `30Mi` |
| `overhead.cpu` | Per-pod VM overhead | `50m` |
| `resources.limits.memory` | CRI pod memory limit | `512Mi` |

### Run a Pod

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: hello
spec:
  runtimeClassName: a3s-box
  containers:
    - name: alpine
      image: alpine:latest
      command: ["sleep", "3600"]
```

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
│   ├── cli/            # Docker-like CLI (a3s-box binary, 52 commands)
│   ├── core/           # Config, error types, events
│   ├── runtime/        # VM lifecycle, OCI, health checking, attestation
│   ├── shim/           # VM subprocess shim (libkrun bridge)
│   ├── cri/            # CRI runtime for Kubernetes
│   ├── sdk/            # Embedded sandbox SDK
│   └── guest/init/     # Guest PID 1, exec/PTY/attestation servers
├── sdk/
│   ├── python/         # Python SDK (PyO3)
│   └── node/           # TypeScript SDK (napi-rs)
├── deploy/
│   ├── helm/           # Helm chart
│   ├── examples/       # Example Pod specs
│   └── scripts/        # CRI smoke test
└── CLAUDE.md           # Development guidelines
```

### Troubleshooting

`invalid linker name '-fuse-ld=lld'` → `brew install lld`

`Vendored sources not found` → `git submodule update --init --recursive`

Testing without VM → `A3S_DEPS_STUB=1 cargo check -p a3s-box-runtime`

## License

MIT

---

<p align="center">
  Built by <a href="https://github.com/a3s-lab">A3S Lab</a>
</p>
