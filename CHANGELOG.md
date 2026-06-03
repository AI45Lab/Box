# Changelog

All notable changes to A3S Box will be documented in this file.

## [Unreleased]

### Added
- Registry mirrors: `A3S_REGISTRY_MIRRORS=host=mirror,...` pulls image content
  from a configured mirror while preserving the canonical image identity in the
  store (e.g. fetch `registry.k8s.io`/`gcr.io` images via an accessible mirror).
- CRI `SecurityContext.no_new_privs`: the guest sets `PR_SET_NO_NEW_PRIVS`
  before exec, so a setuid/setgid or file-capability binary can no longer raise
  the container process's privileges (privileged containers opt out).
- CRI `SecurityContext.readonly_rootfs`: the guest remounts the container root
  read-only before exec (writes to `/` fail), while `/proc`, `/sys`, and inner
  mounts stay writable.
- CRI pod DNS config: a pod's `DNSConfig` (servers, searches, options) is
  captured on the sandbox and rendered into each container's `/etc/resolv.conf`
  (falling back to the default when unset).
- Image-defined supplemental groups: when a container runs as a specific user,
  the guest applies the groups that user belongs to per the image's `/etc/group`
  (runc-style initgroups) and defaults the primary gid to the user's
  `/etc/passwd` group when no `RunAsGroup` is set.
- `import` creates a single-layer image from a rootfs tarball (`.tar`/`.tar.gz`),
  with Dockerfile-style `--change` directives (CMD/ENTRYPOINT/ENV/WORKDIR/USER/
  EXPOSE/LABEL/VOLUME) and `--message` — matching `docker import`.
- `images --filter` supports `reference=<glob>` and `label=<key>[=<value>]`
  (repeatable; all must match), matching common `docker images --filter` usage.
- `build --target <stage>` builds only up to the named (or indexed) stage of a
  multi-stage build and emits that stage's image; later stages are not executed.
- `build --no-cache` disables the layer build cache so every layer is rebuilt.
- `inspect <name>` is now polymorphic: it resolves a container first, then falls
  back to an image (matching `docker inspect`), instead of only handling boxes.
- `ADD --chown=user[:group]` is now supported (was "not supported yet").
- COPY/ADD `--chown` now also resolves named users/groups from the rootfs
  `/etc/passwd`/`/etc/group`, not only numeric IDs.
- `.dockerignore` support: a context-root `.dockerignore` now excludes matching
  paths from `COPY`/`ADD` (comments, blank lines, `!` negation with last-match-
  wins, and `?`/`*`/`**` globs). Previously `COPY . /app` copied everything —
  `.git`, `node_modules`, `.env` secrets — into the image; those are now kept
  out, matching Docker. (Applies to the build context, not `COPY --from`.)
- Layer-level build cache (Docker/BuildKit-style): `a3s-box build` reuses
  previously built layers across builds via a rolling chain key over each
  instruction (and, for `COPY`/`ADD`, the content of the source files), so an
  unchanged prefix is reused and a changed instruction/input rebuilds from that
  layer on. Cached at `~/.a3s/buildcache`, size-capped (default 2 GiB,
  `A3S_BOX_BUILDCACHE_MAX_BYTES`; oldest evicted first), best-effort.
- CRI `ReopenContainerLog` flush boundary: log rotation now asks the guest to
  flush and drains every buffered output chunk into the old log file (stopping
  at a flush-ack marker added to the exec protocol) before reopening, so output
  produced before the rotation cannot leak into the new file.

### Security
- Host network/IPC namespaces are now rejected fail-closed: a pod or container
  requesting `HostNetwork`/`HostIpc` (or a host user namespace) —
  `NamespaceMode::NODE` — gets a clear `Unimplemented` error instead of being
  silently run fully isolated. A microVM-per-pod has no host network or IPC
  namespace inside the guest, so silently accepting gave the workload wrong
  (fail-open) semantics. `HostPID` is accepted (the pod's shared VM-wide PID
  namespace satisfies it), as are `POD`/`CONTAINER`.
- AppArmor: a requested Localhost profile (modern `apparmor` SecurityProfile or
  the deprecated `apparmor_profile` string) is now validated against the host's
  loaded profiles and the container is rejected when the profile is not loaded,
  instead of being silently ignored. The microVM cannot enforce an in-guest LSM
  profile, so a loaded profile is accepted with a warning that it is not
  enforced. Passes critest "should fail with an unloaded profile".
- Non-privileged containers are now restricted to the runtime default
  capability set (e.g. no `CAP_NET_ADMIN`/`CAP_SYS_ADMIN`), adjusted by the
  container's `add`/`drop` capabilities; privileged containers keep the full
  set. Previously every container ran as full-capability root, so a
  non-privileged container could perform privileged operations (e.g. create a
  network bridge). The guest applies an exact keep-set via `capset` + bounding
  drop before exec.

### Added
- Pod port reachability: a port mapping with only a container port now publishes
  it on the same host port (Docker/containerd style), and a default (TSI) pod
  that publishes ports reports `127.0.0.1` as its pod IP — TSI binds
  `0.0.0.0:<port>` and forwards to the guest, so `podIP:<containerPort>` is
  genuinely reachable from the node. Passes the port-mapping and
  multi-container networking conformance specs. (Single-node reachability via
  the node loopback; not a unique cluster-routable pod IP, and concurrent pods
  publishing the same port still contend for the host port.)
- Crash recovery: on startup the CRI reaps sandbox microVMs orphaned by a
  previous crash/SIGKILL — it kills the leftover `a3s-box-shim` (matched by the
  box id in its argv), unmounts its overlay, and removes its box directory —
  instead of leaking the VM, mount, and disk across restarts. A graceful
  shutdown already reaps VMs, so this is a no-op then.

### Fixed
- `save`/`load` now round-trip the image tag: `save` stamps the image reference
  into the OCI `index.json` `org.opencontainers.image.ref.name` annotation, so
  `load` restores the tag (e.g. `rt:9`) instead of importing the image untagged
  (by digest only). `load` already read the annotation; `save` never wrote it.
- Image references with a purely numeric tag and no registry (`redis:7`,
  `node:18`, `postgres:16`, `ubuntu:24`) were mis-parsed: the numeric tag was
  treated as a registry port and dropped, so the reference resolved to the
  `:latest` tag instead. A colon with no `/` is always a tag (a bare
  `registry:port` with no repository is not a valid reference), so numeric tags
  now parse correctly — affecting pull, run, and `images` display.
- `COPY`/`ADD` now preserve symlinks instead of following them: a copied symlink
  (e.g. a shared library `libfoo.so -> libfoo.so.1`, or any `node_modules`/
  `/usr/lib` link) was dereferenced into a duplicate regular file, losing the
  link and bloating the image. Symlinks (including symlink-to-dir and dangling
  links) are now stored as symlink layer entries, matching Docker.
- Multi-stage `COPY --from=<stage> /abs/path` (and any absolute COPY/ADD source)
  was broken: the absolute source was resolved against the host root instead of
  the source stage's rootfs (`Path::join` discards the base for an absolute
  argument), failing with "source not found". Absolute sources are now resolved
  relative to the context/stage, so multi-stage builds work.
- Multi-layer image corruption in `a3s-box build`: layer digest and size were
  computed before the gzip stream was flushed to disk (the tar builder owning
  the encoder was dropped only at function end), so every layer recorded the
  same digest — the hash of the partial 10-byte gzip header — and `size` 10.
  Manifests referenced one wrong digest for every layer and the content-addressed
  blob store collapsed all layers into the first; single-layer images happened
  to round-trip, hiding the bug. The encoder is now finished before hashing.
- Container `/dev` now contains the standard device nodes (`null`, `zero`,
  `full`, `random`, `urandom`, `tty`), created in the guest before the container
  starts. Workloads that need them — e.g. Apache httpd, which reads
  `/dev/urandom` to seed its RNG and otherwise aborts with `AH00141` — now run.
  Fixes the multi-container exec/log conformance specs.
- The container log file is now created eagerly at `StartContainer` (instead of
  lazily when the first output arrives), so a caller that opens the log
  immediately after start — e.g. `ReopenContainerLog`, or before the container
  has produced any output — finds it. Fixes the critest "reopening container
  log" conformance spec.
- CRI image identity now follows the digest, matching real runtimes:
  - `ListImages`/`ImageStatus` coalesce references by content digest, so an
    image with multiple tags appears once with all `repo_tags`.
  - `ImageStatus` resolves an image by exact reference, image id (digest), a
    `name@sha256:...` digest pin, or an unnormalized name (e.g. a tagless name
    defaulting to `:latest`).
  - `RemoveImage` accepts an image id (digest), not just a tag/reference.
  - `PullImage` returns the content digest as `image_ref`, so different tags of
    the same image dedupe to one image id.
  - An image pulled by digest (`repo@sha256:...`) is reported with that
    reference as a `repo_digest` and empty `repo_tags` (digest pins have no tag).
  - `ImageStatus`/`ListImages` surface the image's configured user as `uid`
    (numeric `uid`/`uid:gid`) or `username` (named user), from the OCI config.
  - `CreateContainer` resolves the image the same way (exact ref, digest id,
    `name@sha256:` pin, or unnormalized name), via a shared `ImageStore::resolve`
    — so a container referencing an image by an untagged name now starts.
  - The full critest Image Manager conformance suite now passes (7/7): public
    image pull/remove by tag, without tag, and by digest; image status across
    all reference kinds; non-empty uid/username; and the listImage image and
    repoTag counts.

## [2.0.6] — 2026-06-01

### Added
- CRI Linux SecurityContext: `RunAsUser`/`RunAsGroup`/`RunAsUserName` (passwd
  lookup), `SupplementalGroups` (setgroups), `MaskedPaths`/`ReadonlyPaths`, and
  the `RuntimeDefault` seccomp profile (default BPF filter → `Seccomp: 2`).
- `/proc` and `/sys` are now mounted inside the container chroot, so in-container
  reads of `/proc/self/*` and `/sys/class/*` work like any container runtime.
- Pod sysctls: safe sysctls from `PodSandboxConfig` are applied in the guest at
  VM boot.
- Writable CRI volume mounts (materialized by copy into the rootfs; read-only
  and host-path-symlink volumes included).
- Graceful shutdown: on SIGTERM/SIGINT the CRI reaps every sandbox VM and
  unmounts its overlay, so microVMs/overlays no longer orphan across restarts.

### Fixed
- Corrected the CRI v1 `LinuxContainerSecurityContext` proto field numbers to the
  official spec (kubelet/critest can now decode security-context pods).
- `RemoveContainer` force-removes a running container (stops it first), per the
  CRI contract.
- Security & safety hardening from an adversarial code review (16 confirmed
  findings): a container image/pod env can no longer spoof the `A3S_SEC_*`
  security envelope (privilege escalation); the seccomp BPF filter is built
  before `fork` (no async-signal-unsafe allocation in the post-fork child —
  a musl malloc-deadlock risk); MaskedPaths/ReadonlyPaths mounts are idempotent
  (no per-exec mount leak); MaskedPaths/ReadonlyPaths/sysctl names are
  path-traversal validated; plus panic/leak/non-Linux-build fixes.
- `ReopenContainerLog` is now synchronous (waits for the supervisor to reopen the
  log) — correct CRI semantics for log rotation.

### Conformance
- `critest` v1.30.1: 44 of 82 runnable specs pass (up from 21), with no
  regressions. Remaining failures are environmental (registry egress),
  guest-kernel-limited (bridge/mqueue/AppArmor), architectural (mount
  propagation), or test-image artifacts — see `docs/cri-conformance.md`.

## [2.0.5] — 2026-05-31

### Added
- CRI `exec` works end to end over the Kubernetes SPDY/3.1 `remotecommand`
  protocol — `kubectl exec` / `crictl exec` (non-TTY and TTY), stdin, stdout,
  stderr, and exit-code propagation. Implemented in `cri/src/spdy.rs`; the two
  critest exec conformance specs now pass.

### Fixed
- CRI server is now reachable by standard gRPC clients (`crictl`, the kubelet,
  `critest`) over its Unix domain socket. `grpc-go >= 1.57` sends the
  percent-encoded socket path as the HTTP/2 `:authority`, which upstream `h2`
  rejected with a `PROTOCOL_ERROR` stream reset before any CRI RPC ran. A
  vendored `h2` patch (`third_party/h2`, wired via `[patch.crates-io]`) relaxes
  authority validation for UDS-style values; the full pod+container lifecycle
  (`runp`/`create`/`start`/`ps`/`stop`/`rm`/`stopp`/`rmp`) now works end to end.

### Changed
- Split the 7732-line `cri/src/runtime_service.rs` into a focused
  `runtime_service/` module (no behavior change).

## [2.0.4] — 2026-05-09

### Changed
- README and product documentation now describe the verified local CLI runtime,
  image lifecycle, networking, Compose subset, TEE boundaries, and experimental
  CRI surface without Docker/Kubernetes overclaiming.

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
