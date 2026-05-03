# a3s-box Docker/CRI Parity Development Plan

Date: 2026-05-03

This plan tracks the remaining work before a3s-box can be positioned as either a
Docker replacement or a Kubernetes CRI runtime. Today a3s-box should be described
as a Docker-style CLI plus OCI image handling plus a MicroVM-backed runtime with
partial CRI support. It is not yet a complete Docker Engine replacement, and it
is not yet validated as a production Kubernetes runtime.

## Goals

1. Provide Docker-compatible workflows for common image, container, registry,
   logs, exec, networking, volume, stats, and Compose use cases.
2. Provide a CRI v1 RuntimeService and ImageService that can pass crictl smoke
   tests and run a controlled k3s node end to end.
3. Keep registry behavior configurable through `~/.a3s/config.json`, including
   Docker Hub and user-provided registries.
4. Avoid claiming Docker or Kubernetes runtime replacement status until the
   acceptance gates in this document pass.

## Current Baseline

- Docker-style CLI commands exist for several image, container, and system
  workflows, but the command surface and behavior are still incomplete.
- Registry login supports Docker Hub style accounts and configurable registry
  endpoints through `~/.a3s/config.json`.
- CRI ImageService supports image pull/list/status/remove behavior with auth and
  platform-aware image pulls.
- CRI RuntimeService supports basic pod sandbox and container lifecycle flows.
- CRI sandbox networking has a default network option, endpoint/IP tracking,
  cleanup, and NetworkReady condition reporting.
- CRI container status can preserve mounts, devices, and Linux config data.
- CRI stats RPCs exist, but resource metrics are not yet backed by real VM or
  guest measurements.
- CRI container start can write synchronous command output to the CRI log path.
- a3s-box has not yet been proven as the runtime endpoint for OrbStack-managed
  k3s. That environment needs a separate feasibility check because OrbStack may
  hide or own the kubelet runtime configuration.

## Docker Replacement Gaps

### P0: Core Docker Workflows

- Long-running container execution model: detached containers must remain
  observable and controllable after `run -d`.
- Container lifecycle parity: `run`, `create`, `start`, `stop`, `restart`,
  `kill`, `rm`, `ps`, and exit code reporting need Docker-compatible behavior.
- Logs and attach: `logs -f`, timestamps, tailing, stdout/stderr separation,
  attach, and stream reconnection need end-to-end support.
- Exec: `exec`, interactive TTY, environment override, working directory, user,
  and exit status behavior need implementation against running workloads.
- Inspect and events: `inspect`, status fields, labels, mounts, network
  settings, state transitions, and event streaming must be compatible enough for
  tools that parse Docker output.
- Mount and volume execution: bind mounts, named volumes, read-only mounts,
  tmpfs, propagation flags, and cleanup semantics must be applied inside the
  runtime, not only recorded in metadata.
- Networking: bridge network behavior, DNS, published ports, container-to-host
  reachability, container-to-container reachability, and network removal safety
  need Docker-compatible semantics.
- Image operations: `pull`, `push`, `tag`, `inspect`, `history`, `save`, `load`,
  digest handling, multi-platform selection, and private registry auth need
  compatibility tests.
- Resource controls and stats: CPU, memory, pids, block I/O, and network stats
  must be enforced or clearly rejected and reported.
- Signals and restart policy: signal delivery, graceful timeout handling, and
  restart policies need runtime support.

### P1: Docker Ecosystem Compatibility

- Docker Engine API socket: provide enough `/var/run/docker.sock` API
  compatibility for Docker SDKs, Testcontainers, Compose, and common developer
  tools.
- Docker Compose: support common Compose service, environment, bind mount,
  named volume, port, network, dependency, and healthcheck behavior.
- Build support: support Dockerfile builds through BuildKit integration or a
  compatible builder path, including multi-stage builds and build secrets.
- Credential helpers: support Docker credential helper discovery and fallback
  behavior across macOS, Linux, and Windows.
- Contexts and remote endpoints: support Docker context-like workflows where
  they are useful for local and remote a3s-box runtimes.
- Healthchecks: execute, store, and report healthcheck status in Docker and CRI
  compatible forms.

### P2: Advanced Docker Surface

- Logging drivers, network drivers, and volume driver plugin compatibility.
- Remote daemon TLS compatibility and daemon-level authorization controls.
- Advanced security features: seccomp, AppArmor, SELinux, user namespaces,
  capabilities, readonly rootfs, and device policy parity.
- Swarm compatibility is not a near-term target unless product direction
  changes.

### Docker Acceptance Gates

a3s-box can be described as a practical Docker replacement only after these
checks pass on macOS, Linux, and Windows where the platform supports the runtime:

- `docker run -d nginx`, `ps`, `logs -f`, `exec`, `stop`, and `rm` equivalents
  work consistently through a3s-box.
- Common Docker CLI scripts can use a3s-box without output-shape surprises for
  `inspect`, `ps`, `images`, `pull`, `tag`, `push`, `save`, and `load`.
- A Docker SDK smoke test and a Testcontainers smoke test pass through the
  a3s-box Engine API socket.
- A representative Docker Compose app with ports, environment variables, bind
  mounts, named volumes, and multiple services starts and tears down cleanly.
- `stats`, `events`, restart policy, signal handling, and logs behave
  predictably for long-running containers.

## Kubernetes CRI Gaps

### P0: crictl Runtime MVP

- Stable sandbox image path: define, build, publish, and configure a known-good
  pause/sandbox image for CRI pod sandboxes.
- crictl harness: add repeatable local smoke tests for `info`, `pull`, `runp`,
  `create`, `start`, `logs`, `stats`, `stop`, and remove flows.
- RunPodSandbox boot path: verify that a sandbox reaches ready state and has a
  stable network identity.
- Long-running container lifecycle: containers started through CRI must keep
  running, be inspectable, be stoppable, and be removable.
- Continuous CRI logs: log files must update while a workload runs, with CRI log
  format, timestamp handling, and stdout/stderr stream identity.
- Mount execution: CRI mounts must be applied inside the guest/runtime session,
  including read-only and basic propagation expectations.
- Stop and remove idempotency: CRI calls must tolerate kubelet retry patterns
  and return compatible errors.
- Private image pull: ImageService auth config and configured registries must
  pass private registry pull tests.

### P1: k3s Single-Node Runtime

- Kubelet endpoint integration: provide clear socket startup and kubelet flags
  for `--container-runtime-endpoint` and `--image-service-endpoint`.
- CNI and pod networking: pod IP assignment, DNS, service reachability, and
  cleanup must work with k3s networking.
- RuntimeConfig: implement pod CIDR and network runtime config behavior expected
  by kubelet.
- Real stats: pod and container CPU, memory, filesystem, network, writable
  layer, and image filesystem stats need real VM or guest data.
- SecurityContext execution: runAsUser, runAsGroup, supplementalGroups,
  readonlyRootfs, capabilities, privileged mode, devices, and seccomp/AppArmor
  behavior need implementation or explicit unsupported errors.
- Exec, attach, and port-forward: kubelet streaming workflows need an
  implementation compatible with Kubernetes clients.
- ConfigMap, Secret, projected, and service account token mounts must work in
  running pods.
- Image garbage collection and container garbage collection must behave safely
  under kubelet pressure.
- Runtime errors must map to CRI-compatible gRPC status codes and messages.

### P2: Broader Kubernetes Readiness

- RuntimeClass integration and scheduling labels.
- Node pressure and eviction signal integration.
- Checkpoint/restore support if required by target workloads.
- Conformance-style regression coverage for CRI v1 fields currently ignored by
  a3s-box.
- Multi-architecture validation for supported host platforms.

### CRI Acceptance Gates

a3s-box can be described as a Kubernetes CRI runtime only after these checks
pass:

- `crictl info`, `pull`, `runp`, `create`, `start`, `logs`, `stats`, `stop`,
  and remove commands pass against a local a3s-box CRI socket.
- A controlled k3s node can be started with a3s-box as both runtime and image
  service endpoint, and the node reaches `Ready`.
- BusyBox, nginx, and at least one multi-container pod run successfully.
- Pods using ConfigMap, Secret, service account token, bind mounts, environment
  variables, and private registry images run successfully.
- Pod networking supports pod IP reachability, DNS, ClusterIP service access,
  and clean teardown.
- `kubectl logs`, `exec`, attach, and port-forward work for representative
  workloads.
- kubelet restarts do not orphan sandboxes, containers, network endpoints, or
  log files.

## OrbStack k3s Test Position

OrbStack-managed k3s can be used for a3s-box CRI testing only if OrbStack exposes
or allows replacement of the kubelet container runtime endpoint and the a3s-box
CRI socket can be mounted into the environment where kubelet runs. If OrbStack
does not expose that configuration, CRI validation should happen in a controlled
k3s node first, such as a Linux VM, Lima VM, or dedicated test host.

Required OrbStack investigation:

1. Locate the kubelet process and confirm whether runtime endpoint flags are
   user-configurable.
2. Confirm whether a Unix socket from the host can be made visible to kubelet.
3. Confirm whether kubelet restart/configuration changes survive OrbStack
   lifecycle operations.
4. Only after those checks pass, run the same CRI acceptance suite against
   OrbStack k3s.

## Integration Test Plan

### Phase A: Local crictl Socket

- Start `a3s-box-cri` with an explicit socket, sandbox image, and sandbox
  network.
- Run crictl against the a3s-box runtime and image service endpoints.
- Store pod sandbox and container fixtures under a repeatable integration test
  directory.
- Gate each new CRI feature with at least one crictl command sequence.

### Phase B: Controlled k3s Node

- Start k3s with a3s-box as the CRI runtime endpoint and image service endpoint.
- Verify node readiness, image pulls, pod lifecycle, logs, exec, mounts, and
  networking.
- Capture startup instructions and known host requirements in documentation.

### Phase C: OrbStack Feasibility

- Verify runtime endpoint replacement feasibility.
- If feasible, run the Phase B test suite against OrbStack k3s.
- If not feasible, document OrbStack as unsuitable for CRI replacement testing
  until it exposes the needed runtime configuration.

### Phase D: Regression Automation

- Add fast unit tests for metadata, config, auth, and CRI request translation.
- Add integration tests that can run locally with crictl.
- Add a gated CI job for the heavier k3s end-to-end suite.

## Milestones

### Milestone 1: CRI crictl MVP

- Add crictl fixture and smoke-test harness.
- Finalize sandbox image selection and configuration.
- Implement long-running CRI container lifecycle behavior.
- Implement continuous CRI logs.
- Verify stop/remove idempotency.

Done when the Phase A crictl suite passes locally.

### Milestone 2: k3s Single-Node MVP

- Implement kubelet endpoint startup documentation.
- Make pod networking usable with k3s.
- Apply mounts and basic Linux security context settings in the runtime.
- Back CRI stats with real measurements.
- Validate ConfigMap, Secret, service account token, and private registry pods.

Done when the Phase B k3s node reaches `Ready` and the CRI acceptance workloads
pass.

### Milestone 3: Docker CLI MVP

- Complete detached container lifecycle support.
- Complete logs, attach, exec, inspect, events, and stats for long-running
  workloads.
- Complete common image operations and private registry workflows.
- Complete basic port publishing, bind mounts, and named volumes.

Done when the Docker CLI acceptance gates pass without relying on Docker Engine.

### Milestone 4: Docker Engine API and Compose

- Implement the Engine API subset required by Docker SDKs, Testcontainers, and
  Docker Compose.
- Add compatibility tests for representative SDK, Compose, and Testcontainers
  workflows.
- Fill gaps in inspect response shapes and event streams found by ecosystem
  tools.

Done when SDK, Compose, and Testcontainers acceptance tests pass through the
a3s-box socket.

### Milestone 5: Build and Advanced Ecosystem

- Add Dockerfile build support through BuildKit integration or a compatible
  builder path.
- Add advanced credential helper, context, logging, network, and volume driver
  support where needed.
- Harden cross-platform packaging and daemon startup behavior.

Done when a3s-box can support common developer workflows from image build to
local multi-service execution without Docker Engine.

## Risk Register

- MicroVM-backed execution is intentionally different from Docker's container
  model, so mounts, exec, networking, and security context behavior may require
  guest agent work rather than metadata-only changes.
- Docker Engine API compatibility is a large surface area. The practical target
  should be ecosystem-driven compatibility, not every obscure daemon endpoint at
  once.
- BuildKit-level Dockerfile compatibility is its own major subsystem.
- k3s and kubelet retry aggressively, so CRI idempotency and error mapping are
  as important as the happy path.
- OrbStack may not expose enough kubelet control to test a custom CRI runtime.
- Real stats and resource enforcement require platform-specific implementations
  across macOS, Linux, and Windows.

## Definition of Done

- Do not describe a3s-box as a Docker replacement until Docker CLI, Engine API,
  Compose, and ecosystem acceptance gates pass.
- Do not describe a3s-box as a Kubernetes CRI runtime replacement until crictl
  and controlled k3s acceptance gates pass.
- Do not describe OrbStack k3s as validated until the OrbStack-specific endpoint
  replacement test passes.
- Every completed capability should include focused tests, documentation updates
  where behavior is user-visible, and a dedicated commit.
