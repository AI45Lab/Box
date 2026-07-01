//! Central PID 1 child reaper registry.
//!
//! As PID 1, guest-init must reap orphaned/reparented grandchildren and the
//! sidecar so they do not accumulate as zombies for the VM's lifetime. But it
//! must NOT steal the exit status of the children that the exec/PTY request
//! handlers are waiting on: each handler reaps its own child (via `waitpid` on a
//! specific pid) to read the real exit code, and a `waitpid(-1)` in the
//! supervision loop would race and reap it first — making the handler observe
//! `ECHILD` and report a bogus exit code 0.
//!
//! This registry lets the supervision loop tell the two apart. A handler marks
//! its child pid MANAGED across the spawn (so the loop cannot reap-decide the new
//! pid before it is registered), and the loop peeks exited children
//! non-destructively (`waitid` with `WNOWAIT`): it reaps only the container (→
//! lifecycle) and unmanaged children (true orphans + the sidecar), and leaves
//! managed children for their handler to reap.

use std::sync::Mutex;

/// PIDs currently owned by an exec/PTY request handler (it will reap them).
/// A small `Vec` — only a handful of in-flight exec/PTY children at once, so the
/// linear scans are cheaper than a hashed set and `Vec::new()` is a `const` init.
static MANAGED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// RAII guard: removes its pid from the MANAGED set when dropped (i.e. when the
/// handler returns, having reaped its own child). Drop runs on every handler
/// exit path — normal, timeout-kill, and error — so the set never leaks.
pub struct ManagedChild(i32);

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Recover from a poisoned lock rather than panicking inside PID 1.
        let mut managed = MANAGED.lock().unwrap_or_else(|e| e.into_inner());
        managed.retain(|&p| p != self.0);
    }
}

/// Spawn a child while atomically marking its pid MANAGED.
///
/// The MANAGED lock is held across the `spawn` call, so the supervision loop
/// cannot conclude the new pid is unmanaged (and reap it as an orphan) in the
/// window between fork and registration — which closes the race for commands
/// that exit almost immediately (e.g. `exec … -- false`). Holding the lock
/// across the fork is safe: the forked child runs only async-signal-safe code
/// before `exec` and never touches this mutex, so its inherited (locked) copy is
/// discarded at `exec` without deadlock.
pub fn spawn_managed<F>(spawn: F) -> std::io::Result<(std::process::Child, ManagedChild)>
where
    F: FnOnce() -> std::io::Result<std::process::Child>,
{
    let mut managed = MANAGED.lock().unwrap_or_else(|e| e.into_inner());
    let child = spawn()?;
    let pid = child.id() as i32;
    managed.push(pid);
    drop(managed);
    Ok((child, ManagedChild(pid)))
}

/// Mark an already-forked pid MANAGED (raw-fork callers, e.g. the PTY server).
/// Call immediately after `fork` in the parent. Returns the RAII guard.
pub fn manage_pid(pid: i32) -> ManagedChild {
    let mut managed = MANAGED.lock().unwrap_or_else(|e| e.into_inner());
    if !managed.contains(&pid) {
        managed.push(pid);
    }
    drop(managed);
    ManagedChild(pid)
}

/// Whether `pid` is owned by a request handler (the loop must not reap it).
pub fn is_managed(pid: i32) -> bool {
    MANAGED
        .lock()
        .map(|m| m.contains(&pid))
        .unwrap_or_else(|e| e.into_inner().contains(&pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct synthetic pids per test so the shared MANAGED static doesn't
    // collide across parallel test threads.
    #[test]
    fn manage_pid_registers_until_guard_dropped() {
        let pid = 0x7fff_fff0;
        assert!(!is_managed(pid));
        {
            let _guard = manage_pid(pid);
            assert!(
                is_managed(pid),
                "pid should be managed while the guard lives"
            );
        }
        assert!(!is_managed(pid), "guard drop must unregister the pid");
    }

    #[test]
    fn unregistered_pid_is_not_managed() {
        assert!(!is_managed(0x7fff_fff1));
    }

    #[test]
    fn spawn_managed_marks_then_releases_real_child() {
        // A child that exits immediately is the worst case the lock-across-spawn
        // protects: it must still be MANAGED for the handler (here, the test) to
        // reap, never the supervision loop.
        let (mut child, guard) =
            spawn_managed(|| std::process::Command::new("true").spawn()).expect("spawn true");
        let pid = child.id() as i32;
        assert!(
            is_managed(pid),
            "spawned pid must be registered before we observe it"
        );
        let _ = child.wait(); // the handler (test) reaps its own child
        drop(guard);
        assert!(!is_managed(pid), "guard drop must unregister after reap");
    }
}
