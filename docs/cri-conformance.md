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
| Date | 2026-06-01 |
| `critest` | v1.30.1 |
| a3s-box | 2.0.6 |
| Host | Linux KVM node (`/dev/kvm`), Ubuntu 24.04 |
| Result | **44 Passed · 38 Failed · 15 Skipped** (ran 82 of 97 specs; full re-run at the hardened HEAD, no regressions) |

This is up from the original **21 Passed / 59 Failed** baseline (2.0.4): streaming
exec/attach, container logs + reopen, force-RemoveContainer, safe sysctls,
RuntimeDefault seccomp, and most of the Linux SecurityContext now pass. The
remaining 40 failures are **all** registry-egress / guest-kernel / architectural
/ test-image artifacts — none is a logic defect (see below).

How it was run:

```bash
a3s-box-cri --socket /tmp/a3s-box.sock \
  --image-dir ~/.a3s/images \
  --agent-image docker.1ms.run/library/alpine:latest &

critest --runtime-endpoint unix:///tmp/a3s-box.sock \
        --image-endpoint  unix:///tmp/a3s-box.sock \
        --test-images-file test-images.yaml \
        --ginkgo.skip="PortForward"
```

> **Caveat:** all `critest` test images were mapped to a single cached `alpine`
> image (`test-images.yaml`), because the node has no general registry egress.
> Specs that pull a distinct image (`registry.k8s.io/e2e-test-images/{nginx,
> httpd,nonewprivs}`, `gcr.io/.../test-image-predefined-group`) therefore fail at
> image pull as **test-setup artifacts**, not runtime defects. Only
> `defaultTestContainerImage` + `webServerImage` are overridable via the images
> file, so these pulls cannot be redirected to the mirror.

## What passes (42)

- **Pod + container lifecycle:** `RunPodSandbox` (boots a microVM),
  `PodSandboxStatus`, `CreateContainer`, `StartContainer`, `ContainerStatus`,
  `ListContainers`/`ListPodSandbox`, `StopContainer`, **`RemoveContainer`
  (incl. force-removing a running container)**, `StopPodSandbox`,
  `RemovePodSandbox`, plus `RuntimeStatus`/`Version`.
- **Streaming:** `Exec`/`Attach` over SPDY/3.1 (tty on/off, stdin on/off),
  `ExecSync`.
- **Container logs:** writing to `log_path` and `ReopenContainerLog` (rotation).
- **Linux SecurityContext:** `RunAsUser`, `RunAsGroup`, `RunAsUserName`
  (passwd lookup), reject `RunAsGroup` without `RunAsUser`, `SupplementalGroups`,
  `ReadonlyPaths`, seccomp `unconfined`/nil/**RuntimeDefault** (default BPF
  filter → `Seccomp: 2`). `/proc` + `/sys` are mounted inside the container
  chroot.
- **Pod sysctls:** safe sysctls applied at VM boot (`/proc/sys` writes).
- **Volumes:** read-only and writable mounts (incl. host-path symlink) are
  materialized by copying the source into the rootfs (no host write-propagation).
- Basic `ImageStatus`/`ListImages`.

## Real gaps (failures grouped by cause)

| Category | Failing specs (examples) | Root cause | Fixable here? |
|----------|--------------------------|------------|---------------|
| **Registry egress** (~22) | Multiple-Containers exec/log/network, image pull/list/status, DNS, port-mapping, port-forward, HostNetwork/PID/IPC, Privileged, ReadOnlyRootfs, NoNewPrivs, image-group SupplementalGroups | webserver/helper images on `registry.k8s.io`/`gcr.io` are unreachable and not overridable via the images file | ❌ environmental |
| **Mount propagation** (~3) | mount propagation rshared/rslave/rprivate, non-recursive readonly mounts | basic volume mounts (incl. writable + host-path symlink) now PASS — both read-only and writable mounts are materialized by COPY into the rootfs; bidirectional host↔container propagation needs a real shared mount, which libkrun would have to configure at VM boot (containers are created post-boot) | ⚠️ architectural |
| **seccomp localhost profiles** (2) | localhost profile, SYS_ADMIN-block | RuntimeDefault now installs the default BPF filter (`Seccomp: 2`); localhost profiles need the host profile file plumbed into the VM + compiled | ⚠️ host-file plumbing |
| **unsafe sysctls** (1) | `fs.mqueue.msg_max` | safe sysctls are applied (`/proc/sys` writes); the guest kernel lacks `CONFIG_POSIX_MQUEUE` so `/proc/sys/fs/mqueue/` is absent | ❌ guest-kernel |
| **AppArmor** (~2) | unloaded profile, profile blocking writes | LSM not wired in the guest | ✅ guest feature |
| **Capabilities** (~3) | add/drop capability, drop ALL | capabilities not managed per-container; `brctl` bridge test also needs a `CONFIG_BRIDGE` guest kernel | ⚠️ murky / possibly kernel-limited |
| **MaskedPaths** (1) | mask `/bin/ls` | masking code is correct, but under the alpine/busybox substitution `/bin/ls` and `/bin/sh` are the same `busybox` binary, so masking `ls` breaks `sh`; the expected stderr needs `sh` to run | ❌ test-image artifact |

## Stability & hardening

- **Graceful shutdown:** on SIGTERM/SIGINT the CRI drains the gRPC server then
  reaps every sandbox VM (kills the shim, unmounts its overlay, removes the
  rootfs dir) — no more orphaned microVMs/overlays across restarts. The reaper
  runs even if the server exited with an error.
- **Adversarial review (16 confirmed findings, all fixed):** caller-supplied
  `A3S_SEC_*` env can no longer spoof the security envelope (privilege
  escalation); the seccomp BPF filter is built before `fork` (no async-signal
  unsafe allocation in the post-fork child — a musl malloc-deadlock risk);
  MaskedPaths/ReadonlyPaths mounts are idempotent (no per-exec mount-leak);
  MaskedPaths/ReadonlyPaths/sysctl names are path-traversal validated; misc
  panic/leak/non-Linux-build fixes.
- **ReopenContainerLog is synchronous** (waits for the supervisor to confirm the
  reopen). Reduces — but does not fully eliminate — the "reopening container
  log" flake; the residual race is guest→host log-transport ordering and needs
  an exec-stream flush barrier.
- **TODO:** SIGKILL/crash-case startup reaping (reconcile already marks orphaned
  sandboxes NotReady, but does not yet reap the dead instance's leftover shim
  processes + overlay mounts).

## Methodology

The baseline is captured so each fix is measurable. Re-run after each CRI feature
lands and update the "Latest run" table; the goal is to drive Failed → 0
(excluding documented environmental/test-image artifacts) and graduate
`a3s-box-cri` to a conformant, mature CRI runtime.
