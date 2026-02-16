# Box Next Steps

## Completed ✅

- [x] Add a3s-transport dependency to Box workspace
- [x] Migrate Exec server (guest + host) to Frame protocol
- [x] Migrate PTY protocol (host-side FrameReader/FrameWriter)
- [x] Migrate Attest protocol (Frame inside TLS tunnel)
- [x] Embedded sandbox SDK (`a3s-box-sdk` crate: BoxSdk, Sandbox, SandboxOptions)
- [x] Guest-side TEE self-detection API (`detect_tee()`, `TeeCapability`, `TeeType` in core)
- [x] AgentClient health check migration (HTTP → Frame Heartbeat on exec server)
- [x] Prometheus metrics (`RuntimeMetrics`: VM lifecycle, exec, image, warm pool)
- [x] Instrument VM boot, exec, destroy with Prometheus metrics
- [x] OpenTelemetry spans (VM lifecycle: `vm_boot` → `prepare_layout` → `vm_start` → `wait_for_ready`, exec, destroy)
- [x] Autoscaler with warm pool pressure-based scaling (`ScalingPolicy`, `PoolScaler`, miss rate window)
- [x] Seccomp profiles, no-new-privileges, capability dropping (default BPF filter, `SecurityConfig`, env var bridge)

- [x] Image signing (cosign-compatible `SignaturePolicy`, `VerifyResult`, registry signature fetch, payload verification)
- [x] Multi-container orchestration (compose YAML: `ComposeConfig`, `ComposeProject`, topological boot order, `a3s-box compose up/down/ps/config`)
- [x] Buildx multi-platform builds (`Platform` type, `--platform` flag, parameterized OCI config architecture, Image Index with platform annotations)
- [x] Audit logging (`AuditEvent`, `AuditLog` with rotation, `AuditQuery` with filters, `a3s-box audit` CLI)
- [x] Network isolation policies (`NetworkPolicy`, `IsolationMode`: None/Strict/Custom, `PolicyRule` with from/to/ports/action, policy-aware peer discovery)
- [x] VM snapshot/restore (configuration-based `SnapshotStore`, `SnapshotMetadata`, rootfs copy, `snapshot create/restore/ls/rm/inspect` CLI, pruning)
- [x] KBS (Key Broker Service) integration (`KbsClient`, `KbsConfig`, RATS challenge-response, resource path parsing)
- [x] Periodic re-attestation (`ReattestConfig`, `ReattestState`, configurable interval/threshold/action, grace period)
- [x] Version-based rollback protection for sealed storage (`VersionStore`, `VersionedSealedData`, monotonic version counter)
- [x] Scale API and instance readiness signaling (`ScaleRequest`/`ScaleResponse`, `ScaleManager`, `InstanceEvent` lifecycle, `InstanceRegistration`)
- [x] Instance health reporting (`ServiceHealth` aggregation: avg CPU, total memory, inflight requests, unhealthy count)
- [x] Graceful scale-down (`start_drain()` → `is_drain_complete()` → `complete_drain()`, Draining state in lifecycle)
- [x] Instance self-registration (`InstanceRegistry` with heartbeat, stale eviction, per-host/per-service queries)
- [x] Warm pool auto-scaling with Gateway pressure signals (blended effective miss rate, pre-warm on rising traffic)
- [x] Kubernetes Operator (`BoxAutoscaler` CRD, `AutoscalerController` with ratio-based reconciliation, multi-metric, stabilization windows)

## Next — Embedded SDK Parity (boxlite feature parity)

- [x] Streaming exec (stdout/stderr streaming via Frame protocol, `StreamingExecResult` with async iterator)
- [x] File transfer (upload/download files to/from sandbox, `Sandbox::upload()` / `Sandbox::download()`)
- [x] Port forwarding (expose guest ports to host, `SandboxOptions::port_forwards`, TCP proxy)
- [x] Persistent workspaces (named workspaces that survive sandbox restart, `WorkspaceConfig`, volume reuse)
- [x] Execution metrics (per-exec CPU/memory/duration stats, `ExecMetrics` in `ExecResult`)

## Next — Multi-Language SDKs

- [x] Python SDK (`a3s-box` on PyPI: BoxSdk, Sandbox, streaming exec, file transfer, 25 tests)
- [x] TypeScript SDK (`@a3s-lab/box` on npm: BoxSdk, Sandbox, streaming exec, file transfer, 21 tests)

## Later

- [ ] Real hardware testing on AMD SEV-SNP (Azure DCasv5 / bare-metal EPYC)