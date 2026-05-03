# A3S Box

<p align="center">
  <strong>MicroVM Runtime</strong>
</p>

<p align="center">
  <em>Run any OCI image in a hardware-isolated MicroVM. ~200ms cold start. Docker-compatible CLI, Kubernetes CRI runtime, and optional AMD SEV-SNP confidential computing.</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#cli-reference">CLI</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#tee">TEE</a>
</p>

---

## What is A3S Box?

A3S Box is a **MicroVM runtime** — not a platform, not an orchestrator. It runs workloads inside hardware-isolated virtual machines.

**Core properties:**
- **Isolated**: Each workload gets its own Linux kernel, memory encryption (with TEE), and namespace isolation
- **Compatible**: Runs any OCI image — Docker Hub, private registries, self-built images
- **Fast**: ~200ms cold start via libkrun (Apple HVF on macOS, KVM on Linux, WHPX on Windows)
- **Portable**: Same CLI and CRI interface across macOS, Linux, and Windows

**Two ways to use it:**
- **CLI** (`a3s-box run`) — Docker-like commands for local development and production
- **CRI** (`a3s-box-shim`) — Kubernetes RuntimeClass for pod isolation

A3S Box is application-agnostic. It doesn't know what's inside the VM — web servers, databases, AI agents, or anything else packaged as an OCI image.

## Features

### Runtime

| Capability | Description |
|-----------|-------------|
| OCI Images | Pull, push, build, tag, inspect from any registry with local LRU cache |
| Dockerfile/Containerfile Build | Multi-stage builds, all instructions, `ADD <url>` HTTP download, `ONBUILD` triggers |
| Multi-Platform | `--platform linux/amd64,linux/arm64` with OCI Image Index |
| Snapshot/Restore | Configuration-based VM snapshots |
| Cross-Platform | macOS ARM64, Linux x86_64/ARM64, Windows x86_64 |
| Warm Pool | Pre-booted VMs for instant allocation |

### CLI (52 commands)

| Category | Commands |
|----------|----------|
| Lifecycle | `run`, `create`, `start`, `stop`, `pause`, `unpause`, `restart`, `rm`, `kill`, `wait` |
| Execution | `exec`, `attach`, `top`, `shell` |
| Images | `pull`, `push`, `build`, `images`, `rmi`, `tag`, `image-inspect`, `history`, `save`, `load`, `commit` |
| Filesystem | `cp`, `export`, `diff` |
| Networking | `network create`, `ls`, `rm`, `inspect`, `connect`, `disconnect` |
| Volumes | `volume create`, `ls`, `rm`, `inspect`, `prune` |
| Snapshots | `snapshot create`, `restore`, `ls`, `rm`, `inspect` |
| Compose | `compose up`, `down`, `ps`, `config` |
| Observability | `ps`, `logs`, `inspect`, `stats`, `events`, `df`, `port` |
| System | `system-prune`, `container-update`, `login`, `logout`, `audit`, `monitor`, `version` |

### Security

| Layer | Mechanism |
|-------|-----------|
| VM Isolation | Separate Linux kernel, memory isolation via virtualization |
| Namespaces | mount, PID, IPC, UTS, user, cgroup within VM |
| Resource Limits | CPU pinning/shares/quota, memory limits, PID limits, ulimits (cgroup v2) |
| Capabilities | `--cap-add/drop`, bounding + ambient set clearing |
| Seccomp | BPF filter with architecture validation |
| Image Signing | Cosign key-based and keyless verification on pull |
| Network Policies | Ingress/egress rules per network |

### TEE (Confidential Computing)

| Feature | Description |
|---------|-------------|
| AMD SEV-SNP | Hardware memory encryption (Milan/Genoa EPYC) |
| Remote Attestation | SNP report generation, ECDSA-P384 verification, certificate chain |
| RA-TLS | SNP report in X.509 certificate extensions |
| Sealed Storage | AES-256-GCM with HKDF-SHA256, measurement/chip policies |
| Secret Injection | Secrets over RA-TLS to `/run/secrets/` |
| KBS Client | RATS challenge-response for key brokering |
| Simulation | Full TEE workflow on any hardware via `A3S_TEE_SIMULATE=1` |

### Observability

- **Metrics**: 19 Prometheus metrics (VM boot, exec, image pull, cache, pool)
- **Tracing**: OpenTelemetry spans for VM lifecycle, exec, destroy
- **Audit**: Persistent JSON-lines log with query filters

### Kubernetes

- CRI v1 implementation (RuntimeService + ImageService)
- DaemonSet + RuntimeClass deployment via Helm

## Quick Start

### Install

```bash
# macOS / Linux
brew install a3s-lab/tap/a3s-box

# Windows
winget install A3SLab.Box

# Or build from source
git clone https://github.com/A3S-Lab/Box.git
cd Box/src && cargo build --release
```

### Run your first box

```bash
# Run an OCI image
a3s-box run --name hello alpine:latest -- echo "Hello from MicroVM"

# Interactive shell
a3s-box run -it --name dev alpine:latest -- /bin/sh

# With resources
a3s-box run -d --name web --cpus 2 --memory 1g nginx:alpine
```

### Pull and inspect images

```bash
# Pull with signature verification
a3s-box pull --verify-key cosign.pub myimage:latest

# List cached images
a3s-box images

# Inspect image metadata
a3s-box image-inspect myimage:latest
```

### Execute commands

```bash
# Run command in box
a3s-box exec mybox -- ls -la

# With environment and user
a3s-box exec -it -u root -e FOO=bar mybox -- /bin/sh
```

### Networking

```bash
# Create isolated network
a3s-box network create backend --isolation strict

# Run box in network with port mapping
a3s-box run -d --name api --network backend -p 8080:80 myapi:latest
```

### TEE (confidential computing)

```bash
# Run with SEV-SNP (requires AMD EPYC hardware)
a3s-box run -d --name secure --tee myimage:latest -- sleep 3600

# Or simulate TEE on any hardware
export A3S_TEE_SIMULATE=1
a3s-box run -d --name dev --tee --tee-simulate myimage:latest -- sleep 3600

# Attest the TEE
a3s-box attest secure --ratls --allow-simulated

# Inject secrets via RA-TLS
a3s-box inject-secret secure --secret "API_KEY=secret" --set-env --allow-simulated
```

## CLI Reference

### Lifecycle

```bash
a3s-box run [OPTIONS] IMAGE [CMD...]        # Pull + create + start
a3s-box create [OPTIONS] IMAGE [CMD...]     # Create without starting
a3s-box start BOX [BOX...]                  # Start stopped boxes
a3s-box stop BOX [BOX...]                   # Graceful stop
a3s-box restart BOX [BOX...]                # Restart
a3s-box rm BOX [BOX...]                     # Remove (-f force)
a3s-box pause BOX [BOX...]                  # SIGSTOP
a3s-box unpause BOX [BOX...]                # SIGCONT
a3s-box kill BOX [BOX...]                   # Force kill
a3s-box wait BOX [BOX...]                   # Block until stop
```

### Execution

```bash
a3s-box exec [OPTIONS] BOX CMD [ARG...]
  -it           # Interactive PTY
  -u USER       # User (default: root)
  -e KEY=VAL    # Environment variable
  -w DIR        # Working directory

a3s-box attach BOX                        # Attach to PTY
a3s-box top BOX                           # Show processes
a3s-box shell BOX                         # Interactive shell (-u root)
```

### Images

```bash
a3s-box pull [OPTIONS] IMAGE              # Pull from registry
  --verify-key PATH    # Cosign key verification
  --verify-issuer URL  # Keyless issuer verification

a3s-box push IMAGE [TAG]                  # Push to registry
a3s-box build [OPTIONS] -t TAG PATH      # Dockerfile/Containerfile build
  --platform LINUX/ARCH,...  # Multi-arch
a3s-box images                           # List cached
a3s-box rmi IMAGE [IMAGE...]              # Remove images
a3s-box tag IMAGE NEW_TAG                 # Create alias
a3s-box image-inspect IMAGE               # JSON metadata
a3s-box image-prune                       # Remove unused
a3s-box history IMAGE                     # Layer history
a3s-box save -o FILE.tar IMAGE           # Export archive
a3s-box load -i FILE.tar                 # Import archive
```

### Filesystem

```bash
a3s-box cp [OPTIONS] SRC DST              # Copy between host/box
  -a, --archive   # Preserve permissions
a3s-box export BOX -o FILE.tar            # Export box fs
a3s-box commit BOX -t TAG [OPTIONS]       # Create image from box
a3s-box diff BOX                          # Show fs changes (A/C/D)
```

### Networking

```bash
a3s-box network create NAME [OPTIONS]
  --driver bridge|tsi|none
  --isolation none|strict|custom
a3s-box network ls
a3s-box network inspect NAME
a3s-box network rm NAME [NAME...]
a3s-box network connect NETWORK BOX
a3s-box network disconnect NETWORK BOX
a3s-box port BOX                         # List port mappings
```

### Volumes

```bash
a3s-box volume create NAME [OPTIONS]
a3s-box volume ls
a3s-box volume inspect NAME
a3s-box volume rm NAME [NAME...]
a3s-box volume prune
```

### Snapshots

```bash
a3s-box snapshot create BOX NAME
a3s-box snapshot restore BOX SNAPSHOT
a3s-box snapshot ls BOX
a3s-box snapshot inspect BOX SNAPSHOT
a3s-box snapshot rm BOX SNAPSHOT
```

### Compose

```bash
a3s-box compose -f FILE.yaml up           # Start services
a3s-box compose -f FILE.yaml down         # Stop services
a3s-box compose -f FILE.yaml ps           # List services
a3s-box compose -f FILE.yaml config      # Validate config
```

### Observability

```bash
a3s-box ps [OPTIONS]                     # List boxes (-a all, -q quiet)
a3s-box logs BOX [OPTIONS]                # View logs (-f follow, --tail N)
a3s-box inspect BOX                       # Detailed JSON
a3s-box stats [OPTIONS]                   # Live resource usage
a3s-box events [OPTIONS]                  # Stream events (--json)
a3s-box df                                # Disk usage
a3s-box audit [OPTIONS]                   # Query audit log
  --action run|stop|exec|...
  --outcome success|failure
  --box BOX
```

### TEE

```bash
a3s-box attest BOX [OPTIONS]              # Request attestation
  --ratls           # RA-TLS mode
  --policy POLICY   # min-version, force, allow-simulated
  --nonce HEX       # Nonce for freshness
  --raw             # Raw report output

a3s-box seal BOX --data SECRET [OPTIONS]  # Seal data to TEE
  --context PATH    # KBS resource path
  --policy POLICY   # measurement-and-chip, measurement-only, chip-only

a3s-box unseal BOX --context PATH         # Unseal data in TEE

a3s-box inject-secret BOX --secret K=V [OPTIONS]
  --set-env        # Export as environment variables
  --allow-simulated
```

### System

```bash
a3s-box version
a3s-box info                             # System information
a3s-box login REGISTRY -u USER -p PASS  # Registry auth
a3s-box logout REGISTRY
a3s-box system-prune [OPTIONS]           # Clean up (-f force)
a3s-box container-update BOX [OPTIONS]   # Hot-update resources
  --cpus N
  --memory SIZE
  --restart always|on-failure[:N]|unless-stopped
a3s-box monitor                          # Background restart daemon
a3s-box pool [start|stop|status]        # Warm VM pool
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Host                                     │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                      a3s-box-cli                           │  │
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────┐  │  │
│  │  │  CLI (52)   │ │   State     │ │   Runtime Engine    │  │  │
│  │  │  commands   │ │  boxes.json │ │  VmManager · OCI    │  │  │
│  │  └─────────────┘ └─────────────┘ └─────────────────────┘  │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │ vsock                               │
└──────────────────────────────┼──────────────────────────────────┘
                               │
┌──────────────────────────────┼──────────────────────────────────┐
│                              ▼                                   │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │              guest-init (PID 1)                            │  │
│  │  Exec :4089  ·  PTY :4090  ·  Attest :4091              │  │
│  └───────────────────────────┬───────────────────────────────┘  │
│                              │                                   │
│  ┌───────────────────────────▼───────────────────────────────┐  │
│  │              User Namespace                                │  │
│  │  /a3s/workspace/  ·  /run/secrets/  ·  /a3s/skills/     │  │
│  └───────────────────────────────────────────────────────────┘  │
│                         Guest VM                                 │
└──────────────────────────────────────────────────────────────────┘
```

### Vsock Ports

| Port | Service | Protocol |
|-----:|---------|----------|
| 4088 | gRPC control | Health, metrics |
| 4089 | Exec server | Command execution |
| 4090 | PTY server | Terminal I/O |
| 4091 | Attestation | RA-TLS (TEE only) |

### Crates

| Crate | Binary | Purpose |
|-------|--------|---------|
| `cli` | `a3s-box` | Docker-like CLI |
| `core` | — | Config, errors, events, types |
| `runtime` | — | VM lifecycle, OCI, TEE, networking |
| `shim` | `a3s-box-shim` | libkrun bridge |
| `guest/init` | `a3s-box-guest-init` | Guest PID 1 |
| `cri` | `a3s-box-cri` | Kubernetes CRI runtime |

## TEE

AMD SEV-SNP provides hardware memory encryption. The VM's memory is encrypted with a key only the hardware knows.

### Requirements

- AMD EPYC 7003 (Milan) or 9004 (Genoa)
- Linux kernel 5.19+ with SEV-SNP patches
- `/dev/sev` and `/dev/sev-guest` accessible
- Or Azure DCasv5/ECasv5 instances

### Workflow

```bash
# 1. Run with TEE enabled
a3s-box run -d --name app --tee myimage:latest -- myapp

# 2. Attest the TEE (verify it's genuine)
a3s-box attest app --ratls

# 3. Inject secrets (delivered over RA-TLS)
a3s-box inject-secret app --secret "DB_PASSWORD=secret" --set-env

# 4. Seal data (only accessible inside this TEE)
a3s-box seal app --data "encryption-key=xyz" --context keys --policy measurement-and-chip
```

### Simulation Mode

For development without SEV-SNP hardware:

```bash
export A3S_TEE_SIMULATE=1
a3s-box run -d --name dev --tee --tee-simulate myimage -- sleep 3600
a3s-box attest dev --ratls --allow-simulated
```

## Kubernetes

### Install

```bash
helm install a3s-box deploy/helm/a3s-box/ -n a3s-box-system --create-namespace
```

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

```bash
# Build
just build              # All crates
just release            # Release build

# Test
just test               # Unit tests (no VM required)
A3S_DEPS_STUB=1 cargo test --workspace --lib

# Quality
just fmt                # Format
just lint               # Clippy
```

### Environment

| Variable | Description | Default |
|----------|-------------|---------|
| `A3S_HOME` | Data directory | `~/.a3s` |
| `A3S_DEPS_STUB` | Skip libkrun for CI | — |
| `A3S_IMAGE_CACHE_SIZE` | Cache size | `10g` |
| `A3S_TEE_SIMULATE` | TEE simulation | — |
| `RUST_LOG` | Log level | `info` |

## License

MIT
