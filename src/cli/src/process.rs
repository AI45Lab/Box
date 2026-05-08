//! Shared process management utilities for CLI commands.

/// Result of asking a VM/shim process to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The process was already gone before a signal could be delivered.
    AlreadyExited,
    /// The process exited after the requested stop signal.
    GracefulExit,
    /// The process did not exit before the timeout and was force-killed.
    ForceKilled,
}

impl StopOutcome {
    /// Best-effort container-style exit code for commands that cannot reap the child.
    pub fn inferred_exit_code(self, stop_signal: i32) -> Option<i32> {
        match self {
            Self::AlreadyExited => None,
            Self::GracefulExit => Some(128 + stop_signal.max(0)),
            Self::ForceKilled => Some(137),
        }
    }
}

/// Check if a process is alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        if handle == 0 {
            return false;
        }
        let mut exit_code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        windows_sys::Win32::Foundation::CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }
}

/// Terminate a process immediately.
#[cfg(unix)]
pub fn terminate_process(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Send a Unix signal to a process and surface the OS error when delivery fails.
#[cfg(unix)]
pub fn send_signal(pid: u32, signal: i32) -> Result<(), std::io::Error> {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub fn terminate_process(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle != 0 {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// Send `signal`, wait up to `timeout` seconds, then force-terminate if still alive.
#[cfg(unix)]
pub async fn graceful_stop(pid: u32, signal: i32, timeout: u64) -> StopOutcome {
    if !is_process_alive(pid) {
        return StopOutcome::AlreadyExited;
    }

    unsafe {
        if libc::kill(pid as i32, signal) != 0 && !is_process_alive(pid) {
            return StopOutcome::AlreadyExited;
        }
    }

    let start = std::time::Instant::now();
    let timeout_ms = timeout.saturating_mul(1000);
    loop {
        if !is_process_alive(pid) {
            return StopOutcome::GracefulExit;
        }
        if start.elapsed().as_millis() >= timeout_ms as u128 {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            return StopOutcome::ForceKilled;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

#[cfg(windows)]
pub async fn graceful_stop(pid: u32, _signal: i32, _timeout: u64) -> StopOutcome {
    if !is_process_alive(pid) {
        return StopOutcome::AlreadyExited;
    }

    terminate_process(pid);
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    StopOutcome::ForceKilled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_current_process() {
        let current_pid = std::process::id();
        assert!(is_process_alive(current_pid));
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        assert!(!is_process_alive(99999));
    }

    #[cfg(unix)]
    #[test]
    fn test_is_process_alive_parent_process() {
        let parent_pid = unsafe { libc::getppid() as u32 };
        assert!(is_process_alive(parent_pid));
    }

    #[test]
    fn test_stop_outcome_exit_code_inference() {
        assert_eq!(StopOutcome::AlreadyExited.inferred_exit_code(15), None);
        assert_eq!(StopOutcome::GracefulExit.inferred_exit_code(15), Some(143));
        assert_eq!(StopOutcome::ForceKilled.inferred_exit_code(15), Some(137));
    }
}
