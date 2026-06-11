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

    /// Wait for the exec server to become ready (a Frame Heartbeat round-trip).
    ///
    /// Waits for the readiness EVENT — a successful heartbeat — bounded by VM
    /// liveness, instead of guessing a fixed timeout. guest-init binds the exec
    /// socket early (before the slow network bring-up and container spawn), so the
    /// host connect succeeds immediately and the heartbeat passes the moment the
    /// guest's accept loop runs — however late in a slow cold boot. Each attempt
    /// is individually time-bounded (the early-bound socket makes a host `connect`
    /// succeed and then block on read until the guest accepts), the loop returns
    /// at once if the VM has exited (a fast-exiting container never stalls), and a
    /// large absolute cap is only a last-resort backstop against a wedged-but-alive
    /// guest — not the expected wait. Best-effort: exec/attach also connect on
    /// demand, so even a timed-out probe does not mean exec is unavailable.
    #[cfg(unix)]
    pub(crate) async fn wait_for_exec_ready(
        &mut self,
        exec_socket_path: &std::path::Path,
    ) -> Result<()> {
        use tokio::time::Duration;

        // Per-attempt cap on one connect + heartbeat round-trip. guest-init binds
        // the exec socket early, so the host `connect` succeeds as soon as the VM
        // boots and `heartbeat()`'s read then blocks until the guest's accept loop
        // runs; bounding each attempt keeps the loop checking VM liveness instead
        // of hanging in that read.
        const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);
        const POLL_INTERVAL: Duration = Duration::from_millis(200);
        // Last-resort backstop against a wedged-but-alive guest that binds but
        // never accepts. A healthy guest passes the heartbeat the instant its
        // accept loop runs (however late), and an exited VM returns immediately
        // below — so this cap is not the expected wait.
        const MAX_WAIT_MS: u64 = 120_000;

        tracing::debug!(
            socket_path = %exec_socket_path.display(),
            "Waiting for exec server readiness"
        );

        let start = std::time::Instant::now();

        loop {
            // Return at once if the VM has already exited (zombie-aware: has_exited
            // treats a zombie shim as exited, unlike is_running's kill(pid,0)). A
            // fast-exiting container never stalls here.
            if let Some(ref handler) = *self.handler.read().await {
                if handler.has_exited() {
                    tracing::debug!("VM exited before exec server became ready");
                    return Ok(());
                }
            }

            // One bounded connect + heartbeat attempt. A timeout (early-bound
            // socket, guest not yet accepting) or any error just means "retry".
            if let Ok(Ok(client)) =
                tokio::time::timeout(ATTEMPT_TIMEOUT, ExecClient::connect(exec_socket_path)).await
            {
                if let Ok(Ok(true)) =
                    tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await
                {
                    tracing::debug!("Exec server heartbeat passed");
                    self.exec_client = Some(client);
                    return Ok(());
                }
            }

            if start.elapsed().as_millis() >= MAX_WAIT_MS as u128 {
                tracing::warn!(
                    timeout_ms = MAX_WAIT_MS,
                    "Exec server did not become ready within the safety cap; exec/attach connect on demand and may still succeed once the guest finishes starting"
                );
                return Ok(());
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
