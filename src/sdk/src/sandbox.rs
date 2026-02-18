//! Sandbox — a running MicroVM instance.

use std::path::PathBuf;

use a3s_box_core::error::Result;
use a3s_box_core::exec::{ExecMetrics, ExecOutput, ExecRequest, FileOp, FileRequest};
use a3s_box_runtime::{ExecClient, PtyClient, StreamingExec, VmManager};
use base64::Engine;

/// Result of executing a command in a sandbox.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Standard output (lossy UTF-8 conversion from raw bytes).
    pub stdout: String,
    /// Standard error (lossy UTF-8 conversion from raw bytes).
    pub stderr: String,
    /// Exit code (0 = success).
    pub exit_code: i32,
    /// Execution metrics (duration, bytes transferred).
    pub metrics: ExecMetrics,
}

impl From<ExecOutput> for ExecResult {
    fn from(output: ExecOutput) -> Self {
        Self {
            metrics: ExecMetrics {
                stdout_bytes: output.stdout.len() as u64,
                stderr_bytes: output.stderr.len() as u64,
                ..Default::default()
            },
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.exit_code,
        }
    }
}

/// A running MicroVM sandbox.
///
/// Provides methods to execute commands, stream output, transfer files,
/// open PTY sessions, and manage the sandbox lifecycle.
pub struct Sandbox {
    /// Unique sandbox identifier.
    id: String,
    /// Human-readable name.
    name: String,
    /// VM manager (owns the VM lifecycle).
    vm: VmManager,
    /// Path to the exec Unix socket.
    exec_socket: PathBuf,
    /// Path to the PTY Unix socket.
    pty_socket: PathBuf,
}

impl Sandbox {
    /// Create a new Sandbox handle (called by BoxSdk::create).
    pub(crate) fn new(
        id: String,
        name: String,
        vm: VmManager,
        exec_socket: PathBuf,
        pty_socket: PathBuf,
    ) -> Self {
        Self {
            id,
            name,
            vm,
            exec_socket,
            pty_socket,
        }
    }

    /// Get the sandbox ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the sandbox name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the current sandbox state.
    pub async fn state(&self) -> a3s_box_runtime::BoxState {
        self.vm.state().await
    }

    /// Execute a command in the sandbox.
    ///
    /// Waits for the command to complete and returns all output at once.
    ///
    /// # Arguments
    /// * `cmd` - Command to execute
    /// * `args` - Command arguments
    pub async fn exec(&self, cmd: &str, args: &[&str]) -> Result<ExecResult> {
        let started = std::time::Instant::now();
        let mut cmd_parts = vec![cmd.to_string()];
        cmd_parts.extend(args.iter().map(|a| a.to_string()));

        let request = ExecRequest {
            cmd: cmd_parts,
            timeout_ns: 0,
            env: Vec::new(),
            working_dir: None,
            stdin: None,
            user: None,
            streaming: false,
        };

        let client = ExecClient::connect(&self.exec_socket).await?;
        let output = client.exec_command(&request).await?;
        let mut result = ExecResult::from(output);
        result.metrics.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Execute a command with environment variables and working directory.
    pub async fn exec_with_options(
        &self,
        cmd: Vec<String>,
        env: Vec<String>,
        working_dir: Option<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<ExecResult> {
        let started = std::time::Instant::now();
        let request = ExecRequest {
            cmd,
            timeout_ns: 0,
            env,
            working_dir,
            stdin,
            user: None,
            streaming: false,
        };

        let client = ExecClient::connect(&self.exec_socket).await?;
        let output = client.exec_command(&request).await?;
        let mut result = ExecResult::from(output);
        result.metrics.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Execute a command in streaming mode.
    ///
    /// Returns a `StreamingExec` handle that yields output chunks as they
    /// arrive from the guest. Use this for long-running commands or when
    /// you need real-time output.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use a3s_box_sdk::Sandbox;
    /// # async fn example(sandbox: &Sandbox) -> Result<(), Box<dyn std::error::Error>> {
    /// use a3s_box_core::exec::ExecEvent;
    ///
    /// let mut stream = sandbox.exec_stream("tail", &["-f", "/var/log/syslog"]).await?;
    /// while let Some(event) = stream.next_event().await? {
    ///     match event {
    ///         ExecEvent::Chunk(chunk) => {
    ///             print!("{}", String::from_utf8_lossy(&chunk.data));
    ///         }
    ///         ExecEvent::Exit(exit) => {
    ///             println!("Exited with code {}", exit.exit_code);
    ///         }
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn exec_stream(&self, cmd: &str, args: &[&str]) -> Result<StreamingExec> {
        let mut cmd_parts = vec![cmd.to_string()];
        cmd_parts.extend(args.iter().map(|a| a.to_string()));

        let request = ExecRequest {
            cmd: cmd_parts,
            timeout_ns: 0,
            env: Vec::new(),
            working_dir: None,
            stdin: None,
            user: None,
            streaming: true,
        };

        let client = ExecClient::connect(&self.exec_socket).await?;
        client.exec_stream(&request).await
    }

    /// Upload a file from the host into the sandbox.
    ///
    /// # Arguments
    /// * `data` - File contents
    /// * `guest_path` - Destination path inside the sandbox
    pub async fn upload(&self, data: &[u8], guest_path: &str) -> Result<()> {
        let request = FileRequest {
            op: FileOp::Upload,
            guest_path: guest_path.to_string(),
            data: Some(base64::engine::general_purpose::STANDARD.encode(data)),
        };

        let client = ExecClient::connect(&self.exec_socket).await?;
        let response = client.file_transfer(&request).await?;

        if !response.success {
            return Err(a3s_box_core::error::BoxError::ExecError(format!(
                "Upload failed: {}",
                response.error.unwrap_or_else(|| "unknown error".into())
            )));
        }

        Ok(())
    }

    /// Download a file from the sandbox to the host.
    ///
    /// # Arguments
    /// * `guest_path` - Path inside the sandbox to download
    ///
    /// # Returns
    /// Raw file contents as bytes.
    pub async fn download(&self, guest_path: &str) -> Result<Vec<u8>> {
        let request = FileRequest {
            op: FileOp::Download,
            guest_path: guest_path.to_string(),
            data: None,
        };

        let client = ExecClient::connect(&self.exec_socket).await?;
        let response = client.file_transfer(&request).await?;

        if !response.success {
            return Err(a3s_box_core::error::BoxError::ExecError(format!(
                "Download failed: {}",
                response.error.unwrap_or_else(|| "unknown error".into())
            )));
        }

        let data = response.data.ok_or_else(|| {
            a3s_box_core::error::BoxError::ExecError("Download response missing data".to_string())
        })?;

        base64::engine::general_purpose::STANDARD
            .decode(&data)
            .map_err(|e| {
                a3s_box_core::error::BoxError::ExecError(format!(
                    "Failed to decode downloaded file: {}",
                    e
                ))
            })
    }

    /// Open an interactive PTY session.
    ///
    /// Returns a `PtyClient` for bidirectional terminal I/O.
    pub async fn pty(&self, shell: &str, cols: u16, rows: u16) -> Result<PtyClient> {
        let mut client = PtyClient::connect(&self.pty_socket).await?;

        let request = a3s_box_core::pty::PtyRequest {
            cmd: vec![shell.to_string()],
            env: Vec::new(),
            working_dir: None,
            user: None,
            cols,
            rows,
        };
        client.send_request(&request).await?;

        Ok(client)
    }

    /// Stop the sandbox and release resources.
    pub async fn stop(mut self) -> Result<()> {
        tracing::info!(sandbox_id = %self.id, "Stopping sandbox");
        self.vm.destroy().await
    }

    /// Pause the sandbox (SIGSTOP).
    ///
    /// The sandbox remains in memory but all processes are frozen.
    /// Use `resume()` to continue execution.
    pub async fn pause(&self) -> Result<()> {
        tracing::info!(sandbox_id = %self.id, "Pausing sandbox");
        self.vm.pause().await
    }

    /// Resume a paused sandbox (SIGCONT).
    pub async fn resume(&self) -> Result<()> {
        tracing::info!(sandbox_id = %self.id, "Resuming sandbox");
        self.vm.resume().await
    }

    /// Check if the sandbox is running.
    pub async fn is_running(&self) -> bool {
        matches!(
            self.vm.state().await,
            a3s_box_runtime::BoxState::Ready | a3s_box_runtime::BoxState::Busy
        )
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_result_from_exec_output() {
        let output = ExecOutput {
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        };
        let result = ExecResult::from(output);
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.metrics.stdout_bytes, 6);
        assert_eq!(result.metrics.stderr_bytes, 0);
    }

    #[test]
    fn test_exec_result_nonzero_exit() {
        let output = ExecOutput {
            stdout: Vec::new(),
            stderr: b"not found\n".to_vec(),
            exit_code: 127,
        };
        let result = ExecResult::from(output);
        assert_eq!(result.exit_code, 127);
        assert_eq!(result.stderr, "not found\n");
        assert_eq!(result.metrics.stderr_bytes, 10);
    }

    #[test]
    fn test_exec_result_metrics_byte_counts() {
        let output = ExecOutput {
            stdout: vec![0u8; 1024],
            stderr: vec![0u8; 512],
            exit_code: 0,
        };
        let result = ExecResult::from(output);
        assert_eq!(result.metrics.stdout_bytes, 1024);
        assert_eq!(result.metrics.stderr_bytes, 512);
    }
}
