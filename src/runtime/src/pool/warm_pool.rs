//! WarmPool — Pre-warmed pool of ready-to-use MicroVMs.
//!
//! Maintains a set of pre-booted VMs in `Ready` state so that
//! `acquire()` can return a VM instantly without waiting for boot.

use std::sync::Arc;
use std::time::Instant;

use a3s_box_core::config::{BoxConfig, PoolConfig};
use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::event::{BoxEvent, EventEmitter};
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::pool::scaler::PoolScaler;
use crate::vm::VmManager;

/// A pre-warmed VM waiting in the pool.
struct WarmVm {
    /// The ready VM manager instance.
    vm: VmManager,
    /// When this VM was added to the pool.
    created_at: Instant,
}

/// Statistics about the warm pool.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Number of idle VMs ready for acquisition.
    pub idle_count: usize,
    /// Total number of VMs created by this pool (including acquired ones).
    pub total_created: u64,
    /// Total number of VMs acquired from the pool.
    pub total_acquired: u64,
    /// Total number of VMs released back to the pool.
    pub total_released: u64,
    /// Total number of VMs evicted due to idle TTL.
    pub total_evicted: u64,
}

/// A pre-warmed pool of ready-to-use MicroVMs.
///
/// The pool maintains `min_idle` VMs in `Ready` state. When a VM is
/// acquired, the pool spawns a replacement in the background. Idle VMs
/// that exceed `idle_ttl_secs` are automatically evicted.
///
/// # Usage
///
/// ```ignore
/// let pool = WarmPool::start(pool_config, box_config, emitter).await?;
/// let vm = pool.acquire().await?;  // Instant if pool has capacity
/// // ... use vm ...
/// pool.release(vm).await?;         // Return to pool or destroy
/// pool.drain().await?;             // Graceful shutdown
/// ```
pub struct WarmPool {
    /// Pool configuration.
    config: PoolConfig,
    /// Base BoxConfig template for creating new VMs.
    box_config: BoxConfig,
    /// Idle VMs ready for acquisition.
    idle: Arc<Mutex<Vec<WarmVm>>>,
    /// Pool statistics.
    stats: Arc<Mutex<PoolStats>>,
    /// Event emitter for pool lifecycle events.
    event_emitter: EventEmitter,
    /// Background replenishment task handle.
    replenish_handle: Option<JoinHandle<()>>,
    /// Shutdown signal sender.
    shutdown_tx: watch::Sender<bool>,
    /// Shutdown signal receiver (cloned for background task).
    shutdown_rx: watch::Receiver<bool>,
    /// Autoscaler for dynamic min_idle adjustment (None if scaling disabled).
    scaler: Option<Arc<Mutex<PoolScaler>>>,
    /// Prometheus metrics (optional).
    metrics: Option<crate::prom::RuntimeMetrics>,
    /// Snapshot-fork template (built lazily on first fill when
    /// `config.snapshot_fork`): the file-backed RAM image + state file every other
    /// pool VM restores from. `None` until the first template is built.
    template: Arc<Mutex<Option<PoolTemplate>>>,
}

/// A built snapshot-fork template: the shared RAM image + state file that pool VMs
/// restore from (MAP_PRIVATE CoW of the RAM file).
#[derive(Clone)]
struct PoolTemplate {
    mem_file: String,
    state_file: String,
}

impl WarmPool {
    /// Create and start the warm pool.
    ///
    /// Spawns `min_idle` VMs in the background and starts the
    /// replenishment/eviction loop.
    pub async fn start(
        config: PoolConfig,
        box_config: BoxConfig,
        event_emitter: EventEmitter,
    ) -> Result<Self> {
        if config.max_size == 0 {
            return Err(BoxError::PoolError(
                "Pool max_size must be greater than 0".to_string(),
            ));
        }
        if config.min_idle > config.max_size {
            return Err(BoxError::PoolError(format!(
                "Pool min_idle ({}) cannot exceed max_size ({})",
                config.min_idle, config.max_size
            )));
        }

        let idle = Arc::new(Mutex::new(Vec::with_capacity(config.max_size)));
        let stats = Arc::new(Mutex::new(PoolStats {
            idle_count: 0,
            total_created: 0,
            total_acquired: 0,
            total_released: 0,
            total_evicted: 0,
        }));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let scaler = if config.scaling.enabled {
            Some(Arc::new(Mutex::new(PoolScaler::new(
                config.scaling.clone(),
                config.min_idle,
                config.max_size,
            ))))
        } else {
            None
        };

        let mut pool = Self {
            config,
            box_config,
            idle,
            stats,
            event_emitter,
            replenish_handle: None,
            shutdown_tx,
            shutdown_rx,
            scaler,
            metrics: None,
            template: Arc::new(Mutex::new(None)),
        };

        // Initial fill
        pool.fill_to_min().await;

        // Start background maintenance loop
        let handle = pool.spawn_maintenance_loop();
        pool.replenish_handle = Some(handle);

        tracing::info!(
            min_idle = pool.config.min_idle,
            max_size = pool.config.max_size,
            idle_ttl_secs = pool.config.idle_ttl_secs,
            "Warm pool started"
        );

        Ok(pool)
    }

    /// Attach Prometheus metrics to this pool.
    pub fn set_metrics(&mut self, metrics: crate::prom::RuntimeMetrics) {
        metrics.warm_pool_capacity.set(self.config.max_size as i64);
        self.metrics = Some(metrics);
    }

    /// Acquire a ready VM from the pool.
    ///
    /// If an idle VM is available, returns it immediately.
    /// Otherwise, boots a new VM on demand (slower path).
    pub async fn acquire(&self) -> Result<VmManager> {
        // Try to pop an idle VM
        {
            let mut idle = self.idle.lock().await;
            if let Some(warm_vm) = idle.pop() {
                let mut stats = self.stats.lock().await;
                stats.total_acquired += 1;
                stats.idle_count = idle.len();

                // Record hit for autoscaler
                if let Some(ref scaler) = self.scaler {
                    scaler.lock().await.record_acquire(true);
                }

                if let Some(ref m) = self.metrics {
                    m.warm_pool_hits.inc();
                    m.warm_pool_size.set(idle.len() as i64);
                }

                self.event_emitter.emit(BoxEvent::with_string(
                    "pool.vm.acquired",
                    format!("Acquired VM {} from pool", warm_vm.vm.box_id()),
                ));

                tracing::debug!(
                    box_id = %warm_vm.vm.box_id(),
                    idle_remaining = idle.len(),
                    "Acquired VM from warm pool"
                );

                return Ok(warm_vm.vm);
            }
        }

        // No idle VM available — boot one on demand (miss)
        tracing::info!("No idle VM in pool, booting on demand");

        // Record miss for autoscaler
        if let Some(ref scaler) = self.scaler {
            scaler.lock().await.record_acquire(false);
        }

        if let Some(ref m) = self.metrics {
            m.warm_pool_misses.inc();
        }

        let vm = self.boot_new_vm().await?;

        let mut stats = self.stats.lock().await;
        stats.total_acquired += 1;

        Ok(vm)
    }

    /// Release a VM back to the pool.
    ///
    /// If the pool is at capacity, the VM is destroyed instead.
    pub async fn release(&self, vm: VmManager) -> Result<()> {
        let mut idle = self.idle.lock().await;

        if idle.len() >= self.config.max_size {
            // Pool is full — destroy the VM
            drop(idle); // Release lock before async destroy
            let mut vm = vm;
            vm.destroy().await?;

            tracing::debug!(
                box_id = %vm.box_id(),
                "Pool full, destroyed released VM"
            );
            return Ok(());
        }

        let box_id = vm.box_id().to_string();
        idle.push(WarmVm {
            vm,
            created_at: Instant::now(),
        });

        let mut stats = self.stats.lock().await;
        stats.total_released += 1;
        stats.idle_count = idle.len();

        if let Some(ref m) = self.metrics {
            m.warm_pool_size.set(idle.len() as i64);
        }

        self.event_emitter.emit(BoxEvent::with_string(
            "pool.vm.released",
            format!("Released VM {} back to pool", box_id),
        ));

        tracing::debug!(
            box_id = %box_id,
            idle_count = idle.len(),
            "Released VM back to warm pool"
        );

        Ok(())
    }

    /// Get current pool statistics.
    pub async fn stats(&self) -> PoolStats {
        self.stats.lock().await.clone()
    }

    /// Get the number of idle VMs currently in the pool.
    pub async fn idle_count(&self) -> usize {
        self.idle.lock().await.len()
    }

    /// Signal the pool to shutdown. This signals the background task to stop
    /// replenishing and sets the shutdown flag. VMs will continue to exist
    /// until the pool is drained or dropped.
    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        tracing::info!("Warm pool shutdown signaled");
    }

    /// Gracefully drain all VMs and stop the pool.
    pub async fn drain(&mut self) -> Result<()> {
        // Signal shutdown to background task
        let _ = self.shutdown_tx.send(true);

        // Wait for background task to finish
        if let Some(handle) = self.replenish_handle.take() {
            let _ = handle.await;
        }

        // Destroy all idle VMs
        let mut idle = self.idle.lock().await;
        let count = idle.len();

        for warm_vm in idle.drain(..) {
            let mut vm = warm_vm.vm;
            if let Err(e) = vm.destroy().await {
                tracing::warn!(
                    box_id = %vm.box_id(),
                    error = %e,
                    "Failed to destroy pooled VM during drain"
                );
            }
        }

        let mut stats = self.stats.lock().await;
        stats.idle_count = 0;

        self.event_emitter.emit(BoxEvent::empty("pool.drained"));

        tracing::info!(destroyed = count, "Warm pool drained");

        Ok(())
    }

    /// Destroy all idle VMs without consuming the pool (`&self`), so it can be
    /// shut down from behind an `Arc` (e.g. a daemon serving concurrent requests).
    /// Pair with [`Self::signal_shutdown`] first to stop the background replenisher;
    /// its task then exits on its own (it watches the shutdown channel).
    pub async fn drain_idle(&self) -> Result<()> {
        let mut idle = self.idle.lock().await;
        let count = idle.len();
        for warm_vm in idle.drain(..) {
            let mut vm = warm_vm.vm;
            if let Err(e) = vm.destroy().await {
                tracing::warn!(
                    box_id = %vm.box_id(),
                    error = %e,
                    "Failed to destroy pooled VM during drain_idle"
                );
            }
        }
        self.stats.lock().await.idle_count = 0;
        tracing::info!(destroyed = count, "Warm pool idle VMs drained");
        Ok(())
    }

    /// Remove and destroy specific idle VMs by their box IDs.
    ///
    /// Used when `fill_to_min` partially fails and needs to rollback
    /// successfully added VMs.
    async fn remove_idle_vms(&self, box_ids: &[String]) {
        // First pass: collect indices of VMs to remove
        let indices_to_remove: Vec<usize> = {
            let idle = self.idle.lock().await;
            idle.iter()
                .enumerate()
                .filter(|(_, wm)| box_ids.iter().any(|id| id == wm.vm.box_id()))
                .map(|(i, _)| i)
                .collect()
        };

        if indices_to_remove.is_empty() {
            return;
        }

        // Second pass: remove and collect VMs to destroy
        // We do this in reverse order to avoid index shifting issues
        let mut to_destroy: Vec<WarmVm> = Vec::new();
        {
            let mut idle = self.idle.lock().await;
            for idx in indices_to_remove.into_iter().rev() {
                if idx < idle.len() {
                    let warm_vm = idle.remove(idx);
                    to_destroy.push(warm_vm);
                }
            }
        }

        // Update stats before destroying (approximate, since VMs still exist in to_destroy)
        {
            let idle_count = self.idle.lock().await.len();
            if let Ok(mut stats) = self.stats.try_lock() {
                stats.idle_count = idle_count;
            }
        }

        // Destroy collected VMs (outside of pool lock)
        for warm_vm in to_destroy {
            let box_id = warm_vm.vm.box_id().to_string();
            let mut vm = warm_vm.vm;
            if let Err(e) = vm.destroy().await {
                tracing::warn!(
                    box_id = %box_id,
                    error = %e,
                    "Failed to destroy VM during fill_to_min rollback"
                );
            } else {
                tracing::debug!(box_id = %box_id, "Destroyed VM during fill_to_min rollback");
            }
        }
    }

    /// Boot a new VM using the pool's template config.
    async fn boot_new_vm(&self) -> Result<VmManager> {
        let vm = Self::boot_or_restore(
            self.config.snapshot_fork,
            &self.box_config,
            &self.event_emitter,
            &self.template,
        )
        .await?;

        let mut stats = self.stats.lock().await;
        stats.total_created += 1;

        self.event_emitter.emit(BoxEvent::with_string(
            "pool.vm.created",
            format!("Booted new VM {}", vm.box_id()),
        ));

        Ok(vm)
    }

    /// Fill one slot: restore from the snapshot-fork template when enabled, else cold
    /// boot. Static so both `boot_new_vm` and the background replenish task use it.
    async fn boot_or_restore(
        snapshot_fork: bool,
        box_config: &BoxConfig,
        event_emitter: &EventEmitter,
        template: &Arc<Mutex<Option<PoolTemplate>>>,
    ) -> Result<VmManager> {
        if snapshot_fork {
            let tpl = Self::ensure_template(box_config, event_emitter, template).await?;
            let mut cfg = box_config.clone();
            cfg.snapshot_mem_file = Some(tpl.mem_file.clone());
            cfg.restore_from = Some(tpl.state_file.clone());
            cfg.snapshot_sock = None;
            let mut vm = VmManager::new(cfg, event_emitter.clone());
            vm.boot().await?;
            Ok(vm)
        } else {
            let mut vm = VmManager::new(box_config.clone(), event_emitter.clone());
            vm.boot().await?;
            Ok(vm)
        }
    }

    /// Build the snapshot-fork template once (lazily): cold-boot one VM with
    /// file-backed RAM, snapshot it, tear down the source. Concurrent callers wait on
    /// the lock and reuse the first-built template.
    async fn ensure_template(
        box_config: &BoxConfig,
        event_emitter: &EventEmitter,
        template: &Arc<Mutex<Option<PoolTemplate>>>,
    ) -> Result<PoolTemplate> {
        let mut guard = template.lock().await;
        if let Some(t) = guard.as_ref() {
            return Ok(t.clone());
        }

        let dir = a3s_box_core::dirs_home().join("pool").join(format!(
            "tpl-{:016x}",
            crate::vm::fnv1a_hash(&box_config.image)
        ));
        std::fs::create_dir_all(&dir).map_err(BoxError::IoError)?;
        let mem_file = dir.join("template.ram");
        let sock = dir.join("template.sock");
        let state_file = dir.join("template.state");
        let _ = std::fs::remove_file(&sock);

        // Cold-boot the source as a snapshot TEMPLATE (file-backed RAM + trigger sock).
        let mut cfg = box_config.clone();
        cfg.snapshot_mem_file = Some(mem_file.to_string_lossy().into_owned());
        cfg.snapshot_sock = Some(sock.to_string_lossy().into_owned());
        cfg.restore_from = None;
        let mut src = VmManager::new(cfg, event_emitter.clone());
        src.boot().await?;

        // Trigger the snapshot over libkrun's socket, then tear down the source (it is
        // left paused by the snapshot; the RAM + state files are the template).
        Self::trigger_snapshot(&sock, &state_file).await?;
        let _ = src.destroy_with_timeout(2000).await;

        let tpl = PoolTemplate {
            mem_file: mem_file.to_string_lossy().into_owned(),
            state_file: state_file.to_string_lossy().into_owned(),
        };
        *guard = Some(tpl.clone());

        event_emitter.emit(BoxEvent::with_string(
            "pool.template.built",
            format!("Snapshot-fork template built at {}", dir.display()),
        ));

        Ok(tpl)
    }

    /// Send a `snapshot <state>` request to libkrun's per-template trigger socket and
    /// wait for the `ok` reply (the socket appears once the template's vCPUs run).
    ///
    /// Snapshot-fork is a Linux/KVM (Unix) feature; on non-Unix hosts the trigger
    /// socket does not exist, so this is unavailable (see the `not(unix)` stub).
    #[cfg(unix)]
    async fn trigger_snapshot(sock: &std::path::Path, state_file: &std::path::Path) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // The socket is bound by libkrun after the guest starts; poll briefly.
        let mut stream = None;
        for _ in 0..200 {
            match tokio::net::UnixStream::connect(sock).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
        let mut stream = stream.ok_or_else(|| {
            BoxError::PoolError(format!("snapshot socket {} never appeared", sock.display()))
        })?;
        let cmd = format!("snapshot {}\n", state_file.display());
        stream
            .write_all(cmd.as_bytes())
            .await
            .map_err(BoxError::IoError)?;
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.map_err(BoxError::IoError)?;
        let reply = String::from_utf8_lossy(&buf[..n]);
        if reply.trim() == "ok" {
            Ok(())
        } else {
            Err(BoxError::PoolError(format!(
                "snapshot trigger failed: {}",
                reply.trim()
            )))
        }
    }

    /// Non-Unix stub: snapshot-fork relies on libkrun's Unix trigger socket and KVM
    /// state save/restore, neither of which exist on Windows. `--snapshot-fork` is
    /// Linux/KVM-only, so this path is never reached there in practice.
    #[cfg(not(unix))]
    async fn trigger_snapshot(
        _sock: &std::path::Path,
        _state_file: &std::path::Path,
    ) -> Result<()> {
        Err(BoxError::PoolError(
            "snapshot-fork is only supported on Linux/KVM hosts".to_string(),
        ))
    }

    /// Fill the pool to the minimum idle count.
    async fn fill_to_min(&self) {
        let current = self.idle.lock().await.len();
        let needed = self.config.min_idle.saturating_sub(current);

        if needed == 0 {
            return;
        }

        tracing::debug!(
            current,
            needed,
            min_idle = self.config.min_idle,
            "Replenishing warm pool"
        );

        // Track VMs added in this fill attempt so we can clean up on failure.
        let mut added_ids: Vec<String> = Vec::new();

        for _ in 0..needed {
            match self.boot_new_vm().await {
                Ok(vm) => {
                    let box_id = vm.box_id().to_string();
                    let mut idle = self.idle.lock().await;
                    idle.push(WarmVm {
                        vm,
                        created_at: Instant::now(),
                    });
                    let mut stats = self.stats.lock().await;
                    stats.idle_count = idle.len();
                    added_ids.push(box_id.clone());

                    tracing::debug!(box_id = %box_id, "Added VM to warm pool");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to boot VM for warm pool");
                    // Clean up any VMs that were successfully added before this failure.
                    if !added_ids.is_empty() {
                        tracing::info!(
                            count = added_ids.len(),
                            "Cleaning up VMs added before fill_to_min failed"
                        );
                        self.remove_idle_vms(&added_ids).await;
                    }
                    break;
                }
            }
        }

        self.event_emitter.emit(BoxEvent::empty("pool.replenish"));
    }

    /// Spawn the background maintenance loop.
    ///
    /// Periodically checks for:
    /// 1. Autoscaler evaluation → adjust min_idle dynamically
    /// 2. Pool below min_idle → replenish
    /// 3. Idle VMs past TTL → evict
    fn spawn_maintenance_loop(&self) -> JoinHandle<()> {
        let idle = Arc::clone(&self.idle);
        let stats = Arc::clone(&self.stats);
        let config = self.config.clone();
        let box_config = self.box_config.clone();
        let event_emitter = self.event_emitter.clone();
        let mut shutdown_rx = self.shutdown_rx.clone();
        let scaler = self.scaler.clone();
        let template = Arc::clone(&self.template);

        tokio::spawn(async move {
            let check_interval = std::time::Duration::from_secs(
                // Check every 1/5 of TTL, minimum 5 seconds
                if config.idle_ttl_secs > 0 {
                    (config.idle_ttl_secs / 5).max(5)
                } else {
                    30
                },
            );

            // Dynamic min_idle starts from config, adjusted by scaler
            let mut effective_min_idle = config.min_idle;

            loop {
                tokio::select! {
                    result = shutdown_rx.changed() => {
                        if result.is_ok() && *shutdown_rx.borrow() {
                            tracing::debug!("Pool maintenance loop shutting down");
                            break;
                        }
                    }
                    _ = tokio::time::sleep(check_interval) => {
                        // Evict expired VMs
                        if config.idle_ttl_secs > 0 {
                            Self::evict_expired_static(
                                &idle,
                                &stats,
                                &event_emitter,
                                config.idle_ttl_secs,
                            ).await;
                        }

                        // Evaluate autoscaler
                        if let Some(ref scaler) = scaler {
                            let mut s = scaler.lock().await;
                            let decision = s.evaluate();
                            let new_min = s.current_min_idle();
                            if new_min != effective_min_idle {
                                tracing::info!(
                                    old_min_idle = effective_min_idle,
                                    new_min_idle = new_min,
                                    ?decision,
                                    "Autoscaler adjusted min_idle"
                                );
                                event_emitter.emit(BoxEvent::with_string(
                                    "pool.autoscale",
                                    format!(
                                        "min_idle adjusted {} → {} ({:?})",
                                        effective_min_idle, new_min, decision
                                    ),
                                ));
                                effective_min_idle = new_min;
                            }
                        }

                        // Replenish if below effective min_idle
                        let current = idle.lock().await.len();
                        if current < effective_min_idle {
                            let needed = effective_min_idle - current;
                            tracing::debug!(current, needed, min_idle = effective_min_idle, "Replenishing warm pool");

                            // Fill the `needed` slots CONCURRENTLY rather than one
                            // boot at a time — a snapshot-fork restore (or even a cold
                            // boot) overlaps its readiness wait, so a batch fills in
                            // roughly one boot's time instead of N×. For snapshot-fork
                            // the first task builds the template under ensure_template's
                            // lock; the rest wait then restore in parallel.
                            let mut set = tokio::task::JoinSet::new();
                            for _ in 0..needed {
                                let sf = config.snapshot_fork;
                                let bc = box_config.clone();
                                let ee = event_emitter.clone();
                                let tpl = Arc::clone(&template);
                                set.spawn(async move {
                                    WarmPool::boot_or_restore(sf, &bc, &ee, &tpl).await
                                });
                            }
                            while let Some(joined) = set.join_next().await {
                                match joined {
                                    Ok(Ok(vm)) => {
                                        let box_id = vm.box_id().to_string();
                                        let mut pool = idle.lock().await;
                                        pool.push(WarmVm {
                                            vm,
                                            created_at: Instant::now(),
                                        });
                                        let mut s = stats.lock().await;
                                        s.total_created += 1;
                                        s.idle_count = pool.len();
                                        drop(s);
                                        drop(pool);

                                        event_emitter.emit(BoxEvent::with_string(
                                            "pool.vm.created",
                                            format!("Replenished VM {}", box_id),
                                        ));
                                    }
                                    Ok(Err(e)) => {
                                        tracing::warn!(error = %e, "Failed to replenish warm pool");
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "Replenish task join error");
                                    }
                                }
                            }

                            event_emitter.emit(BoxEvent::empty("pool.replenish"));
                        }
                    }
                }
            }
        })
    }

    /// Static version of evict_expired for use in the spawned task.
    async fn evict_expired_static(
        idle: &Arc<Mutex<Vec<WarmVm>>>,
        stats: &Arc<Mutex<PoolStats>>,
        event_emitter: &EventEmitter,
        idle_ttl_secs: u64,
    ) {
        let ttl = std::time::Duration::from_secs(idle_ttl_secs);

        let mut pool = idle.lock().await;
        let mut kept = Vec::new();
        let mut expired = Vec::new();

        for warm_vm in pool.drain(..) {
            if warm_vm.created_at.elapsed() > ttl {
                expired.push(warm_vm);
            } else {
                kept.push(warm_vm);
            }
        }
        *pool = kept;
        let after_count = pool.len();
        drop(pool);

        let evicted_count = expired.len();
        for warm_vm in expired {
            let mut vm = warm_vm.vm;
            let _ = vm.destroy().await;
        }

        if evicted_count > 0 {
            let mut s = stats.lock().await;
            s.total_evicted += evicted_count as u64;
            s.idle_count = after_count;

            event_emitter.emit(BoxEvent::with_string(
                "pool.vm.evicted",
                format!("Evicted {} expired VMs", evicted_count),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::config::PoolConfig;

    fn test_pool_config(min_idle: usize, max_size: usize) -> PoolConfig {
        PoolConfig {
            enabled: true,
            min_idle,
            max_size,
            idle_ttl_secs: 300,
            ..Default::default()
        }
    }

    fn test_event_emitter() -> EventEmitter {
        EventEmitter::new(100)
    }

    // --- PoolConfig validation tests ---

    #[tokio::test]
    async fn test_pool_rejects_zero_max_size() {
        let config = test_pool_config(0, 0);
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        match result {
            Err(e) => assert!(e.to_string().contains("max_size must be greater than 0")),
            Ok(_) => panic!("Expected error for zero max_size"),
        }
    }

    #[tokio::test]
    async fn test_pool_rejects_min_idle_exceeds_max() {
        let config = test_pool_config(10, 5);
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        match result {
            Err(e) => assert!(e.to_string().contains("cannot exceed max_size")),
            Ok(_) => panic!("Expected error for min_idle > max_size"),
        }
    }

    // --- PoolStats tests ---

    #[test]
    fn test_pool_stats_default() {
        let stats = PoolStats {
            idle_count: 0,
            total_created: 0,
            total_acquired: 0,
            total_released: 0,
            total_evicted: 0,
        };
        assert_eq!(stats.idle_count, 0);
        assert_eq!(stats.total_created, 0);
    }

    #[test]
    fn test_pool_stats_clone() {
        let stats = PoolStats {
            idle_count: 3,
            total_created: 10,
            total_acquired: 7,
            total_released: 5,
            total_evicted: 2,
        };
        let cloned = stats.clone();
        assert_eq!(cloned.idle_count, 3);
        assert_eq!(cloned.total_created, 10);
        assert_eq!(cloned.total_acquired, 7);
        assert_eq!(cloned.total_released, 5);
        assert_eq!(cloned.total_evicted, 2);
    }

    #[test]
    fn test_pool_stats_debug() {
        let stats = PoolStats {
            idle_count: 1,
            total_created: 2,
            total_acquired: 3,
            total_released: 4,
            total_evicted: 5,
        };
        let debug = format!("{:?}", stats);
        assert!(debug.contains("idle_count"));
        assert!(debug.contains("total_created"));
    }

    // --- PoolConfig serialization tests ---

    #[test]
    fn test_pool_config_roundtrip() {
        let config = PoolConfig {
            enabled: true,
            min_idle: 3,
            max_size: 10,
            idle_ttl_secs: 600,
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: PoolConfig = serde_json::from_str(&json).unwrap();

        assert!(parsed.enabled);
        assert_eq!(parsed.min_idle, 3);
        assert_eq!(parsed.max_size, 10);
        assert_eq!(parsed.idle_ttl_secs, 600);
    }

    #[test]
    fn test_pool_config_default_values() {
        let config = PoolConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.min_idle, 1);
        assert_eq!(config.max_size, 5);
        assert_eq!(config.idle_ttl_secs, 300);
    }

    #[test]
    fn test_pool_config_deserialization_with_defaults() {
        let json = r#"{"enabled": true}"#;
        let config: PoolConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.min_idle, 1);
        assert_eq!(config.max_size, 5);
        assert_eq!(config.idle_ttl_secs, 300);
    }

    // --- PoolConfig validation edge cases ---

    #[tokio::test]
    async fn test_pool_accepts_min_idle_equals_max() {
        let config = test_pool_config(3, 3);
        // This should be accepted (min_idle == max_size is valid)
        // It will fail at boot (no shim), but config validation should pass
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        // The error should be about VM boot, not config validation
        match result {
            Err(e) => assert!(!e.to_string().contains("cannot exceed max_size")),
            Ok(mut pool) => {
                let _ = pool.drain().await;
            }
        }
    }

    #[tokio::test]
    async fn test_pool_accepts_min_idle_zero() {
        let config = test_pool_config(0, 5);
        // min_idle=0 means no pre-warming, should be valid
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        match result {
            Ok(mut pool) => {
                // Pool should start with 0 idle VMs
                assert_eq!(pool.idle_count().await, 0);
                let stats = pool.stats().await;
                assert_eq!(stats.idle_count, 0);
                assert_eq!(stats.total_created, 0);
                let _ = pool.drain().await;
            }
            Err(e) => {
                // If it fails, it should NOT be a config validation error
                assert!(!e.to_string().contains("max_size"));
                assert!(!e.to_string().contains("min_idle"));
            }
        }
    }

    // --- WarmPool internal state tests (using min_idle=0 to avoid boot) ---

    #[tokio::test]
    async fn test_pool_stats_initial() {
        let config = test_pool_config(0, 5);
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        if let Ok(mut pool) = result {
            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
            assert_eq!(stats.total_created, 0);
            assert_eq!(stats.total_acquired, 0);
            assert_eq!(stats.total_released, 0);
            assert_eq!(stats.total_evicted, 0);
            let _ = pool.drain().await;
        }
    }

    #[tokio::test]
    async fn test_pool_idle_count_initial() {
        let config = test_pool_config(0, 5);
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        if let Ok(mut pool) = result {
            assert_eq!(pool.idle_count().await, 0);
            let _ = pool.drain().await;
        }
    }

    #[tokio::test]
    async fn test_pool_drain_empty_pool() {
        let config = test_pool_config(0, 5);
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        if let Ok(mut pool) = result {
            // Draining an empty pool should succeed without error
            let drain_result = pool.drain().await;
            assert!(drain_result.is_ok());

            let stats = pool.stats().await;
            assert_eq!(stats.idle_count, 0);
        }
    }

    #[tokio::test]
    async fn test_pool_drain_emits_event() {
        let emitter = test_event_emitter();
        let mut receiver = emitter.subscribe();
        let config = test_pool_config(0, 5);

        let result = WarmPool::start(config, BoxConfig::default(), emitter).await;
        if let Ok(mut pool) = result {
            pool.drain().await.unwrap();

            // Check that pool.drained event was emitted
            let mut found_drain_event = false;
            // Drain all events from the receiver
            while let Ok(event) = receiver.try_recv() {
                if event.key == "pool.drained" {
                    found_drain_event = true;
                }
            }
            assert!(found_drain_event, "Expected pool.drained event");
        }
    }

    #[tokio::test]
    async fn test_pool_acquire_from_empty_pool_fails_without_shim() {
        let config = test_pool_config(0, 5);
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        if let Ok(pool) = result {
            // Acquire from empty pool should try to boot a VM, which will fail
            // because there's no shim binary available in test environment
            let acquire_result = pool.acquire().await;
            assert!(acquire_result.is_err());
        }
    }

    // --- Maintenance loop check interval calculation ---

    #[test]
    #[allow(clippy::unnecessary_min_or_max)]
    fn test_maintenance_check_interval_with_ttl() {
        // TTL = 300s → check every 60s (300/5)
        let interval = if 300_u64 > 0 {
            (300_u64 / 5).max(5)
        } else {
            30
        };
        assert_eq!(interval, 60);
    }

    #[test]
    #[allow(clippy::unnecessary_min_or_max)]
    fn test_maintenance_check_interval_short_ttl() {
        // TTL = 10s → check every 5s (min 5)
        let interval = if 10_u64 > 0 { (10_u64 / 5).max(5) } else { 30 };
        assert_eq!(interval, 5);
    }

    #[test]
    #[allow(clippy::unnecessary_min_or_max)]
    fn test_maintenance_check_interval_very_short_ttl() {
        // TTL = 1s → check every 5s (min 5)
        let interval = if 1_u64 > 0 { (1_u64 / 5).max(5) } else { 30 };
        assert_eq!(interval, 5);
    }

    #[test]
    #[allow(
        clippy::absurd_extreme_comparisons,
        clippy::erasing_op,
        clippy::unnecessary_min_or_max,
        unused_comparisons
    )]
    fn test_maintenance_check_interval_no_ttl() {
        // TTL = 0 → check every 30s
        let interval = if 0_u64 > 0 { (0_u64 / 5).max(5) } else { 30 };
        assert_eq!(interval, 30);
    }

    // --- WarmVm struct tests ---

    #[test]
    fn test_warm_vm_created_at_is_recent() {
        let before = Instant::now();
        let created_at = Instant::now();
        let after = Instant::now();

        assert!(created_at >= before);
        assert!(created_at <= after);
    }

    // --- PoolStats field coverage ---

    #[test]
    fn test_pool_stats_all_fields() {
        let stats = PoolStats {
            idle_count: 10,
            total_created: 100,
            total_acquired: 80,
            total_released: 70,
            total_evicted: 15,
        };

        assert_eq!(stats.idle_count, 10);
        assert_eq!(stats.total_created, 100);
        assert_eq!(stats.total_acquired, 80);
        assert_eq!(stats.total_released, 70);
        assert_eq!(stats.total_evicted, 15);

        // Verify debug output contains all fields
        let debug = format!("{:?}", stats);
        assert!(debug.contains("10"));
        assert!(debug.contains("100"));
        assert!(debug.contains("80"));
        assert!(debug.contains("70"));
        assert!(debug.contains("15"));
    }

    // Note: Full integration tests for acquire/release/drain with actual VMs
    // require a working VM runtime (shim binary + libkrun). These are tested
    // in integration tests with the full box environment. The unit tests here
    // validate configuration, statistics, error handling, and pool lifecycle
    // with min_idle=0 (no VM boot required).

    #[tokio::test]
    async fn test_pool_set_metrics_attaches() {
        let config = test_pool_config(0, 5);
        let result = WarmPool::start(config, BoxConfig::default(), test_event_emitter()).await;
        match result {
            Ok(mut pool) => {
                let metrics = crate::prom::RuntimeMetrics::new();
                pool.set_metrics(metrics.clone());
                assert!(pool.metrics.is_some());
                // Metrics start at zero
                assert_eq!(metrics.warm_pool_hits.get(), 0);
                assert_eq!(metrics.warm_pool_misses.get(), 0);
                assert_eq!(metrics.warm_pool_size.get(), 0);
                let _ = pool.drain().await;
            }
            Err(_) => {
                // Boot failure is acceptable in unit test environment
            }
        }
    }
}
