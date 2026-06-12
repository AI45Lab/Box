# Feasibility: Native snapshot-fork in our libkrun fork

Status: **survey complete — GO, native path chosen over a second VMM.**
Supersedes the backend-selection question left open in
`cow-snapshot-fork-design.md` (§2): instead of adding a Firecracker/Cloud
Hypervisor second backend, we extend **our own libkrun fork**
([A3S-Lab/libkrun](https://github.com/A3S-Lab/libkrun), vendored as a git
checkout at `src/deps/libkrun-sys/vendor/libkrun`, built by our `build.rs`).

## Why native (vs a second VMM)

- Single VMM: rootfs (virtio-fs dir), vsock exec server, networking, lifecycle
  all stay as-is — no adapter layer, no dual ops surface. (Firecracker has no
  virtio-fs at all; CH would still mean a permanent second backend.)
- We already patch vendored deps (the CRI vendored-h2 patch precedent).
- The fork's VMM is Firecracker-derived and **already contains** much of the
  hard state code (below).

## What the survey found (vendor/libkrun/src, x86_64 Linux)

| Area | Status |
|---|---|
| vCPU state save+restore (regs/sregs/xsave/MSRs/LAPIC/CPUID, ordered) | **EXISTS** — `vmm/src/linux/vstate.rs:1302-1416` (`#[allow(unused)]` dead code) |
| VM state save+restore (PIT, clock, PIC/IOAPIC irqchips) | **EXISTS** — `vstate.rs:834-892` |
| vCPU Pause/Resume events | **HALF-WIRED** — `VcpuEvent::Pause/Resume` + `VcpuHandle::pause()` exist; `Vmm::resume_vcpus()` is public, `pause_vcpus()` missing |
| Guest RAM | anonymous mmap (`builder.rs:3057`); **but the TEE path already uses `guest_memfd`** — in-tree precedent for non-anonymous backing |
| Virtio device state serialization (fs/net/vsock/console/balloon/rng) | **MISSING entirely** — no Persist trait, nothing |
| Snapshot orchestration / FFI | **MISSING** |

## The key scoping insight: snapshot an IDLE deferred-main template

The generic "snapshot any running VM" problem (Firecracker's) is dominated by
device state. **Our use case doesn't need it.** The pool snapshots a template VM
that booted with `deferred_main` (P2) and is **quiesced**: no container main, the
exec server blocked on accept, virtio queues empty, no in-flight I/O. At that
point:

- queue rings live in guest RAM (captured by the RAM snapshot); host-side device
  state shrinks to per-queue ring addresses + ready/activation status + device
  config — small, explicit structs;
- vsock: no live connections at idle → muxer state is empty;
- console: stream positions only;
- **virtio-fs is the one real device problem**: the in-process passthrough
  server holds a host-side inode/nodeid map the guest references. It must be
  serialized (or rebuilt deterministically) — this is the riskiest single item.

## Phased plan (Linux x86_64 only first)

1. **Phase A — RAM + CPU snapshot/restore of a quiesced idle VM (~1.5–2w)**
   - memfd/file-backed guest RAM (mirror the TEE `guest_memfd` plumbing for
     non-TEE).
   - `Vmm::pause_vcpus()` (wire the existing Pause event), then
     `krun_snapshot(path)`: vCPU+VM state (existing code) + RAM dump + per-queue
     MMIO/virtio registration state.
   - `krun_restore(path)`: rebuild the VM, **`MAP_PRIVATE` the RAM file**
     (kernel page-level CoW — this is the fork), restore vCPU/VM state, re-plug
     devices with the saved queue state, resume.
   - Exit criterion: an idle deferred-main alpine template restores and answers
     an exec heartbeat. Measure restore latency (target ~10–150ms).
2. **Phase B — virtio-fs inode-map persistence (~1–2w, the hard part)**
   - Serialize the passthrough server's nodeid→inode map (paths + generation),
     re-open on restore. Exit criterion: restored VM can read/write its rootfs
     and run `spawn-main` to completion with correct logs/exit code.
3. **Phase C — box/pool integration (~1w)**
   - Template manager: boot idle template → snapshot → N×`krun_restore` children;
     re-stamp identity via the existing exec server (P2 spawn-main already gives
     the command path). Pool flag `--fork` falling back to full boots.
4. **Phase D — hardening**: concurrent restores, snapshot invalidation
   (image digest + config + libkrun version), KSM interaction, aarch64 later.

Estimated total: **4–6 weeks** (matches the survey's verdict), with Phase A
delivering a measurable go/no-go checkpoint in ~2 weeks.

## Risks

| risk | mitigation |
|---|---|
| virtio-fs inode map (guest holds nodeids) | Phase B dedicated; fallback: restart the fs device + remount in guest via a guest-init hook (slower, uglier) |
| `vm_memory` crate hides mmap flags | TEE guest_memfd path shows where to hook; worst case small vm-memory patch in-fork |
| device quiesce atomicity | snapshot only IDLE deferred-main templates (enforced by the box side) |
| fork maintenance burden | changes confined to fork; upstream rebases already our responsibility |
