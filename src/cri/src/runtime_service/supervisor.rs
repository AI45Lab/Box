//! Container workload exit supervision for the CRI runtime service.
//!
//! Drives a running container exec/pty stream to completion: fans output out
//! to attach subscribers and the CRI log, records the exit, and emits the
//! corresponding container lifecycle event.

use std::sync::Arc;

use tokio::sync::{oneshot, Notify};

use crate::cri_api::ContainerEventType;
use crate::persistent_store::PersistentCriStore;

use super::convert::container_event_response;
use super::log_writer::CriLogWriter;
use super::{
    AttachStreamMap, AttachStreamSender, ContainerEventSender, LogReopenMap, WorkloadStdinMap,
    WorkloadStopMap,
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

    pub(super) async fn cancel(&mut self) -> a3s_box_core::error::Result<()> {
        match self {
            Self::Exec(stream) => stream.cancel().await,
            Self::Pty(stream) => stream.cancel().await,
        }
    }

    /// Request a flush of the guest's buffered output for a clean log-rotation
    /// boundary. Returns `true` if a `FlushAck` should be awaited (exec
    /// workloads), `false` if the workload does not support flushing (pty
    /// workloads keep the prior best-effort reopen behaviour).
    async fn flush(&mut self) -> a3s_box_core::error::Result<bool> {
        match self {
            Self::Exec(stream) => {
                stream.flush().await?;
                Ok(true)
            }
            Self::Pty(_) => Ok(false),
        }
    }
}

pub(super) struct ContainerExitSupervisor {
    pub(super) store: Arc<PersistentCriStore>,
    pub(super) attach_streams: AttachStreamMap,
    pub(super) workload_stdins: WorkloadStdinMap,
    pub(super) workload_stops: WorkloadStopMap,
    pub(super) log_reopens: LogReopenMap,
    pub(super) container_events: ContainerEventSender,
    pub(super) container_id: String,
    pub(super) sandbox_id: String,
    pub(super) log_path: String,
    /// The container log writer, opened eagerly by `start_container` so the
    /// log file exists the moment `StartContainer` returns (a caller may open
    /// it immediately, e.g. before the container has produced any output).
    pub(super) log_writer: Option<CriLogWriter>,
    pub(super) attach_tx: AttachStreamSender,
    pub(super) stop_rx: oneshot::Receiver<()>,
    pub(super) log_reopen: Arc<Notify>,
    pub(super) log_reopen_done: Arc<Notify>,
    pub(super) workload: SupervisedWorkload,
}

pub(super) fn spawn_container_exit_supervisor(supervisor: ContainerExitSupervisor) {
    tokio::spawn(async move {
        let ContainerExitSupervisor {
            store,
            attach_streams,
            workload_stdins,
            workload_stops,
            log_reopens,
            container_events,
            container_id,
            sandbox_id,
            log_path,
            log_writer,
            attach_tx,
            stop_rx,
            log_reopen,
            log_reopen_done,
            workload,
        } = supervisor;
        let mut workload = workload;
        let mut stop_rx = stop_rx;
        let mut stop_requested = false;
        // The writer was opened eagerly in start_container; the reopen path
        // below re-creates it from log_path if it was never opened.
        let mut log_writer = log_writer;
        let mut exit_code = -1;
        let mut oom_killed = false;
        // Set when the workload exits while we are draining output for a
        // log-rotation flush: reopen the log, then leave the supervision loop.
        let mut reopen_then_exit = false;

        loop {
            tokio::select! {
                _ = log_reopen.notified() => {
                    // CRI ReopenContainerLog: the kubelet rotated (renamed) the
                    // log file; reopen our writer at log_path so new output lands
                    // in a fresh file there.
                    //
                    // Before reopening, ask the guest to flush and drain every
                    // chunk it had buffered into the OLD writer, stopping at the
                    // FlushAck marker. This gives a definitive boundary so output
                    // produced before the rotation cannot leak into the new file.
                    let await_ack = match workload.flush().await {
                        Ok(await_ack) => await_ack,
                        Err(error) => {
                            tracing::warn!(
                                container_id = %container_id,
                                error = %error,
                                "Failed to send log-rotation flush; reopening without barrier"
                            );
                            false
                        }
                    };
                    if await_ack {
                        loop {
                            match workload.next_event().await {
                                Ok(Some(a3s_box_core::exec::ExecEvent::FlushAck)) => break,
                                Ok(Some(a3s_box_core::exec::ExecEvent::Chunk(chunk))) => {
                                    let _ = attach_tx.send(a3s_box_core::exec::ExecEvent::Chunk(chunk.clone()));
                                    if let Some(writer) = log_writer.as_mut() {
                                        if let Err(error) = writer.write_chunk(chunk.stream, &chunk.data).await {
                                            tracing::warn!(
                                                container_id = %container_id,
                                                log_path = %log_path,
                                                error = %error,
                                                "Failed to write CRI container log during flush; disabling log writes"
                                            );
                                            log_writer = None;
                                        }
                                    }
                                }
                                Ok(Some(a3s_box_core::exec::ExecEvent::Exit(exit))) => {
                                    exit_code = exit.exit_code;
                                    oom_killed = exit.oom_killed;
                                    reopen_then_exit = true;
                                    break;
                                }
                                Ok(None) => { reopen_then_exit = true; break; }
                                Err(error) => {
                                    tracing::warn!(
                                        container_id = %container_id,
                                        error = %error,
                                        "Container workload supervision failed during log-rotation flush; recording synthetic failure exit"
                                    );
                                    exit_code = 255;
                                    reopen_then_exit = true;
                                    break;
                                }
                            }
                        }
                    }
                    match log_writer.as_mut() {
                        Some(writer) => {
                            if let Err(error) = writer.reopen().await {
                                tracing::warn!(
                                    container_id = %container_id,
                                    log_path = %log_path,
                                    error = %error,
                                    "Failed to reopen CRI container log"
                                );
                            }
                        }
                        None => {
                            log_writer = CriLogWriter::open(&log_path).await.ok().flatten();
                        }
                    }
                    // Acknowledge so a synchronous ReopenContainerLog can return
                    // only now that the new log file is in place.
                    log_reopen_done.notify_one();
                    // If the workload exited while we were draining, finish up.
                    if reopen_then_exit {
                        break;
                    }
                }
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
                        Ok(Some(a3s_box_core::exec::ExecEvent::FlushAck)) => {
                            // Only meaningful while draining for a log rotation
                            // (handled in the reopen arm); ignore otherwise.
                        }
                        Ok(Some(a3s_box_core::exec::ExecEvent::Exit(exit))) => {
                            exit_code = exit.exit_code;
                            oom_killed = exit.oom_killed;
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
            a3s_box_core::exec::ExecExit {
                exit_code,
                oom_killed,
            },
        ));

        let finished_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let updated = store
            .mark_container_exited_if_running(&container_id, finished_ns, exit_code, oom_killed)
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
        log_reopens.write().await.remove(&container_id);
    });
}
