//! VM readiness checks — waiting for exec socket.

use a3s_box_core::error::{BoxError, Result};

#[cfg(unix)]
use crate::grpc::ExecClient;

use super::VmManager;

impl VmManager {
    /// Wait for the VM process to be running (for generic OCI images without an agent).
    ///
    /// Gives the VM a brief moment to start, then verifies the process hasn't exited.
    pub(crate) async fn wait_for_vm_running(&self) -> Result<()> {
        const STABILIZE_MS: u64 = 1000;

        tracing::debug!("Waiting for VM process to stabilize");
        tokio::time::sleep(tokio::time::Duration::from_millis(STABILIZE_MS)).await;

        if let Some(ref handler) = *self.handler.read().await {
            if !handler.is_running() {
                return Err(BoxError::BoxBootError {
                    message: "VM process exited immediately after start".to_string(),
                    hint: Some("Check console output for errors".to_string()),
                });
            }
        }

        tracing::debug!("VM process is running");
        Ok(())
    }

    /// Wait for the exec server socket to become ready.
    ///
    /// Polls for the socket file to appear, then verifies the exec server
    /// is healthy via a Frame Heartbeat round-trip. This is best-effort:
    /// if the exec socket never appears (e.g., older guest init without
    /// exec server), the VM still boots successfully.
    #[cfg(unix)]
    pub(crate) async fn wait_for_exec_ready(
        &mut self,
        exec_socket_path: &std::path::Path,
    ) -> Result<()> {
        // The guest binds the exec server only late in boot (after virtio-fs
        // pivot, passt network bring-up, and the container spawn — guest-init
        // cannot start it earlier without forking the container while
        // multi-threaded, which is unsafe). A cold first run on a slow/loaded
        // host can push that past the old 10s budget, which surfaced as a false
        // "heartbeat failed" warning and, for `run -it`, a PTY connect that gave
        // up before the server came up (issue #3). Wait longer — this is cheap
        // for healthy boxes (they return as soon as the heartbeat passes) and the
        // loop already bails out the moment the VM exits, so a fast-exiting
        // container never stalls for the full budget.
        const MAX_WAIT_MS: u64 = 30000;
        const POLL_INTERVAL_MS: u64 = 200;

        tracing::debug!(
            socket_path = %exec_socket_path.display(),
            "Waiting for exec server socket"
        );

        let start = std::time::Instant::now();

        // Phase 1: Wait for socket file to appear
        loop {
            if start.elapsed().as_millis() >= MAX_WAIT_MS as u128 {
                tracing::warn!(
                    timeout_ms = MAX_WAIT_MS,
                    "Exec socket did not appear within timeout; exec/attach will connect on demand if the guest exposes it"
                );
                return Ok(());
            }

            if exec_socket_path.exists() {
                tracing::debug!("Exec socket file detected");
                break;
            }

            // Check if VM is still running (has_exited treats a zombie shim as
            // exited; is_running's kill(pid,0) would not).
            if let Some(ref handler) = *self.handler.read().await {
                if handler.has_exited() {
                    tracing::warn!("VM exited before exec socket appeared");
                    return Ok(());
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        // Phase 2: Connect and verify with Heartbeat health check
        while start.elapsed().as_millis() < MAX_WAIT_MS as u128 {
            // Stop waiting if the VM has already exited: the exec socket can
            // appear during guest init and then vanish when a fast-exiting
            // container shuts the VM down. The shim becomes a zombie the moment
            // the VM halts, so use has_exited (zombie-aware) rather than
            // is_running — without this, a container that exits before its first
            // heartbeat stalls the whole boot for the full MAX_WAIT_MS budget,
            // which hit every short-lived `run` that lost the heartbeat race and
            // every monitor restart of a fast-exiting container.
            if let Some(ref handler) = *self.handler.read().await {
                if handler.has_exited() {
                    tracing::debug!("VM exited before exec server became ready");
                    return Ok(());
                }
            }

            match ExecClient::connect(exec_socket_path).await {
                Ok(client) => match client.heartbeat().await {
                    Ok(true) => {
                        tracing::debug!("Exec server heartbeat passed");
                        self.exec_client = Some(client);
                        return Ok(());
                    }
                    Ok(false) => {
                        tracing::debug!("Exec server heartbeat failed, retrying");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "Exec heartbeat error, retrying");
                    }
                },
                Err(e) => {
                    tracing::debug!(error = %e, "Exec connect failed, retrying");
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
        }

        tracing::warn!(
            timeout_ms = MAX_WAIT_MS,
            "Exec server did not pass a heartbeat within timeout; exec/attach connect on demand and may still succeed once the guest finishes starting"
        );
        Ok(())
    }
}
