//! VM readiness checks — waiting for exec socket.

use a3s_box_core::error::{BoxError, Result};

#[cfg(unix)]
use crate::grpc::ExecClient;

use super::VmManager;

impl VmManager {
    /// Confirm the VM didn't fail on launch (for generic OCI images without an agent).
    ///
    /// A bad config makes libkrun exit within milliseconds, so we only need a short
    /// window to catch an *immediate* crash and fail loudly. Poll for that instead
    /// of a fixed 1 s sleep — it shaved ~750 ms off every boot. Crashes that happen
    /// later are caught by `wait_for_exec_ready`'s `has_exited` checks, which gate
    /// the rest of boot anyway.
    pub(crate) async fn wait_for_vm_running(&self) -> Result<()> {
        // This is a crash-detection grace period, not a readiness wait: the VM
        // process is alive the instant the shim is spawned, and we just watch for it
        // exiting immediately. A snapshot-restored VM reaches its run loop in ~20ms
        // (no cold boot), so a short grace catches an immediate restore failure while
        // saving ~200ms on the fork fast-path; a cold boot keeps the longer grace.
        #[cfg(unix)]
        let max_wait_ms: u64 = if super::is_restore_mode(&self.config) {
            40
        } else {
            250
        };
        #[cfg(not(unix))]
        let max_wait_ms: u64 = 250;
        const POLL_MS: u64 = 10;

        tracing::debug!("Confirming VM process started");
        let start = std::time::Instant::now();
        loop {
            if let Some(ref handler) = *self.handler.read().await {
                // has_exited is zombie-aware (a halted VM's shim becomes a zombie);
                // is_running's kill(pid,0) would still report it alive.
                if handler.has_exited() {
                    return Err(BoxError::BoxBootError {
                        message: "VM process exited immediately after start".to_string(),
                        hint: Some("Check console output for errors".to_string()),
                    });
                }
            }
            if start.elapsed().as_millis() >= max_wait_ms as u128 {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_MS)).await;
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

    /// Single best-effort exec-server probe for snapshot-restore boots.
    ///
    /// A restored guest is already past boot, so its exec server never re-signals
    /// readiness the way a cold boot does — blocking on [`wait_for_exec_ready`]'s
    /// cold-boot loop would stall registration for up to its safety cap. Instead try
    /// exactly one connect + heartbeat to populate `exec_client` if the guest answers
    /// promptly, and otherwise proceed immediately: exec/attach connect on demand.
    #[cfg(unix)]
    pub(crate) async fn probe_exec_ready_once(&mut self, exec_socket_path: &std::path::Path) {
        use tokio::time::Duration;
        const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);

        if let Ok(Ok(client)) =
            tokio::time::timeout(ATTEMPT_TIMEOUT, ExecClient::connect(exec_socket_path)).await
        {
            if let Ok(Ok(true)) = tokio::time::timeout(ATTEMPT_TIMEOUT, client.heartbeat()).await {
                tracing::debug!("restore: exec server heartbeat passed");
                self.exec_client = Some(client);
                return;
            }
        }
        tracing::debug!(
            "restore: exec server did not answer an immediate heartbeat; exec/attach will connect on demand"
        );
    }
}
