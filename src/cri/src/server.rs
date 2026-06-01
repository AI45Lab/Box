//! gRPC server setup for CRI services.
//!
//! Listens on a Unix domain socket for CRI RuntimeService and ImageService RPCs.
//! Also starts an HTTP streaming server for exec/attach/port-forward.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use a3s_box_runtime::oci::{ImageStore, RegistryAuth};

use crate::cri_api::image_service_server::ImageServiceServer;
use crate::cri_api::runtime_service_server::RuntimeServiceServer;
use crate::image_service::BoxImageService;
use crate::runtime_service::{BoxRuntimeService, CriRuntimeOptions};
use crate::streaming::StreamingServer;

/// CRI gRPC server configuration.
pub struct CriServer {
    /// Path to the Unix domain socket.
    socket_path: PathBuf,
    /// Shared image store.
    image_store: Arc<ImageStore>,
    /// Registry authentication.
    auth: RegistryAuth,
    /// Streaming server bind address.
    streaming_addr: SocketAddr,
    /// Runtime-level CRI defaults and RuntimeClass overrides.
    runtime_options: CriRuntimeOptions,
}

/// Default streaming server bind address.
const DEFAULT_STREAMING_ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], 18800);

impl CriServer {
    /// Create a new CRI server.
    pub fn new(socket_path: PathBuf, image_store: Arc<ImageStore>, auth: RegistryAuth) -> Self {
        Self {
            socket_path,
            image_store,
            auth,
            streaming_addr: SocketAddr::from(DEFAULT_STREAMING_ADDR),
            runtime_options: CriRuntimeOptions::default(),
        }
    }

    /// Set the streaming server bind address.
    pub fn with_streaming_addr(mut self, addr: SocketAddr) -> Self {
        self.streaming_addr = addr;
        self
    }

    /// Set runtime-level CRI defaults and RuntimeClass image overrides.
    pub fn with_runtime_options(mut self, options: CriRuntimeOptions) -> Self {
        self.runtime_options = options;
        self
    }

    /// Start serving CRI RPCs on the Unix socket.
    pub async fn serve(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Remove existing socket file if present
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Start streaming server
        let streaming_server = StreamingServer::new(self.streaming_addr).bind().await?;
        let streaming_handle = streaming_server.handle();

        tokio::spawn(async move {
            if let Err(e) = streaming_server.serve().await {
                tracing::error!(error = %e, "CRI streaming server failed");
            }
        });

        let runtime_service = BoxRuntimeService::new(
            self.image_store.clone(),
            self.auth.clone(),
            streaming_handle,
        )
        .with_runtime_options(self.runtime_options.clone());
        runtime_service.load_state().await;
        let image_service = BoxImageService::new(self.image_store.clone(), self.auth.clone());

        let uds = UnixListener::bind(&self.socket_path)?;
        let uds_stream = UnixListenerStream::new(uds);

        tracing::info!(
            socket = %self.socket_path.display(),
            streaming_addr = %self.streaming_addr,
            "CRI server listening"
        );

        // Keep a handle so we can reap sandbox VMs once the server stops; the
        // service itself is moved into the gRPC server below.
        let shutdown_service = runtime_service.clone();
        Server::builder()
            .add_service(RuntimeServiceServer::new(runtime_service))
            .add_service(ImageServiceServer::new(image_service))
            .serve_with_incoming_shutdown(uds_stream, shutdown_signal())
            .await?;

        // The server stopped (SIGTERM/SIGINT): tear down sandbox VMs and unmount
        // their overlays so they do not orphan across restarts.
        tracing::info!("CRI server stopping — reaping sandbox VMs");
        shutdown_service.shutdown_all_sandboxes().await;

        Ok(())
    }
}

/// Resolves when the process receives a termination signal, driving a graceful
/// gRPC server shutdown so the CRI can reap its sandbox VMs.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
            (Ok(mut sigterm), Ok(mut sigint)) => {
                tokio::select! {
                    _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down CRI"),
                    _ = sigint.recv() => tracing::info!("Received SIGINT, shutting down CRI"),
                }
            }
            _ => {
                tracing::error!("Failed to install signal handlers; graceful shutdown disabled");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Received Ctrl-C, shutting down CRI");
    }
}
