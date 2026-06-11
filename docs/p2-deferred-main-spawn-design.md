# Design: P2 — Deferred-Main-Spawn (full box semantics for pooled sandboxes)

Status: **GO-WITH-CONDITIONS** (design + prototype-first). Builds on
`refactor/init-readiness` (PR #15: early-bind + event-driven readiness + PID1
reaper) and `feat/p1-template-pool` (PR #18: the warm-sandbox pool controller).
Derived from an adversarial mapping of the real #15+#18 base.

## 1. Goal

The pool MVP (PR #18) runs a command in a warm VM via the **exec stream**, so its
output comes back over the exec protocol, **not** the json-file `logs`. P2 gives a
pooled sandbox **full `box` semantics** — the command becomes the VM's real
**container main**, so its exit code flows through the normal `<box>/upper/.a3s_exit_code`
path and its stdout/stderr land in `<box>/logs/container.json` exactly like a
normal `box run`. The VM still skips cold boot (it was pre-warmed), but now behaves
like a first-class box.

## 2. Verdict & the two crux realizations

**GO-WITH-CONDITIONS** — both hard problems are tractable on the #15 base:

1. **Console logs are "free" via process-wide fd inheritance.** The shim wires the
   libkrun split console at boot (`shim/main.rs` `add_split_console`, fds kept alive
   via `mem::forget`); inside the guest, PID 1 holds fds 1/2 = `/dev/console`.
   guest-init routes *its own* logs to `/dev/kmsg` to keep the console clean. The
   boot main reaches `container.json` today **only** because `namespace::spawn_isolated`
   leaves stdout/stderr at the default `Stdio::inherit`. So a deferred main spawned
   with `Stdio::inherit` inherits PID 1's console fds and its output flows to
   `console.log`/`console.err.log` → the log processor tags it into `container.json`.
   **No fd-stashing, dup-to-100, or env-passing of fd numbers is required** — there
   is one shared fd table. (The exec path's `Stdio::piped()` is exactly why exec
   output does *not* reach the logs today.)

2. **The multi-threaded fork hazard is avoidable.** Do **not** spawn the deferred
   main via `spawn_isolated`'s raw `fork()` — its child runs heavy allocating code
   (tracing, `fs::metadata`, user resolution) before exec and can deadlock on an
   allocator/tracing lock held by another thread of the (deeply multi-threaded)
   PID 1. Instead spawn via `std::process::Command::spawn()` — the same clone/exec
   primitive the exec server already uses safely — whose child runs **only** the
   registered async-signal-safe `pre_exec` hook before `execvp`. The VM itself
   provides isolation, so the deferred main needs **no** `unshare()`/PID-namespace,
   removing the second fork entirely.

**Conditions:** exactly one spawn-main (CAS on the container-pid sentinel); the
late container-pid is handed to the reaper atomically (register MANAGED → publish
→ drop guard) so it can't be reaped as an orphan (the issue-#3 class of bug).

## 3. Design (end-to-end)

Deferred-main is a **replacement** for the keepalive (it IS the VM's main), not a
companion.

- **Boot IDLE** — `BOX_DEFERRED_MAIN=1` makes `run_init` skip `spawn_isolated`;
  the container-pid stays the sentinel `-1` and PID 1 stays alive (the
  `wait_for_children` loop already loops). The early-bind (Step 2.6) and accept-loop
  (Step 8) are **unchanged**, so host readiness passes IDLE: the heartbeat handler
  is a pure protocol handshake with **no** container-pid dependency — so
  `BoxState::Ready` already means "exec server live", which is the de-facto contract
  today.
- **spawn-main control frame** — the host sends `spawn-main:<json ExecRequest>` on
  the exec vsock. The handler funnels the request through a **safe** spawn path:
  `std::process::Command` + `Command::spawn()` under `reaper::spawn_managed`, with a
  `build_command` variant that leaves stdout/stderr at `Stdio::inherit()` (so the
  child inherits PID 1's console fds). Security (seccomp, user resolution, binary
  stat) is built in the **parent** before spawn, mirroring `apply_security_before_exec`.
- **Reaper / exit-code handoff** — make the supervision loop's `container_pid` an
  `AtomicI32` read each tick. Order is the crux: `spawn_managed` (lock-held-across-
  spawn closes the fork/registration race) → `set_container_pid(pid)` **while still
  MANAGED** → drop the guard. The `is_managed` branch covers the pre-publish window;
  the `pid == container_pid` branch then reaps it, persists `/.a3s_exit_code`
  (overlay upper), and `process::exit(code)` halts the VM. The handler replies
  `spawn-main-ack` only **after** a successful spawn+publish (so a fork failure is
  reported, not lost).
- **Pool integration (#18)** — add `Request::SpawnMain` to `pool.rs`; the daemon
  sends spawn-main instead of `vm.exec_command`, waits for VM exit (the existing
  teardown owns lifecycle), and reads exit code from `<box>/upper/.a3s_exit_code`
  and logs from `<box>/logs/container.json`. `Request::Run` stays for back-compat.

## 4. Risk-ranked blockers (with mitigations)

| sev | blocker | mitigation |
|-----|---------|-----------|
| HIGH | multi-threaded fork deadlock (raw `fork()` + allocating child) | spawn via `Command::spawn()`, not `spawn_isolated`; build seccomp/user/stat in the parent; no `unshare()` (VM isolates) |
| HIGH | late container-pid race → reaped as orphan, exit code lost (issue-#3 class) | `AtomicI32` container-pid read each tick; `spawn_managed` → publish-while-MANAGED → drop guard; guest unit test: spawn-main an immediate-exit cmd, assert real code not 0 |
| HIGH | console logs broken if deferred main reuses the exec path's `Stdio::piped()` | `build_command` variant with `Stdio::inherit()` stdout/stderr; integration test: spawn-main `echo`, assert the line appears stream-tagged in `container.json` |
| MED | readiness-contract drift (Ready = "no container yet") reopens connection-refused races | scope P2 to the **pool path only** (daemon explicitly drives spawn-main then waits); leave normal `box run` on eager boot-spawn; IDLE-Ready is pool-internal |
| MED | multiple spawn-main frames → two mains race to set container-pid / write exit code | CAS container-pid `-1 → pending → pid`; a second concurrent spawn-main gets "main already spawned" |
| LOW | PTY-server `pre_exec` uses `set_var` (not async-signal-safe) | base the deferred spawner on the **exec** path (`Command::spawn`), never the PTY raw-fork path |

## 5. Phased plan (smallest verifiable steps)

0. **PROTOTYPE (throwaway, KVM)** — from inside an exec-server connection thread
   (i.e. multi-threaded PID 1), `Command::spawn` `/bin/sh -c 'echo OUT; echo ERR 1>&2;
   exit 7'` with `Stdio::inherit`. Verify **at once**: (a) no fork deadlock under
   concurrent exec load, (b) OUT/ERR land in `container.json` correctly stream-tagged,
   (c) exit 7 propagates via `/.a3s_exit_code`. This one prototype de-risks the whole
   feature. **Must run on real KVM** (fork/allocator-lock + virtio-console don't
   reproduce on the macOS dev stub).
1. IDLE boot behind `BOX_DEFERRED_MAIN=1`; assert host readiness still reaches Ready
   with no container.
2. Convert supervision-loop `container_pid` to `AtomicI32` (set once at boot as
   today — no behavior change); regression-test the eager boot main still reaps right.
3. Safe deferred spawner: `Stdio::inherit` `build_command` variant under
   `spawn_managed`; CAS sentinel→pending→pid; publish-before-drop; ack on success.
4. Host spawn-main control frame: `send_spawn_main_control_frame` +
   `EXEC_CONTROL_SPAWN_MAIN`; `wait_main_exit` (poll `/.a3s_exit_code`) +
   `collect_logs` (read `container.json`).
5. Pool integration: `Request::SpawnMain` in `pool.rs`; full e2e — pool spawn-main a
   real image entrypoint, assert exit code + json-file logs + clean teardown.
6. Full KVM matrix: issue-#3 before/after readiness, fast-exit (`false`) exit-code,
   large-output log flushing, concurrent spawn-main rejection. Docs per repo rule.

## 6. Prototype-first

The single experiment that de-risks everything: a multi-threaded spawn-main that
simultaneously proves **(a)** safe fork via `Command::spawn()` under concurrent exec
load (no deadlock), **(b)** inherited-stdio output → `container.json` correctly
stream-tagged, **(c)** real exit code in `/.a3s_exit_code`. On real KVM only.
