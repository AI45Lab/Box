# CRI Conformance Baseline

This is the versioned `critest` scoreboard for `a3s-box-cri`. It records where
the CRI implementation stands against the upstream Kubernetes CRI conformance
suite so regressions are visible and progress is measurable.

> **Prerequisite:** the CRI server must be reachable over its Unix socket. This
> required a vendored `h2` UDS-authority patch (`third_party/h2`, see the
> workspace `[patch.crates-io]`); before it, `crictl`/`critest` could not connect
> at all — every RPC failed CRI-API validation with `PROTOCOL_ERROR`.

## Latest run

| Field | Value |
|-------|-------|
| Date | 2026-06-15 |
| `critest` | v1.30.1 |
| a3s-box | 2.2.0 |
| Host | Linux KVM node (`/dev/kvm`), Ubuntu 24.04 |
| Result | **73 Passed · 7 Failed · 17 Skipped** (ran 80 of 97 specs, `--ginkgo.skip=portforward`) |

Up from the original **21 Passed / 59 Failed** (2.0.4) and the **44 Passed**
mid-point. With registry mirrors configured (so the e2e webserver/helper images
resolve) plus per-container capabilities, image-defined users/groups, pod DNS,
crash-recovery reaping, standard `/dev` nodes, routable pod IPs, **OOMKilled
detection**, **MaskedPaths**, and an **SPDY port-forward bridge**, the suite
passes 73 of the 80 specs that run. Every one of the 7 remaining failures is
architectural (microVM-per-pod) or environmental — none is a logic defect.
Re-verified on `main` for the 2.2.0 release with **zero regression** from that
release's CRI fixes (lifecycle/stats, streaming cleanup, empty-ref rejection,
`rmi` short-tag resolution).

How it was run:

```bash
A3S_REGISTRY_MIRRORS="registry.k8s.io=k8s.m.daocloud.io,gcr.io=gcr.m.daocloud.io" \
a3s-box-cri --socket /tmp/a3s-box.sock --image-dir ~/.a3s/images \
  --agent-image docker.m.daocloud.io/library/alpine:latest &

critest --runtime-endpoint unix:///tmp/a3s-box.sock \
        --image-endpoint  unix:///tmp/a3s-box.sock \
        --test-images-file test-images.yaml   # alpine + nginx:1.14-2
```

> **Note on the port-forward count:** the `portforward [Conformance]` spec hangs
> on *this* node and, depending on ginkgo's randomized order, can consume the
> suite timeout before later specs run. The 73/82 figure was confirmed by also
> running with `--ginkgo.skip="portforward"` (80 ran → **73 Passed / 7 Failed**),
> which isolates the 7 architectural failures from the 2 portforward specs
> accounted for below.

## What passes (73)

- **Pod + container lifecycle:** `RunPodSandbox` (boots a microVM),
  `PodSandboxStatus`, `CreateContainer`, `StartContainer`, `ContainerStatus`,
  `ListContainers`/`ListPodSandbox`, `StopContainer`, `RemoveContainer` (incl.
  force-removing a running container), `StopPodSandbox`, `RemovePodSandbox`,
  `RuntimeStatus`/`Version`, multi-container pods.
- **Streaming:** `Exec`/`Attach` over SPDY/3.1 (tty on/off, stdin on/off),
  `ExecSync`.
- **Container logs:** writing to `log_path` and `ReopenContainerLog` (rotation).
- **Linux SecurityContext:** `RunAsUser`/`RunAsGroup`/`RunAsUserName` (passwd
  lookup), reject `RunAsGroup` without `RunAsUser`, `SupplementalGroups` (incl.
  image-defined groups + passwd primary gid), `ReadonlyPaths`, **`MaskedPaths`**
  (incl. masking a symlinked entry via `O_PATH|O_NOFOLLOW`), seccomp
  `unconfined`/nil/**RuntimeDefault** (`Seccomp: 2`), `NoNewPrivs`,
  `ReadonlyRootfs`, per-container **capabilities** (default set, add/drop),
  HostPID (the pod's shared VM-wide PID namespace). `/proc` + `/sys` are mounted
  inside the container chroot.
- **Resources:** the container `memory_limit_in_bytes` and `cpu_quota`/`cpu_period`
  are enforced inside the guest by a per-container cgroup v2 (`memory.max` /
  `cpu.max`). The container joins the cgroup from its pre-exec hook, so workers it
  forks are bounded too. An OOM kill is detected via `memory.events` and reported
  as the **`OOMKilled`** exit reason. Real CPU/memory usage is reported through
  `ContainerStats`/`PodSandboxStats` (from the pod VM's shim process).
- **Pod sysctls** (safe), **pod `DNSConfig`** → container `/etc/resolv.conf`,
  standard **`/dev` device nodes** (null/zero/full/random/urandom/tty).
- **Volumes:** read-only and writable mounts (incl. host-path symlink),
  materialized by copying the source into the rootfs.
- **Networking:** published pod ports reachable at a reported pod IP (TSI);
  basic `ImageStatus`/`ListImages`, registry mirrors + digest-pinned identity.

## Remaining gaps (9 failures — all architectural or environmental)

The 2.2.0 run skips the 2 PortForward specs (see the port-forward note above), so
its headline is **7 Failed**; the table below catalogues all 9 specs that do not
pass on the full suite. Mount propagation is nondeterministic across ginkgo's
randomized order — `rprivate` and `non-recursive readonly mounts` trade places
between runs — but both share the one virtio-fs root cause and the count is
unchanged.

| Category | Failing specs | Root cause | Fixable here? |
|----------|---------------|------------|---------------|
| **Mount propagation / readonly** (3) | rshared / rslave / non-recursive readonly mounts | a directory volume is shared as a *single* virtio-fs mount of one host subtree; bidirectional host↔container propagation needs a real shared mount configured at VM boot, and a readonly mount necessarily covers the whole subtree (the test's nested host `tmpfs` is not a separate guest mount, so it cannot stay writable) | ❌ architectural |
| **Host namespaces** (2) | HostNetwork=true, HostIpc=true | each pod is an isolated microVM with its own kernel + namespaces — there is no host network/IPC namespace to share; rejected fail-closed rather than silently mis-running | ❌ architectural |
| **Per-container PID isolation** (1) | ContainerPID | all of a pod's containers share the single VM-wide PID namespace; a per-container PID namespace is not modeled | ❌ architectural |
| **AppArmor enforce** (1) | should enforce a profile blocking writes | the guest kernel has no AppArmor LSM, and CRI passes only the profile *name* — critest loads the profile on the **host** kernel and deletes the source, so the guest has nothing to compile, and the host-compiled binary policy is ABI-tied to the host kernel. CRI's shared-host-kernel AppArmor model does not map onto a separate-kernel microVM | ❌ architectural |
| **PortForward (host network)** (1) | portforward in host network | depends on HostNetwork (above) | ❌ architectural |
| **PortForward** (1) | portforward [Conformance] | the SPDY (`portforward.k8s.io`) bridge is implemented and verified end-to-end (`curl` through `crictl port-forward` to an in-guest listener returns the served bytes), but this test node is itself a production k8s node whose CNI DNATs `127.0.0.1:80` to another pod, hijacking the guest's loopback connect. Passes on a node without that hostPort-80 rule | ⚠️ environmental |

## Stability & hardening

- **Leak-free under churn (validated):** 60 serial create/start/stop/remove pod
  cycles + a 10-pod concurrent burst leave the server's RSS flat after warm-up
  and fds/threads/shims/overlay-mounts/box-dirs all at zero — no per-pod leak.
  `VmManager::destroy` now removes the box working directory for non-persistent
  boxes (it previously leaked one `boxes/<id>` dir per pod, slowing later
  RunPodSandbox until it timed out).
- **Crash recovery (validated):** a hard `SIGKILL` of the CRI leaves 3 orphaned
  microVMs (shims/mounts/box-dirs); on restart the runtime reaps all of them
  (kills the shim, unmounts the overlay, removes the box dir) and marks the
  sandboxes NotReady — zero leftovers after recovery.
- **Graceful shutdown:** on SIGTERM/SIGINT the CRI drains the gRPC server then
  reaps every sandbox VM — no orphaned microVMs/overlays across restarts.
- **Capability ordering:** container capabilities are applied *before* the
  `setuid`, not after (a non-root `setuid` clears `CAP_SETPCAP`).
- **Trusted security envelope:** caller-supplied `A3S_SEC_*` env can no longer
  spoof the runtime's security envelope; the seccomp BPF filter is built before
  `fork` (no async-signal-unsafe allocation in the post-fork child);
  MaskedPaths/ReadonlyPaths mounts are idempotent and path-traversal validated;
  masked symlinks are masked by the entry (`O_PATH|O_NOFOLLOW`), never their
  followed target.
- **ReopenContainerLog is synchronous** (waits for the supervisor to confirm the
  reopen) and uses an exec-stream **flush barrier**: on reopen the supervisor
  sends a flush control to the guest, which drains every buffered output chunk
  to the wire and replies with a flush-ack; the supervisor writes those chunks
  into the old log file and only then reopens. This closes the guest→host
  log-transport ordering gap so pre-rotation output cannot land in the new file.
  (Pty/tty workloads keep the prior best-effort reopen.)

## Methodology

The baseline is captured so each fix is measurable. Re-run after each CRI feature
lands and update the "Latest run" table; the goal is to drive Failed → 0
(excluding documented architectural/environmental items) and graduate
`a3s-box-cri` to a conformant, mature CRI runtime. At 73/82 the remaining
failures are inherent to the microVM-per-pod model (host namespaces, per-container
PID, mount propagation, AppArmor's shared-host-kernel assumption) or specific to
this test node (the port-forward CNI DNAT); none is an outstanding logic defect.
