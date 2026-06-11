# Design: CoW Snapshot-Fork Backend for a3s-box

Status: **Proposal / design-only.** No code in this document is implemented yet.
Audience: a3s-box maintainers evaluating the high-fan-out (many short-lived
sandboxes) story for AI-agent workloads.

## 1. Motivation

a3s-box's sweet spot — isolated microVM sandboxes for AI agents (code interpreters,
tool use, eval harnesses) — is exactly the workload that wants to spawn **dozens to
hundreds of short-lived VMs fast and cheap**. Two reference projects show the
ceiling:

- **forkd** (Firecracker): boot a warmed template once, snapshot guest RAM + vCPU
  state, then `mmap` it `MAP_PRIVATE` into N children for kernel page-level CoW;
  UFFD captures dirty pages so the source resumes immediately. Claims: spawn 100
  sandboxes in ~101 ms; ~0.12 MiB per child; live-branch ~56 ms.
- **clone** (custom KVM VMM): "shadow clone" CoW fork from a template + KSM +
  balloon. Claims: fork <20 ms (Alpine), ~160 ms (4 GB Ubuntu); "100 forked VMs
  use memory like 10."

Both reduce the **second-through-Nth** VM of a template to a near-free CoW branch:
the child starts from a *running* state (no kernel boot, no init, no dependency
load) and shares most physical pages with the template.

### What a3s-box already does (and the gap)

| | a3s-box today | Snapshot-fork target |
|---|---|---|
| Memory dedup across same-image VMs | KSM (opt-in, `A3S_BOX_KSM`) — ~3.2× on 6× VMs | shared from t=0, no scan latency |
| Rootfs CoW | overlayfs (shared lower + per-box upper), near-instant | same |
| Fast start | `WarmPool` of **independent** full-boot VMs (each ~full RAM) | template + CoW children (shared RAM) |
| Cold-boot skip | none — every `run` cold-boots kernel+init | child resumes from running snapshot |

The gap is purely the **guest-RAM/vCPU snapshot + CoW restore**. Everything else
(rootfs CoW, the guest agent, the warm pool, networking) is already in place and is
reused below.

## 2. The hard constraint: libkrun has no snapshot

Verified against the vendored libkrun in this repo:

- The public C API exposes ~75 `krun_*` functions; **none** are
  snapshot/restore/pause/resume/migrate/fork. The only lifecycle call,
  `krun_start_enter`, **takes over the process and `_exit`s from inside** on guest
  shutdown — it never returns control on the success path.
- Guest RAM is an **anonymous private `mmap`** (`GuestMemoryMmap::from_ranges`,
  `flags: 0`) — not a memfd, not `MAP_SHARED`. It cannot be exported to another
  process for CoW, nor fault-handled via UFFD.
- libkrun's internal VMM (Firecracker-derived) carries dead, unwired
  `Vm::save_state`/`Vcpu::save_state`/`pause_vcpus`, but they cover **only KVM
  control structs (PIT/clock/irqchips/vCPU regs), not guest RAM**, and have no C
  entry point.

So snapshot-fork **cannot be layered on libkrun as integrated**. The two ways
forward:

1. **Fork libkrun**: memfd-back guest RAM, build a memory-snapshot + UFFD restore
   subsystem, wire the dead save/restore + add new `krun_*` exports, add a
   post-restore re-config hook. Heavy, and an ongoing maintenance burden against an
   upstream that doesn't support it.
2. **Add a second, snapshot-capable backend** behind a3s-box's existing backend
   abstraction (Firecracker or Cloud Hypervisor — both have mature snapshot +
   UFFD). libkrun stays the default for single `run`s; the fork backend serves the
   high-fan-out path.

**This design chooses (2).** Rationale: Firecracker/CH snapshot is battle-tested;
the backend trait already exists ("platform abstraction layer with trait-based
backends" + a "Linux KVM backend stub" are present in the tree); and we avoid
owning a libkrun memory-VMM fork forever. Cost: a second VMM to build/maintain and
cross-backend adapters for rootfs/network/vsock.

## 3. Architecture

```
                 ┌──────────────── a3s-box runtime ────────────────┐
   run --fork →  │  ForkController                                 │
                 │   ├─ TemplatePool   (per image:digest+config)   │
                 │   │    └─ Template  = warmed VM snapshot on disk │
                 │   │         (guest RAM file + vCPU/device state) │
                 │   └─ spawn_child(template) ──► VmBackend::Fork   │
                 └─────────────────────────────────────────────────┘
                                    │ (Firecracker/CH backend)
        UFFD page server ◄──────────┤  restore vCPU/device state,
        (serves template RAM file)  │  mmap RAM region, fault-in CoW
                                    ▼
                          Child microVM (running, t≈running)
                                    │ vsock 4089 (exec)  ← existing guest agent
            re-inject identity ─────┘  (hostname, IP, box_id, container cmd)
```

### 3.1 Backend trait

Extend the existing VM-backend trait with an optional fork capability:

```rust
trait VmBackend {
    fn boot(&self, spec: &InstanceSpec) -> Result<VmHandle>;          // libkrun + fork backends
    // Fork capability (only the snapshot-capable backend implements it):
    fn snapshot_template(&self, vm: &VmHandle, dst: &TemplatePath) -> Result<()> { Err(Unsupported) }
    fn fork_from(&self, template: &TemplatePath, child: &ChildSpec) -> Result<VmHandle> { Err(Unsupported) }
}
```

`ForkController` selects the snapshot-capable backend; if none is available (e.g.,
libkrun-only build) it transparently falls back to the `WarmPool` (full boots) — so
`--fork` degrades gracefully rather than failing.

### 3.2 Template lifecycle

1. **Build template** (once per `image:digest` + resource/config shape): boot a VM
   with the image's rootfs and a *generic* warmed state — kernel up, guest-init up,
   exec/PTY servers listening, optionally common runtime preloaded (e.g. a Python
   interpreter imported in PID 1, forkd-style). **No container command yet** and
   **no per-box identity** baked in.
2. **Quiesce + snapshot**: pause vCPUs, serialize vCPU/device state, write guest RAM
   to a backing file (sparse). Resume or discard the template VM.
3. **Cache** the template keyed by `(image_digest, vcpus, mem, kernel_version, net_mode)`
   under `~/.a3s/templates/<key>/` (`mem.snap` + `vmstate.json`). Diff-snapshot
   chaining (§3.5) layers on top.

### 3.3 Fork a child (the hot path)

1. Restore vCPU/device state from `vmstate.json`.
2. `mmap` the template `mem.snap` `MAP_PRIVATE` (kernel CoW) **or** register a UFFD
   region whose fault handler serves pages from `mem.snap` — the child's writes
   diverge privately; unwritten pages stay shared. (Firecracker's UFFD restore is
   the proven path; `MAP_PRIVATE` of a shared file is the simpler clone-style path.)
3. Give the child its **own rootfs branch**: a fresh overlay `upper`/`work`/`merged`
   over the same shared read-only lower (image cache) — already supported by
   `OverlayProvider`, ~instant, adds nothing new at the FS layer.
4. Attach a fresh network endpoint (new IP / netns / passt) — see §3.6.
5. Resume vCPUs → the child is *running* in ~10–150 ms, sharing template pages.

### 3.4 Post-fork re-injection — a3s-box's key advantage

A forked child is an exact clone of the template's running guest, so it must be
re-stamped: hostname, `/etc/hosts`/`resolv.conf`, network identity, `box_id`, and
**the actual container command** (the template had none). libkrun offers no
post-boot reconfig hook — but **a3s-box already runs a vsock exec server (port
4089) inside the guest**. That agent is the natural landing point:

- The runtime connects to the child's exec socket and sends a new control message,
  e.g. `reidentify { box_id, hostname, ip, gateway, dns }` then `exec-main { cmd,
  args, env, workdir, user }`.
- guest-init applies the identity (write `/etc/hostname`, bring up the new IP,
  re-stamp `BOX_EXEC_*`) and then spawns the real container entrypoint — reusing the
  *existing* `spawn_isolated` + `ExecConfig` path, just triggered post-restore
  instead of at boot.

This turns "the template had no command and generic identity" from a blocker into a
clean two-message handshake over an interface that already exists.

### 3.5 Diff-snapshot chaining (forkd v0.5)

Stack incremental memory snapshots so heavy runtimes aren't duplicated:
`alpine-base` → `+python` → `+numpy`. The template cache stores parent-hash edges;
`fork_from` walks the chain and assembles the child's memory view (base pages from
the root snapshot, overrides from each diff). Saves disk and warm-up when many
templates share a base. Phase 3 — not required for an MVP.

### 3.6 Networking

Per-child network identity is the fiddliest re-injection. Options, simplest first:

- **TSI / userspace-net (default `run`)**: no eth0; the child gets a fresh
  host-side proxy. Re-injection is just DNS/identity env — cheapest, do this first.
- **Bridge mode**: allocate a new tap/veth + IP per child; the guest brings up the
  new interface via the re-identify message (the guest already has the eth0-wait +
  IP-assign code; trigger it post-restore instead of at boot).

## 4. Phased plan

- **Phase 0 (shipped/PR #16):** KSM dedup + boot-floor trim + reflink. Gets the
  *memory-density* half of the clone story now, on libkrun, with no fork.
- **Phase 1 — template pool, full boots (no snapshot yet):** generalize `WarmPool`
  into a *per-image* `TemplatePool` and add the post-fork re-injection handshake
  (reidentify + exec-main) over the existing exec server. This alone removes
  cold-boot from the hot path (acquire a warmed same-image VM, inject the command)
  and is **100% doable on libkrun** — it's the de-risking step and is independently
  valuable.
- **Phase 2 — real snapshot-fork backend:** implement the Firecracker (or CH)
  `VmBackend::{snapshot_template, fork_from}` with UFFD restore + overlay rootfs
  branch + network attach. Reuse Phase-1's re-injection. Gate behind `--fork` /
  config; fall back to the template pool when unavailable.
- **Phase 3 — diff-snapshot chaining + balloon reclaim** for memory scaling.

Phase 1 delivers most of the latency win and is the right next implementation step;
Phase 2 is where the shared-RAM "100 ≈ 10" density and ~10–150 ms spawns land.

## 5. Risks & open questions

- **Two VMMs to maintain.** Firecracker/CH rootfs is a block device or virtio-fs;
  a3s-box's rootfs is a host dir over virtio-fs. The fork backend needs a
  rootfs/vsock/network adapter layer. Scope it to the fork path only.
- **Snapshot security.** A template snapshot freezes secrets/entropy/RNG state;
  children must re-seed RNG and must not inherit per-tenant secrets. vmgenid +
  re-seed on restore (Firecracker supports this). Acceptable for same-tenant agent
  sandboxes; document the boundary.
- **Template invalidation.** Keyed by image digest + config + kernel version; rebuild
  on any change. Stale templates must never be reused across kernel upgrades.
- **KSM vs CoW-fork overlap.** Phase 0 KSM and Phase 2 CoW-fork both dedup memory;
  CoW-fork shares from t=0 (no scan), KSM dedups across *unrelated* boots. Keep both:
  CoW-fork for template children, KSM for the rest.
- **Does the fan-out demand justify a second VMM?** Open product question — Phase 1
  (template pool on libkrun) is the cheap way to test the demand before committing to
  Phase 2.

## 6. Recommendation

Ship Phase 0 (done). Build **Phase 1 (per-image template pool + re-injection
handshake)** next — it removes cold boot from the hot path entirely on the current
libkrun and is the prerequisite + de-risk for Phase 2. Treat Phase 2 (snapshot-fork
backend) as a funded epic, justified once high-fan-out demand is demonstrated.
