# Changelog

All notable changes to A3S Box will be documented in this file.

## [Unreleased]

## [0.8.12] — 2026-03-20

### Fixed
- macOS bridge networking restored for shim-hosted netproxy so `localhost` port publishing works reliably again
- Linux release CI restored by adding the missing `prometheus` dependency back to the workspace
- Windows release builds no longer fail on non-macOS network setup bindings
- Release workflow can dispatch the winget publish workflow with `actions: write`

## [0.4.0] — 2026-02-18

### Added
- Helm chart for Kubernetes deployment (`deploy/helm/a3s-box/`)
- Network isolation enforcement via `--isolation` flag on `network create`
- Image signature verification CLI flags (`--verify-key`, `--verify-issuer`, `--verify-identity`)
- Prometheus metrics auto-activated on every box boot
- Embedded shim support in SDK (`--features embed-shim`)
- Compose orchestration execution (`compose up/down/ps`)

### Changed
- CI workflow optimized: platform builds use `cargo check` instead of full release build
- Clippy and SDK checks now include stub libkrun for reliable linking
- README rewritten based on verified capabilities
- Shared CLI helpers extracted into `commands/common.rs` (DRY)
- Large files split into focused submodules
- Vendored a3s-transport replaced with a3s-common dependency

### Fixed
- Codesign race condition on macOS: concurrent tests no longer fail with file lock protection
- `build/` and `dist/` gitignore patterns scoped to root only

### Removed
- Root Dockerfile (legacy prototype, not part of Box)
- `.dockerignore` (no longer needed)
- `src/sdk/PLAN.md` (completed plan)
- Duplicate `deploy/daemonset.yaml` and `deploy/runtime-class.yaml`
- `deploy/examples/ai-agent-pod.yaml` (a3s-code specific, not Box)
- Kustomize manifests (replaced by Helm chart)
- Dead documentation links in README
- Dead code: `find_agent_binary`, agent/gRPC port 4088 code
- `updater` crate (moved to separate repo)

## [0.3.0] — 2025-02-17

### Added
- Python SDK (`pip install a3s-box`) — async API, streaming exec, file transfer (25 tests)
- TypeScript SDK (`npm install @a3s-lab/box`) — Node.js API, async iterator streaming (21 tests)
- Embedded Rust SDK — `BoxSdk` → `Sandbox` lifecycle, exec/PTY, streaming, file transfer, port forwarding, persistent workspaces, execution metrics (18 tests)
- Full release pipeline — crates.io, PyPI, npm, Homebrew, GitHub Release
- Kubernetes BoxAutoscaler CRD — ratio-based autoscaling, multi-metric evaluation, stabilization windows
- Scale API — instance readiness signaling, service health aggregation, graceful drain, instance registry
- Warm pool auto-scaling with Gateway pressure signals
- TEE hardening — KBS integration, periodic re-attestation, version-based rollback protection
- VM snapshot/restore (`snapshot create/restore/ls/rm/inspect`)
- Network isolation policies (none/strict/custom)
- Audit logging with JSON-lines trail and CLI query
- Multi-platform builds (`--platform linux/amd64,linux/arm64`)
- Compose orchestration (`compose up/down/ps/config`)
- Image signing verification (cosign-compatible)
- Seccomp profiles, no-new-privileges, capability dropping
- Prometheus metrics (18 metrics) and OpenTelemetry tracing spans

### Changed
- SDKs rewritten as native bindings (PyO3 + napi-rs)
- Vendored a3s-transport replaced with a3s-common dependency
- Large files split into focused submodules

### Fixed
- Network env vars moved from shim to entrypoint
- npm package size reduced
- macOS stub libkrun path for CI

## [0.2.0] — 2025-02-16

### Added
- Docker-compatible CLI (50 commands)
- OCI image management (pull, push, build, tag, inspect, prune)
- Dockerfile build with multi-stage support
- CRI runtime (RuntimeService + ImageService)
- Networking (bridge driver, IPAM, DNS discovery)
- Volumes (named, anonymous, tmpfs)
- Resource limits (CPU, memory, PID, ulimits via cgroup v2)
- Security options (capabilities, privileged mode, device mapping, GPU)
- Health checks, restart policies, logging drivers
- PTY support, exec, attach, top
- commit, diff, events, cp, export, save, load
- TEE core — SEV-SNP detection, configuration, shim integration
- Remote attestation — SNP report, ECDSA-P384, certificate chain, RA-TLS, simulation mode
- Sealed storage — HKDF-SHA256, AES-256-GCM, three sealing policies
- Secret injection via RA-TLS
- Rootfs caching, warm pool with TTL
- Guest init (PID 1) with exec/PTY/attestation servers

## [0.1.0] — 2025-02-15

### Added
- MicroVM runtime via libkrun (Apple HVF / Linux KVM)
- ~200ms cold start
- OCI image parser and rootfs composition
- Guest init with namespace isolation
- Vsock communication (exec, PTY, attestation)
- Cross-platform: macOS Apple Silicon, Linux x86_64/ARM64
