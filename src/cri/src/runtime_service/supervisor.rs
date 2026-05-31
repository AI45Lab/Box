//! Container workload exit supervision for the CRI runtime service.
//!
//! Drives a running container exec/pty stream to completion: fans output out
//! to attach subscribers and the CRI log, records the exit, and emits the
//! corresponding container lifecycle event.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::cri_api::ContainerEventType;
use crate::persistent_store::PersistentCriStore;

use super::convert::container_event_response;
use super::log_writer::CriLogWriter;
use super::{
    AttachStreamMap, AttachStreamSender, ContainerEventSender, WorkloadStdinMap, WorkloadStopMap,
};

pub(super) enum SupervisedWorkload {
    Exec(a3s_box_runtime::StreamingExec),
    Pty(a3s_box_runtime::StreamingPty),
}

impl SupervisedWorkload {
    async fn next_event(
        &mut self,
    ) -> a3s_box_core::error::Result<Option<a3s_box_core::exec::ExecEvent>> {
        match self {
            Self::Exec(stream) => stream.next_event().await,
            Self::Pty(stream) => stream.next_event().await,
        }
    }

    async fn cancel(&mut self) -> a3s_box_core::error::Result<()> {
        match self {
            Self::Exec(stream) => stream.cancel().await,
            Self::Pty(stream) => stream.cancel().await,
        }
    }
}

pub(super) struct ContainerExitSupervisor {
    pub(super) store: Arc<PersistentCriStore>,
    pub(super) attach_streams: AttachStreamMap,
    pub(super) workload_stdins: WorkloadStdinMap,
    pub(super) workload_stops: WorkloadStopMap,
    pub(super) container_events: ContainerEventSender,
    pub(super) container_id: String,
    pub(super) sandbox_id: String,
    pub(super) log_path: String,
    pub(super) attach_tx: AttachStreamSender,
    pub(super) stop_rx: oneshot::Receiver<()>,
    pub(super) workload: SupervisedWorkload,
}

pub(super) fn spawn_container_exit_supervisor(supervisor: ContainerExitSupervisor) {
    tokio::spawn(async move {
        let ContainerExitSupervisor {
            store,
            attach_streams,
            workload_stdins,
            workload_stops,
            container_events,
            container_id,
            sandbox_id,
            log_path,
            attach_tx,
            stop_rx,
            workload,
        } = supervisor;
        let mut workload = workload;
        let mut stop_rx = stop_rx;
        let mut stop_requested = false;
        let mut log_writer = match CriLogWriter::open(&log_path).await {
            Ok(log_writer) => log_writer,
            Err(error) => {
                tracing::warn!(
                    container_id = %container_id,
                    sandbox_id = %sandbox_id,
                    log_path = %log_path,
                    error = %error,
                    "Failed to open CRI container log"
                );
                None
            }
        };
        let mut exit_code = -1;

        loop {
            tokio::select! {
                stop = &mut stop_rx, if !stop_requested => {
                    stop_requested = true;
                    if stop.is_ok() {
                        tracing::info!(
                            container_id = %container_id,
                            sandbox_id = %sandbox_id,
                            "Stopping CRI container workload through streaming exec control"
                        );
                        if let Err(error) = workload.cancel().await {
                            tracing::warn!(
                                container_id = %container_id,
                                sandbox_id = %sandbox_id,
                                error = %error,
                                "Failed to send CRI container workload stop control"
                            );
                        }
                    }
                }
                event = workload.next_event() => {
                    match event {
                        Ok(Some(a3s_box_core::exec::ExecEvent::Chunk(chunk))) => {
                            let _ = attach_tx.send(a3s_box_core::exec::ExecEvent::Chunk(chunk.clone()));
                            if let Some(writer) = log_writer.as_mut() {
                                if let Err(error) = writer.write_chunk(chunk.stream, &chunk.data).await {
                                    tracing::warn!(
                                        container_id = %container_id,
                                        sandbox_id = %sandbox_id,
                                        log_path = %log_path,
                                        error = %error,
                                        "Failed to write CRI container log; disabling log writes for this workload"
                                    );
                                    log_writer = None;
                                }
                            }
                        }
                        Ok(Some(a3s_box_core::exec::ExecEvent::Exit(exit))) => {
                            exit_code = exit.exit_code;
                            break;
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(
                                container_id = %container_id,
                                sandbox_id = %sandbox_id,
                                error = %error,
                                "Container workload supervision failed; recording synthetic failure exit"
                            );
                            exit_code = 255;
                            break;
                        }
                    }
                }
            }
        }

        if let Some(writer) = log_writer.as_mut() {
            if let Err(error) = writer.flush_partials().await {
                tracing::warn!(
                    container_id = %container_id,
                    sandbox_id = %sandbox_id,
                    log_path = %log_path,
                    error = %error,
                    "Failed to flush CRI container log"
                );
            }
        }

        if exit_code < 0 {
            tracing::warn!(
                container_id = %container_id,
                sandbox_id = %sandbox_id,
                exit_code,
                "Container workload stream ended without a valid exit code; recording synthetic failure"
            );
            exit_code = 255;
        }
        let _ = attach_tx.send(a3s_box_core::exec::ExecEvent::Exit(
            a3s_box_core::exec::ExecExit { exit_code },
        ));

        let finished_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let updated = store
            .mark_container_exited_if_running(&container_id, finished_ns, exit_code)
            .await;

        if updated {
            tracing::info!(
                container_id = %container_id,
                sandbox_id = %sandbox_id,
                exit_code,
                "Container workload exited"
            );
            let _ = container_events.send(container_event_response(
                &container_id,
                &sandbox_id,
                ContainerEventType::ContainerStoppedEvent,
                finished_ns,
                "ContainerStopped",
                format!("Container workload exited with code {exit_code}"),
            ));
        } else {
            tracing::debug!(
                container_id = %container_id,
                sandbox_id = %sandbox_id,
                exit_code,
                "Container exit was already recorded by another lifecycle path"
            );
        }

        attach_streams.write().await.remove(&container_id);
        workload_stdins.write().await.remove(&container_id);
        workload_stops.write().await.remove(&container_id);
    });
}
