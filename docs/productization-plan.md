# A3S Box Productization Plan

This plan tracks the gap between the current implementation and the product target:
a production-grade MicroVM runtime with a Docker-like CLI, Kubernetes CRI support,
portable networking, and verifiable confidential-computing workflows.

## Release Gates

### Gate 1: Honest MVP

Goal: make the documented surface match working behavior and protect users from
silent partial implementations.

Acceptance criteria:

- `README.md` marks experimental and planned capabilities clearly.
- CLI rejects unsupported combinations instead of silently degrading.
- Pure unit tests run without host virtualization, network access, or privileged
  socket/mount operations.
- Core path is verified on macOS and Linux: `pull`, `run`, `exec`, `logs`,
  `stop`, and `rm`.

Current notes:

- `run` and `create` now reject unsupported runtime options before creating box
  state or booting a VM: device passthrough, GPU passthrough, AppArmor labels,
  SELinux labels, and custom seccomp profiles fail with contextual errors
  instead of being stored and ignored.
- Supported security options are now preserved through both immediate `run`
  boot and persisted `create`/`start` boot: `cap_add`, `cap_drop`,
  `seccomp=default|unconfined`, `no-new-privileges`, and `--privileged` reach
  `BoxConfig` so guest init can enforce them.
- `run` and `create` now validate initial process overrides before boot or
  persistence: `--workdir` must be an absolute in-box path, `--user root`
  normalizes to UID `0`, and unsupported named users fail instead of being
  stored and ignored.
- Port publishing is now validated before box state is persisted or Compose
  services are accepted: only TCP `host_port:guest_port[/tcp]` mappings are
  supported, `/tcp` is normalized for the runtime, and UDP, host-IP binds,
  single-port shorthand, and ranges fail explicitly.
- `--hostname` and `--add-host` are now validated before boot or persistence:
  hostnames must be DNS-safe names and static host entries must use `HOST:IP`
  with a real IPv4 or IPv6 address.
- `src/cli/tests/core_smoke.rs` now provides an ignored real-runtime smoke
  harness for the core path: `pull`, detached `run`, non-TTY `exec`, `logs`,
  `stop`, and `rm`, with an isolated `A3S_HOME` and configurable image,
  timeout, and cached-image mode.
- `src/cli/tests/command_coverage.rs` now contains only local-state command
  coverage. Host-dependent command tests live in
  `src/cli/tests/host_smoke.rs` and remain ignored by default for explicit
  Linux root, registry, or HVF/KVM smoke runs.

### Gate 2: Runtime Correctness

Goal: make one-container MicroVM execution reliable enough for local development.

Acceptance criteria:

- OCI entrypoint, cmd, env, workdir, user, volumes, exposed ports,
  healthchecks, and stop signals are applied inside the guest or persisted host
  lifecycle state.
- Foreground, detached, PTY, and non-PTY execution have deterministic exit-code
  and log behavior.
- Rootfs preparation supports copy everywhere and overlayfs where available, with
  cleanup tests for failure paths.
- Health checks and restart policy behavior are covered by integration tests.

Current notes:

- Initial process user and workdir overrides now flow through `BoxConfig` for
  immediate `run`, persisted `create`/`start`, and Compose `working_dir`.
  Runtime spec construction prefers explicit config overrides over OCI `USER`
  and `WORKDIR` defaults.
- `create IMAGE -- CMD...` now persists the same command override as `run`, so
  `start` boots created boxes with the user-specified process instead of
  falling back to the image default.
- Guest-init boots now always receive the effective workdir through
  `BOX_EXEC_WORKDIR`, including the default `/workspace`, so PID 1 and the
  launched container entrypoint no longer disagree about the working directory.
- Non-PTY `exec`, PTY `exec -t`, `shell`, and interactive `run -it` now share
  the same user/workdir validation. Guest exec and PTY servers apply numeric
  UID/GID directly before `exec`, so command arguments are preserved and the
  path no longer depends on `su` being installed in the image.
- Interactive `run -it` now boots a control keepalive workload and executes
  the requested command over the guest PTY after control sockets are ready.
  This avoids races where quick interactive commands could exit the VM before
  the PTY session connected.
- Runtime boot now writes explicit host identity configuration into the guest:
  `/etc/hostname`, `/etc/hosts`, bridge-network peer aliases, and user-supplied
  `--add-host` entries are generated from a single validated config path.
  guest-init applies `BOX_HOSTNAME` with `sethostname(2)` before read-only
  remounts and before the container entrypoint starts.
- Container environment merging now uses one ordered helper for env files and
  inline overrides. CLI `--env` continues to override `--env-file`, Compose
  `environment` overrides `env_file`, and guest-init receives merged container
  variables as `BOX_EXEC_ENV_*` instead of dropping them into PID 1 only.
- Foreground and interactive `run` cleanup now persists captured exit codes in
  the box record before marking it stopped, and `wait` prints that recorded code
  instead of always reporting success for stopped boxes.
- Invalid lifecycle mode combinations such as detached TTY runs are now rejected
  before the VM setup phase, so bad CLI combinations do not leave orphaned box
  state or booted VMs.
- Immediate `run` now validates and normalizes restart policies the same way as
  `create`, including `on-failure:N` caps, before booting a VM.
- `stop` now records a deterministic best-effort stop exit code, clears health
  state on stopped boxes, and `start`/`restart`/monitor restarts clear stale
  exit codes before a new run begins.
- Auto-remove cleanup is now shared across `rm`, `stop` for `--rm` boxes,
  foreground/interactive `run --rm`, and state reconciliation for detached
  `--rm` boxes whose shim has already exited. Anonymous OCI volumes, box
  directories, external socket directories, named-volume attachments, and
  network endpoints are cleaned consistently.
- Foreground `run` now has explicit stop reasons for natural process exit,
  Ctrl-C, and VM health-check failure. Ctrl-C and VM health failures have
  deterministic fallback exit codes when the runtime cannot report one, and
  completion messages/`stopped_by_user` state are covered by unit tests.
- Health checks now have shared, unit-tested scheduling and state-transition
  logic. The long-running `monitor` command runs due probes itself, so detached
  boxes no longer depend on short-lived CLI health-check tasks to move from
  `starting` to `healthy`/`unhealthy` and trigger restart policy handling.
- `logs` now treats missing log files for an existing box as empty output
  instead of an error, waits for the first log file when following a running box,
  prefers structured JSON logs when available, and starts `--follow --tail`
  from the end after printing the requested tail so historical lines are not
  duplicated.
- Docker-compatible image `HEALTHCHECK` metadata is now parsed from raw OCI
  config JSON and converted into exec health checks for `run`, cached-image
  `create`, and subsequent `start`/`restart`/monitor boots. CLI
  `--health-cmd` overrides the image default and `--no-healthcheck` persists an
  explicit disable flag.
- Image `STOPSIGNAL` is now treated as the default lifecycle stop signal when
  no CLI `--stop-signal` is provided. The resolved signal is persisted so later
  `stop`, `restart`, and monitor recovery paths use the same semantics.
- `image-inspect` now surfaces the remaining OCI defaults that matter for
  runtime behavior: volumes, stop signal, healthcheck, and ONBUILD triggers in
  addition to entrypoint, cmd, env, workdir, user, exposed ports, and labels.
- Immediate `run` now creates the same rootfs baseline snapshot used by
  `a3s-box diff` as persisted `start` does, so detached boxes started through
  the common local path can report later filesystem additions instead of
  missing their baseline.
- Compose service health checks now use Docker-compatible execution semantics:
  string tests and `CMD-SHELL` run through `sh -c`, `CMD` drops the marker before
  exec, and `NONE` or `disable: true` disables both service and image defaults.
  Unsupported `depends_on` conditions fail during config validation instead of
  being silently treated as `service_started`.
- Compose `up` now resolves named volumes through the same volume store as
  `run`/`create`, attaches them to service boxes, records anonymous OCI volumes,
  and `compose down -v` can remove those named volumes after detaching them.
- Compose service restart policies are now validated and normalized before
  `config` or `up`, so invalid values fail before any service VM is started.
- Named-volume lifecycle persistence now has real HVF coverage: data written
  through a `-v name:/path` mount survives `stop` and `start`, remains after box
  removal, and disappears only after explicit `volume rm`.
- Compose service-specific networks now drive the actual pre-boot network
  connection and persisted `network_name`, so `compose down` disconnects the
  correct network. Health dependency waits are scoped by project labels to avoid
  matching a same-named service in another project.
- Compose `up` now reloads state before appending each newly started service, so
  concurrently updated dependency health does not get overwritten by stale
  in-memory project state before a dependent service is recorded.
- Compose labels are persisted alongside A3S project/service labels, and image
  healthcheck/stop-signal defaults are applied to Compose services when the
  service does not override or disable them.
- Boot failure cleanup now stops any shim that was spawned before readiness,
  stops bridge-network backends, unmounts rootfs providers before removing box
  directories, and removes only the anonymous OCI volumes created by that boot
  attempt so failed restarts do not delete existing container data.
- Compose `up` now rolls back services started during the current invocation
  when a later service fails, including graceful stop, state removal, named
  volume detach, anonymous volume cleanup, box directory cleanup, external
  socket cleanup, and removal of project networks created by the failed run.
- `start`, `restart`, and monitor recovery now share one successful-boot state
  transition. This keeps status, PID, health state, stop signal, exit code,
  restart counter semantics, and anonymous OCI volume tracking consistent for
  both `create`/`start` and automatic restart flows.
- Persisted `create`/`start` and restart recovery now re-establish named volume
  attachments and bridge-network endpoints before booting, and roll those host
  resources back when boot fails. State reconciliation also cleans detached
  dead boxes' named-volume attachments, network endpoints, and stale external
  socket directories while preserving anonymous OCI volumes until `rm`.
- Partial create/run setup now has explicit rollback. If state persistence,
  named-volume attach, or log directory setup fails after a box directory or VM
  has been created, the CLI removes partial state, detaches host resources,
  removes anonymous volumes, deletes the box directory, and destroys any booted
  VM before returning the original error.
- Stop-like CLI paths now share the same stopped/removed resource cleanup
  semantics. `stop`, terminating `kill` signals, state reconciliation, `rm`,
  and Compose teardown all detach named volumes, disconnect bridge endpoints
  using persisted or legacy network metadata, clear external socket directories,
  and remove anonymous OCI volumes only when the box record is removed.
- Guest control-channel commands now use shared runtime socket resolution and
  actionable validation. `exec`, `exec -t`, `shell`, and interactive `attach`
  all report stopped/dead boxes and missing exec/PTY sockets with the resolved
  path plus recovery guidance (`ps` to reconcile state and `restart` when the
  control channel remains missing), instead of generic "not found" errors.
- PTY control paths now retry socket connection while libkrun bridges finish
  coming up, guest init mounts `devpts` before starting the PTY server, and the
  host-side PTY relay no longer keeps a non-cancellable Tokio blocking stdin
  task alive after the PTY child exits.
- The same runtime socket validation now covers the remaining host commands
  that talk to guest control sockets: `top`, `cp`, TEE attestation, sealing,
  unsealing, secret injection, and live cgroup updates. Commands that must
  connect fail with the shared actionable error, while `container-update`
  persists changes and emits the same recovery guidance when live application is
  skipped because the exec socket is missing.
- `pause`, `unpause`, and `wait` now treat stale active state consistently.
  Pause transitions require a live recorded host PID before writing `paused`,
  unpause requires the same before returning to `running`, paused boxes with
  missing or dead PIDs reconcile to `dead`, and `wait` blocks for live paused
  boxes instead of immediately reporting success.
- `ps` and `inspect` now share lifecycle status formatting. Default `ps`
  includes both running and paused boxes, stopped/dead rows show recorded exit
  codes when available, restart counts use neutral `Restarts` wording, and
  `inspect` adds a `status_detail` diagnostic block with state, summary, PID,
  health, exit-code, restart count, and lifecycle recovery hints.
- Active-state semantics are now consistent across the non-socket status
  commands. `info` reports active/running/paused counts, `stats` accepts paused
  boxes and includes a status column, and `system-prune` preserves images used
  by paused boxes while pruning only created, stopped, or dead box records.
- Event and monitor semantics now surface abnormal exits more clearly.
  `events` emits `die` for `running`/`paused`/`created -> dead` transitions and
  `restart` for `dead -> running`, while `monitor` logs restart policy,
  exit-code, and box names during dead-box recovery. Health-triggered restarts
  now reuse the same restart-policy cap checks as dead-box recovery.
- `stop`, terminating `kill` signals, and `restart` now accept paused boxes as
  active lifecycle targets. Paused boxes are resumed before graceful
  termination so signal handlers can run, `kill --signal STOP|CONT` updates the
  persisted paused/running status, and active lifecycle commands fail with
  stale-PID guidance instead of silently rewriting state when the recorded PID
  is missing or dead.
- Forced removal and Compose teardown now use the same active-state model.
  `rm` rejects paused boxes unless `--force` is provided, forced removal treats
  missing active PIDs as stale cleanup, `compose up` refuses projects with
  running or paused services, and `compose down`/rollback stop paused services
  before removing service state and resources.
- Image pruning now has explicit reference-protection semantics.
  `image-prune` protects images referenced by any existing box, including
  created/stopped/dead boxes, while `system-prune --all` protects the images
  referenced by remaining active boxes after inactive boxes are removed. Both
  prune paths also protect normalized Docker Hub aliases such as `alpine:latest`
  and `docker.io/library/alpine:latest`, and non-`--all` pruning is limited to
  dangling local digest references instead of deleting every unused tag.
- Image reference resolution is now shared by tag and remove flows. `tag`
  accepts exact references, normalized Docker Hub aliases, and unambiguous
  digest references as sources. `rmi` resolves the same aliases before removal,
  refuses ambiguous digest matches, and refuses to remove any reference still
  protected by an existing box even when the user asks to ignore missing images.
  `load` now rejects empty explicit tags and falls back from OCI ref annotations
  to digest-only dangling references deterministically.
- Read-only image commands now use the same resolver. `image-inspect`, `save`,
  and `history` accept exact references, normalized Docker Hub aliases, and
  unambiguous digest references. `push` resolves aliases for the local source,
  can push an unambiguous digest query through the resolved stored tag, and
  refuses digest-only images that must be tagged before they can be pushed.
- Runtime cache lookup now applies the same Docker Hub alias model to boot-time
  image access. `pull`, `run`, and `start` can reuse locally loaded or tagged
  images across short names such as `alpine`, tagged names such as
  `alpine:latest`, and fully qualified names such as
  `docker.io/library/alpine:latest`, and cached-image `create` resolves image
  defaults through the shared stored-image resolver.
- Digest queries now behave like local image IDs instead of accidental registry
  names. Read-only image commands, `rmi`, `tag`, runtime cache checks, and
  boot-time image access accept exact or unambiguous `sha256:` prefixes, while
  missing digest-only `pull` requests fail as local cache misses instead of
  attempting to pull `docker.io/library/sha256:<tag>`.

### Gate 3: Docker-Compatible Build MVP

Goal: support a truthful subset of Dockerfile builds.

Acceptance criteria:

- Supported instructions are listed explicitly in docs and CLI help.
- Unsupported instructions fail with contextual errors.
- `RUN` executes in an isolated Linux environment, not directly on the macOS host.
- `--platform` supports one target platform; multi-platform OCI indexes are added
  only after per-platform builds are real.

Current notes:

- Build documentation now describes an explicit Dockerfile subset instead of
  claiming full Dockerfile parity. The supported subset is `FROM` (including
  `scratch`), shell-form `RUN`, shell-form `COPY`/`ADD`, `WORKDIR`, `ENV`,
  `ENTRYPOINT`, `CMD`, `EXPOSE`, `LABEL`, `USER`, `ARG`, `SHELL`,
  `STOPSIGNAL`, `HEALTHCHECK`, `ONBUILD` metadata triggers, and `VOLUME`.
- Unsupported Dockerfile flags and deprecated instructions now fail with
  contextual errors rather than being ignored or approximated. Examples:
  `RUN` exec form, `COPY`/`ADD` JSON form, `COPY --chown`, `ADD --chown`, and
  `MAINTAINER` are rejected.
- ONBUILD triggers inherited from a base image only run when they map to
  metadata-only instructions. Triggers that require build execution context,
  such as `RUN` or `COPY`, fail explicitly until full trigger execution is
  implemented.
- The build engine validates its public `BuildConfig` as well as the CLI:
  multi-platform requests and non-Linux target platforms fail instead of
  silently producing a single-platform or wrong-OS image. The default output
  platform is Linux with the host architecture, not the host OS.
- Dockerfile `RUN` no longer has any silent skip path on unsupported hosts.
  Linux uses isolated `chroot`; macOS fails by default unless the user explicitly
  opts into unsafe host execution with `A3S_BOX_UNSAFE_HOST_RUN=1`.
- Linux `RUN` now has explicit preflight diagnostics for the chroot path:
  non-root builders fail before execution with root-capable builder guidance,
  configured shells must be absolute and present in the rootfs, and the build
  workdir is created before chroot execution so `RUN` honors `WORKDIR`.
- CLI build smoke coverage now includes a pure `FROM scratch` build that verifies
  `COPY`, image metadata, history, save/exported layer contents, and local image
  removal without registry or VM access. An ignored Linux-only smoke harness
  also covers `RUN` through the chroot path when a local Alpine OCI tar and root
  privileges are available.
- The real core lifecycle smoke harness can now preload an OCI image archive into
  its isolated `A3S_HOME` via `A3S_BOX_SMOKE_IMAGE_TAR` or
  `A3S_BOX_TEST_ALPINE_TAR`, so HVF/KVM validation can run offline with the same
  image bits used by host smoke coverage.
- Real macOS HVF smoke now covers `create`/`start` command overrides plus
  detached `run`, foreground non-zero exit-code propagation, non-TTY `exec`,
  `run -it`, `exec -it`, `attach -it`, hostname/static-host injection,
  Docker-like `-p host:guest` publishing with `port` output and host loopback
  HTTP reachability, bridge network endpoint allocation and `/etc/hosts` peer
  discovery, named-volume persistence across `stop`/`start`/`rm`, explicit
  named-volume removal, rootfs `diff`, filesystem `export`, committed-image
  creation and re-run, snapshot create/inspect/restore/remove, restart-policy
  monitor recovery after host-side shim death, `cp`, `top`, `stats --no-stream`,
  pause, unpause, signal-driven pause/resume via `kill`, terminating `kill`,
  `wait`, Compose single-service
  `config`/`up -d`/`ps`/`logs`/`exec`/`down`, Compose multi-service
  `depends_on: service_healthy`, shared named volumes, and `down -v`, logs,
  stop, and removal. Exec and PTY child processes now inherit the container
  environment exposed by guest init, so Compose and `run --env` variables are
  visible to later control-channel commands. Shim stdio is redirected to per-box
  log files and macOS host control sockets use `/private/tmp/a3s-box-sockets` so
  CLI output capture and libkrun Unix-socket bridges do not interfere with
  lifecycle commands.
- The full ignored `core_smoke` suite has been run with an offline Alpine OCI
  archive on macOS HVF and all 14 real-runtime tests passed.
- The ignored `host_smoke` VM command matrix and Compose smoke have now also
  passed on macOS HVF with the same offline Alpine OCI archive. Registry push
  coverage stayed skipped because `A3S_BOX_PUSH_TEST_REF` was not configured.
- `scripts/host-integration-smoke.sh` now provides one macOS/Linux validation
  entrypoint. It runs stub-backed format, clippy, unit, and integration compile
  checks by default, and opt-in `--core`, `--host`, `--linux-run`, and `--cri`
  modes run the ignored HVF/KVM, Linux chroot, and crictl suites with the same
  offline image environment documented in `docs/host-integration.md`. The
  runner requires an OCI archive by default for `--core` and `--host`; live
  registry pulls require explicit `A3S_BOX_ALLOW_REGISTRY_PULL=1`.
- Host-dependent smoke tests now accept `A3S_BOX_HOST_SMOKE_IMAGE` and
  `A3S_BOX_HOST_SMOKE_TIMEOUT_SECS`, so macOS HVF and Linux KVM runs can reuse
  private mirrors, preloaded OCI archives, and slower CI hosts without editing
  test code.

### Gate 4: Kubernetes CRI MVP

Goal: pass a focused kubelet/crictl smoke suite for CRI Pods.

Acceptance criteria:

- `RunPodSandbox`, `CreateContainer`, and non-TTY `StartContainer` map to a real
  workload inside the MicroVM.
- `ExecSync`, streaming `Exec`, `Attach`, logs, stop/remove, and image status work
  through CRI.
- Pod sandbox status reports meaningful network information.
- Multi-container Pod behavior is either implemented or explicitly rejected.

Current notes:

- `CreateContainer` persists CRI command, args, env, workdir, TTY/stdin flags,
  and Linux `run_as_user`/`run_as_group` user overrides. Non-empty CRI image
  references must now exist in the local image store, and the resolved digest
  and OCI layout path are persisted with the container alongside OCI defaults
  for entrypoint, cmd, env, workdir, and user. The pod sandbox must be `Ready`.
- `PodSandboxStatus` now uses the CRI `verbose` channel to report sandbox
  lifecycle state, whether a VM manager is currently present, and the number of
  tracked containers in that sandbox.
- `RunPodSandbox` no longer requires every Pod to carry the
  `a3s.box/agent-image` annotation. The runtime has a default CRI agent image,
  and RuntimeClass-specific image overrides can be configured by runtime handler.
- The CRI proto surface now includes the newer streaming list, stats, runtime
  config, checkpoint, events, metrics, and image streaming RPCs. Stats/runtime
  config return minimal safe data, unsupported checkpointing is explicit,
  container lifecycle events stream from in-process CRI operations, and pod
  sandbox metrics expose lifecycle/VM-presence gauges until runtime resource
  collectors are implemented.
- `Status` now uses the CRI `verbose` channel to report runtime-level sandbox,
  container, VM-manager, and warm-pool counts.
- `ContainerStatus` now uses the CRI `verbose` channel to report container
  lifecycle state, sandbox ownership, VM presence, command/env sizing, and
  stream/TTY flags.
- `StartContainer` now builds a guest exec or PTY request from the persisted CRI
  config, wires stdin containers to the streaming exec/PTY stdin channel,
  verifies the sandbox VM is ready, rejects duplicate starts/restarts, starts the
  workload asynchronously, and records the eventual exit code from a background
  stream supervisor.
- `StartContainer` now fails fast when a container with a non-empty image
  reference lacks resolved local image metadata or its resolved OCI layout path
  has disappeared. The next CRI image-rootfs step is to launch the workload
  against that resolved container image rootfs inside the guest, not only the
  sandbox agent/rootfs image.
- `RunPodSandbox` now mounts a managed CRI container-rootfs directory into the
  sandbox VM, `CreateContainer` extracts the resolved OCI image into a
  per-container rootfs under that directory, and `StartContainer`, `ExecSync`,
  and streaming `Exec` carry the guest-visible rootfs path in the exec/PTY
  request. The guest exec server now chroots into that rootfs on Linux before
  spawning the command. TTY/PTY `StartContainer` and `Exec` carry the same rootfs
  through `PtyRequest`, and the guest PTY server chroots before exec on Linux.
  `Attach` now follows the supervised `StartContainer` stdout/stderr or TTY
  stream instead of opening an unrelated shell, and stdin attach forwards bytes
  to the main workload stdin.
- The guest exec server now speaks the streaming exec wire format used by
  `StartContainer` supervision and emits stdout/stderr chunks while the
  workload is still running, instead of buffering all output until exit. The
  same stream now accepts live stdin data frames plus a stdin-close control frame
  for `stdin_once` attach sessions.
- The async `StartContainer` success path now has unit coverage for the
  `Created -> Running -> Exited` transition sequence.
- The pure CRI create/start smoke path now verifies that `CreateContainer`
  resolves OCI image defaults, prepares a per-container rootfs, and that
  `StartContainer` sends the image command, env, workdir, user, and
  guest-visible rootfs path to the sandbox exec server.
- A pure one-container CRI smoke flow now exercises
  `RunPodSandbox -> PodSandboxStatus -> CreateContainer -> StartContainer ->
  StopPodSandbox -> RemovePodSandbox` against a fake ready VM and fake exec
  server. It verifies bridge network IP status, local-image rootfs handoff,
  container exit supervision, CRI log records, and cleanup of sandbox/container
  state without requiring host virtualization.
- Multi-container Pods are now accepted at `CreateContainer`: each container gets
  its own prepared rootfs, independent workload supervision, attach/log stream,
  and exit status while sharing the pod sandbox VM. `StopContainer` refuses the
  destructive VM-teardown fallback when other containers in the same sandbox are
  still running.
- An ignored `crictl` smoke harness now exists for the real CRI socket path. It
  starts `a3s-box-cri`, drives `crictl runp/create/start/logs/inspectp` for a
  two-container Pod, and cleans up all containers plus the pod sandbox. It
  remains opt-in because it needs `crictl`, image availability, and host
  virtualization.
- `RunPodSandbox` now maps CRI TCP `port_mappings` into Box VM `port_map`
  entries so sandbox-level host ports are not silently dropped. Unsupported
  UDP/SCTP mappings, bind-specific `host_ip` values, and invalid port numbers
  now fail fast during sandbox config conversion.
- `PodSandboxStatus.network` now reports runtime-known Pod IP metadata when an
  integration supplies `a3s.box/pod-ip` and optional comma-separated
  `a3s.box/additional-pod-ips` annotations. Invalid IP annotations fail fast,
  and legacy persisted sandboxes default to an empty network status until real
  CNI/bridge allocation is wired in.
- `RunPodSandbox` now accepts the `a3s.box/network` annotation to join an
  existing A3S bridge network before boot. The runtime preallocates a stable
  sandbox ID, registers a network endpoint in `NetworkStore`, uses the allocated
  IPv4 address as `PodSandboxStatus.network.ip`, and disconnects the endpoint on
  sandbox/container teardown or boot failure. Explicit `a3s.box/pod-ip` values
  must match the allocated address. The CRI runtime service can now use an
  injected `NetworkStore`, keeping unit tests isolated from the user's default
  `~/.a3s/networks.json`, and unit coverage now verifies cleanup for IP
  mismatch, VM acquisition failures, `StopContainer`, `StopPodSandbox`, and
  `RemovePodSandbox`.
- `StartContainer` supervision now writes stdout/stderr events to the CRI
  container log path using Kubernetes CRI log records, including parent
  directory creation and final partial-line flushing.
- Non-interactive CRI `Exec` streaming now bridges to the guest through the
  same frame-based exec client protocol as `ExecSync`, instead of the older
  HTTP-over-Unix-socket stub. Non-TTY `Exec` requests with stdin now switch to
  a streaming exec bridge so HTTP input bytes reach the guest process stdin and
  stdout/stderr chunks are returned while the process runs.
- The guest exec server accepts workload connections concurrently, so one
  long-running CRI container no longer blocks starting another container in the
  same pod sandbox.
- `StopContainer` now reports missing container IDs instead of silently returning
  success, preserves already-exited status instead of overwriting the original
  exit code, and first asks the supervised guest workload to stop through the
  streaming exec control channel so the sandbox VM can remain `Ready`.
- `ContainerStatus` reports `Completed` for exit code 0 and `Error` for non-zero
  exits, with a short message that includes the exit code.
- `ListContainers` has unit coverage for ID, sandbox, state, and label-selector
  filters.
- `ExecSync` and streaming `Exec` reject empty command vectors before attempting
  VM lookup or session registration.
- `Attach` rejects requests with no streams selected, requires the TTY flag to
  match the container configuration, requires CRI stdin to be enabled before
  accepting stdin attach, and `PortForward` rejects empty port lists before
  attempting VM lookup.
- `ExecSync`, streaming `Exec`, and `Attach` now require the target container to
  be running before attempting VM lookup or session registration.
- `UpdateContainerResources` now requires a running container when Linux
  resource changes are requested; only the explicit no-op `linux = None` path is
  accepted for non-running containers.
- `RemoveContainer` is now idempotent for missing containers, but rejects
  deletion of running containers until they have been stopped.
- `PortForward` now requires a `Ready` pod sandbox in addition to a non-empty
  port list before it attempts VM lookup.
- CRI `PortForward` now uses a dedicated guest control socket instead of the
  older broken HTTP-over-Unix-socket stub, and it can bridge one requested
  guest TCP port per streaming session on Unix hosts.
- CRI streaming session URLs now reject operation-kind mismatches without
  consuming the token, and PortForward keeps reading guest responses after the
  client half-closes its upload side.
- CRI streaming server startup now binds before constructing runtime services,
  so ephemeral streaming ports advertise the actual listener address instead of
  returning unusable `:0` URLs.
- CRI streaming session tokens now expire after a short TTL and are pruned during
  registration and connection handling, so stale one-shot URLs cannot remain
  usable indefinitely.
- `StartContainer`, `ExecSync`, streaming `Exec`, `Attach`, `PortForward`, and
  `UpdateContainerResources` now share a VM health gate and fail fast when the
  sandbox VM exists but is not ready.
- `StopPodSandbox` now returns `NotFound` for missing sandboxes and is
  idempotent for already-`NotReady` sandboxes; `RemovePodSandbox` is idempotent
  for missing sandboxes but rejects removal while the sandbox is still `Ready`.
- Stopped or removed CRI sandboxes now destroy any lingering VM manager instead
  of recycling a potentially dirty workload VM back into the warm pool.
- `StopContainer` now has workload-level stop plumbing for `StartContainer`
  processes: the runtime sends a streaming exec cancel control frame, the guest
  kills the child process group and reports exit `137`, and the runtime falls
  back to sandbox VM teardown only when no active workload control exists or the
  stop times out.
- `StopPodSandbox` now fans out workload-level stop controls to all running
  containers in the sandbox before tearing down the shared VM, preserving
  supervisor-reported container exit codes when those workload stops complete
  and only marking remaining non-exited containers with the forced-stop `137`
  fallback.
- `GetContainerEvents` now provides a live stream for container created,
  started, stopped, and deleted events emitted by CRI lifecycle operations,
  including supervisor-reported workload exits.
- `ListMetricDescriptors`, `ListPodSandboxMetrics`, and
  `StreamPodSandboxMetrics` now return a minimal runtime snapshot for pod
  sandbox readiness, VM-manager presence, and tracked container counts instead
  of empty metrics responses.
- `ContainerStats`, `ListContainerStats`, and PodSandbox stats now report
  writable-layer filesystem bytes/inodes from the prepared container rootfs
  when it is available, while CPU and memory remain zero-valued until runtime
  resource collectors are wired in.
- `CreateContainer` now validates CRI mounts instead of silently dropping them:
  read-only private host-path mounts are materialized into the prepared
  container rootfs snapshot and surfaced through `ContainerStatus`, while
  writable, SELinux relabel, non-private propagation, and device mounts fail
  explicitly until real runtime mount plumbing is added.

### Gate 5: Portable Networking

Goal: make published ports and outbound access predictable.

Acceptance criteria:

- Linux and macOS have documented backend selection and diagnostics.
- macOS outbound NAT is implemented or explicitly unsupported by mode.
- Port publishing has end-to-end tests for HTTP services.
- Network policy behavior is enforced, not only stored.

Current notes:

- The ignored real core smoke suite now verifies TSI published ports end to end
  on macOS HVF: `run -p host:guest`, `a3s-box port`, and host loopback HTTP
  reachability all pass against an offline Alpine OCI archive.
- Published-port parsing is shared by CLI, persisted `start`, Compose, and
  macOS netproxy, so unsupported UDP/host-IP/range syntax cannot be silently
  treated as a TCP listener or forwarded to libkrun.
- `a3s-box info` now reports the host platform, VM backend, control channel,
  bridge-network backend, published-port support, and TEE availability. The
  diagnostics make the macOS bridge-mode boundary explicit: netproxy supports
  peer networking and published TCP ports, while outbound NAT remains
  unsupported; Linux passt reports peer networking with outbound NAT.
- The shim now routes macOS bridge-mode published ports through the netproxy
  path only, avoiding duplicate TSI port-map registration when a box combines
  `--network` with `-p`.
- User-defined bridge networks now reject unsupported drivers and unsupported
  strict/custom policy modes before persistence, preventing false isolation
  claims until packet filtering is implemented. All attach paths (`run`,
  `create`/`start`, Compose, and `network connect`) also validate existing
  network records so legacy unsupported network definitions cannot be used
  accidentally.
- `network connect` and `network disconnect` now update the persisted
  `BoxRecord` network mode and network store together for inactive boxes, and
  reject active boxes because live network hot-plug is not implemented.
- `network rm --force` now clears inactive boxes' persisted bridge-network
  configuration along with stored endpoints, while rejecting active boxes
  because live network hot-unplug is not implemented.
- `create --network` now validates that the target network exists before
  persisting box state, so invalid network references fail early instead of
  surfacing only at `start` time.
- The ignored real core smoke suite now verifies bridge network endpoint
  allocation and peer `/etc/hosts` discovery across two macOS HVF boxes, plus
  pre-start `network connect`/`disconnect` persistence, force-removal state
  cleanup, and active hot-plug rejection.

### Gate 6: Confidential Computing

Goal: separate simulated TEE development from hardware-backed production claims.

Acceptance criteria:

- Simulated attestation is clearly marked in CLI and audit output.
- SEV-SNP hardware attestation, RA-TLS, sealing, and secret injection have an
  automated or documented hardware acceptance suite.
- TDX remains hidden or documented as planned until runtime support exists.
- KDS/KBS network-dependent tests are isolated from pure unit tests.

### Gate 7: Cross-Platform Packaging

Goal: make installation behavior match platform claims.

Acceptance criteria:

- macOS and Linux packages ship matching binaries and guest assets.
- Windows clearly chooses one path: native WHPX, with command support matrix
  and tests. WSL is not a runtime dependency.
- Version numbers and package metadata are aligned across workspace crates,
  Homebrew, winget, and docs.

Current notes:

- Windows packaging now chooses the native WHPX path explicitly. The Windows
  release package ships `a3s-box.exe`, `a3s-box-shim.exe`, the Linux
  `a3s-box-guest-init` binary that runs inside the MicroVM, and `krun.dll`.
- The Windows package requires Windows Hypervisor Platform and does not require
  WSL. Host/guest control uses Windows named pipes where implemented.
- Winget metadata advertises native WHPX support and declares
  `HypervisorPlatform` as the Windows feature dependency.

## Immediate Development Queue

1. Run `scripts/host-integration-smoke.sh --core --host` on both macOS HVF and
   Linux KVM hosts with the same offline Alpine OCI archive, then record the
   exact host/image/test metadata in the release notes.
2. Run `sudo -E scripts/host-integration-smoke.sh --linux-run --no-pure` on a
   root-capable Linux host with a local Alpine OCI tar. The Linux chroot path
   now has root/shell/workdir preflight checks, but still needs real Linux
   execution validation in this branch.
3. Run and harden the opt-in kubelet/crictl CRI smoke suite on a host with
   `crictl`, image availability, and microVM support. Pure unit coverage now
   verifies one-container and multi-container CRI lifecycle paths through a fake
   ready VM and exec server, and `src/cri/tests/crictl_smoke.rs` provides the
   real CRI socket harness.
4. Replace macOS host-side Dockerfile `RUN` execution with an isolated execution
   path. It now fails by default and requires `A3S_BOX_UNSAFE_HOST_RUN=1` for
   explicit unsafe local experiments.
5. Add a Windows/WHPX command support matrix and make unsupported Windows
   commands hidden or explicitly documented.
