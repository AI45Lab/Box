import { describe, it, expect } from "vitest";
import {
  FrameType,
  encodeFrame,
  dataFrame,
  controlFrame,
  heartbeatFrame,
  errorFrame,
  HEADER_SIZE,
} from "../src/transport.js";
import { StreamType } from "../src/stream.js";
import { DEFAULT_OPTIONS } from "../src/options.js";
import type {
  SandboxOptions,
  MountSpec,
  PortForward,
  WorkspaceConfig,
} from "../src/options.js";
import type { ExecResult, ExecMetrics } from "../src/sandbox.js";

describe("transport", () => {
  it("encodes a data frame", () => {
    const payload = Buffer.from("hello");
    const frame = dataFrame(payload);
    const encoded = encodeFrame(frame);

    expect(encoded.length).toBe(HEADER_SIZE + payload.length);
    expect(encoded.readUInt8(0)).toBe(FrameType.Data);
    expect(encoded.readUInt32BE(1)).toBe(payload.length);
    expect(encoded.subarray(HEADER_SIZE).toString()).toBe("hello");
  });

  it("encodes a control frame", () => {
    const payload = Buffer.from(JSON.stringify({ exit_code: 0 }));
    const frame = controlFrame(payload);
    const encoded = encodeFrame(frame);

    expect(encoded.readUInt8(0)).toBe(FrameType.Control);
    expect(encoded.readUInt32BE(1)).toBe(payload.length);
  });

  it("encodes a heartbeat frame with empty payload", () => {
    const frame = heartbeatFrame();
    const encoded = encodeFrame(frame);

    expect(encoded.length).toBe(HEADER_SIZE);
    expect(encoded.readUInt8(0)).toBe(FrameType.Heartbeat);
    expect(encoded.readUInt32BE(1)).toBe(0);
  });

  it("encodes an error frame", () => {
    const frame = errorFrame("something broke");
    const encoded = encodeFrame(frame);

    expect(encoded.readUInt8(0)).toBe(FrameType.Error);
    expect(encoded.subarray(HEADER_SIZE).toString()).toBe("something broke");
  });

  it("frame types have correct values", () => {
    expect(FrameType.Data).toBe(0x01);
    expect(FrameType.Control).toBe(0x02);
    expect(FrameType.Heartbeat).toBe(0x03);
    expect(FrameType.Error).toBe(0x04);
    expect(FrameType.Close).toBe(0x05);
  });

  it("header size is 5 bytes", () => {
    expect(HEADER_SIZE).toBe(5);
  });

  it("encodes large payload correctly", () => {
    const payload = Buffer.alloc(65536, 0x42);
    const frame = dataFrame(payload);
    const encoded = encodeFrame(frame);

    expect(encoded.readUInt32BE(1)).toBe(65536);
    expect(encoded.length).toBe(HEADER_SIZE + 65536);
  });
});

describe("options", () => {
  it("has correct default options", () => {
    expect(DEFAULT_OPTIONS.image).toBe("alpine:latest");
    expect(DEFAULT_OPTIONS.cpus).toBe(1);
    expect(DEFAULT_OPTIONS.memoryMb).toBe(256);
    expect(DEFAULT_OPTIONS.network).toBe(true);
    expect(DEFAULT_OPTIONS.tee).toBe(false);
  });

  it("SandboxOptions can be partially specified", () => {
    const opts: SandboxOptions = { image: "ubuntu:22.04", cpus: 4 };
    expect(opts.image).toBe("ubuntu:22.04");
    expect(opts.cpus).toBe(4);
    expect(opts.memoryMb).toBeUndefined();
  });

  it("MountSpec supports readonly flag", () => {
    const mount: MountSpec = {
      hostPath: "/host/data",
      guestPath: "/data",
      readonly: true,
    };
    expect(mount.readonly).toBe(true);
  });

  it("PortForward defaults hostPort to undefined", () => {
    const pf: PortForward = { guestPort: 8080 };
    expect(pf.guestPort).toBe(8080);
    expect(pf.hostPort).toBeUndefined();
  });

  it("WorkspaceConfig has optional guestPath", () => {
    const ws: WorkspaceConfig = { name: "my-project" };
    expect(ws.name).toBe("my-project");
    expect(ws.guestPath).toBeUndefined();
  });

  it("full SandboxOptions with all fields", () => {
    const opts: SandboxOptions = {
      image: "python:3.12-slim",
      cpus: 4,
      memoryMb: 2048,
      env: { API_KEY: "secret", DEBUG: "1" },
      workdir: "/app",
      mounts: [{ hostPath: "/local", guestPath: "/mnt", readonly: false }],
      network: true,
      tee: true,
      name: "test-sandbox",
      portForwards: [{ guestPort: 8080, hostPort: 3000 }],
      workspace: { name: "ws", guestPath: "/workspace" },
    };

    expect(opts.env?.API_KEY).toBe("secret");
    expect(opts.mounts?.length).toBe(1);
    expect(opts.portForwards?.[0].hostPort).toBe(3000);
    expect(opts.workspace?.guestPath).toBe("/workspace");
  });
});

describe("stream types", () => {
  it("StreamType enum values", () => {
    expect(StreamType.Stdout).toBe("Stdout");
    expect(StreamType.Stderr).toBe("Stderr");
  });
});

describe("ExecResult shape", () => {
  it("has expected structure", () => {
    const result: ExecResult = {
      stdout: "hello\n",
      stderr: "",
      exitCode: 0,
      metrics: {
        durationMs: 42,
        stdoutBytes: 6,
        stderrBytes: 0,
      },
    };

    expect(result.stdout).toBe("hello\n");
    expect(result.exitCode).toBe(0);
    expect(result.metrics.durationMs).toBe(42);
    expect(result.metrics.stdoutBytes).toBe(6);
    expect(result.metrics.stderrBytes).toBe(0);
  });

  it("metrics track byte counts", () => {
    const metrics: ExecMetrics = {
      durationMs: 100,
      stdoutBytes: 1024,
      stderrBytes: 256,
    };

    expect(metrics.stdoutBytes + metrics.stderrBytes).toBe(1280);
  });
});

describe("BoxSdk", () => {
  it("can be imported", async () => {
    const { BoxSdk } = await import("../src/sdk.js");
    expect(BoxSdk).toBeDefined();
  });

  it("uses custom homeDir", async () => {
    // We can't fully test create() without a running a3s-box,
    // but we can verify the constructor accepts a homeDir.
    const { BoxSdk } = await import("../src/sdk.js");
    const tmpDir = `/tmp/a3s-test-${Date.now()}`;
    const sdk = new BoxSdk(tmpDir);
    expect(sdk.homeDir).toBe(tmpDir);
    expect(sdk.toString()).toBe(`BoxSdk(home=${tmpDir})`);

    // Clean up
    const fs = await import("node:fs");
    fs.rmSync(tmpDir, { recursive: true, force: true });
  });

  it("creates subdirectories on init", async () => {
    const { BoxSdk } = await import("../src/sdk.js");
    const fs = await import("node:fs");
    const tmpDir = `/tmp/a3s-test-${Date.now()}`;

    new BoxSdk(tmpDir);

    for (const dir of ["images", "rootfs-cache", "sandboxes", "workspaces"]) {
      expect(fs.existsSync(`${tmpDir}/${dir}`)).toBe(true);
    }

    fs.rmSync(tmpDir, { recursive: true, force: true });
  });
});

describe("Sandbox", () => {
  it("can be imported and constructed", async () => {
    const { Sandbox } = await import("../src/sandbox.js");
    const sandbox = new Sandbox(
      "test-id",
      "test-name",
      "/tmp/exec.sock",
      "/tmp/pty.sock",
    );

    expect(sandbox.id).toBe("test-id");
    expect(sandbox.name).toBe("test-name");
    expect(sandbox.toString()).toBe("Sandbox(id=test-id, name=test-name)");
  });
});

describe("index exports", () => {
  it("exports all public types", async () => {
    const mod = await import("../src/index.js");
    expect(mod.BoxSdk).toBeDefined();
    expect(mod.Sandbox).toBeDefined();
    expect(mod.StreamingExec).toBeDefined();
    expect(mod.StreamType).toBeDefined();
    expect(mod.FrameType).toBeDefined();
    expect(mod.FrameReader).toBeDefined();
    expect(mod.DEFAULT_OPTIONS).toBeDefined();
    expect(mod.encodeFrame).toBeDefined();
    expect(mod.dataFrame).toBeDefined();
    expect(mod.controlFrame).toBeDefined();
    expect(mod.heartbeatFrame).toBeDefined();
    expect(mod.errorFrame).toBeDefined();
    expect(mod.connectUnix).toBeDefined();
    expect(mod.writeFrame).toBeDefined();
  });
});
