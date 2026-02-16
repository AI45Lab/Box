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

## Next

