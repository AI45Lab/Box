//! Shared process management utilities for CLI commands.

/// Check if a process is alive by sending signal 0.
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Send SIGTERM, wait up to `timeout` seconds, then SIGKILL if still alive.
pub async fn graceful_stop(pid: u32, timeout: u64) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    let start = std::time::Instant::now();
    let timeout_ms = timeout * 1000;
    loop {
        if !is_process_alive(pid) {
            break;
        }
        if start.elapsed().as_millis() > timeout_ms as u128 {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
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

    #[test]
    fn test_is_process_alive_parent_process() {
        let parent_pid = unsafe { libc::getppid() as u32 };
        assert!(is_process_alive(parent_pid));
    }
}
