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

## Next

- [ ] OpenTelemetry spans (VM lifecycle: create → boot → ready)
