/**
 * Sandbox — a running MicroVM instance.
 */

import {
  connectUnix,
  dataFrame,
  writeFrame,
  FrameType,
  type FrameReader,
} from "./transport.js";
import { StreamingExec } from "./stream.js";

export interface ExecMetrics {
  durationMs: number;
  stdoutBytes: number;
  stderrBytes: number;
}

export interface ExecResult {
  stdout: string;
  stderr: string;
  exitCode: number;
  metrics: ExecMetrics;
}

export interface ExecOptions {
  env?: string[];
  workingDir?: string;
  stdin?: Buffer;
  user?: string;
}

/**
 * A running MicroVM sandbox.
 */
export class Sandbox {
  readonly id: string;
  readonly name: string;
  private execSocket: string;
  private ptySocket: string;
  private process?: { kill(): void };

  constructor(
    id: string,
    name: string,
    execSocket: string,
    ptySocket: string,
    process?: { kill(): void },
  ) {
    this.id = id;
    this.name = name;
    this.execSocket = execSocket;
    this.ptySocket = ptySocket;
    this.process = process;
  }

  /**
   * Execute a command and wait for completion.
   */
  async exec(cmd: string, ...args: string[]): Promise<ExecResult>;
  async exec(cmd: string[], options?: ExecOptions): Promise<ExecResult>;
  async exec(
    cmdOrParts: string | string[],
    ...rest: unknown[]
  ): Promise<ExecResult> {
    const started = Date.now();
    let cmdParts: string[];
    let options: ExecOptions = {};

    if (typeof cmdOrParts === "string") {
      cmdParts = [cmdOrParts, ...(rest as string[])];
    } else {
      cmdParts = cmdOrParts;
      if (rest.length > 0 && typeof rest[0] === "object") {
        options = rest[0] as ExecOptions;
      }
    }

    const request = {
      cmd: cmdParts,
      timeout_ns: 0,
      env: options.env ?? [],
      working_dir: options.workingDir ?? null,
      stdin: options.stdin ? Array.from(options.stdin) : null,
      user: options.user ?? null,
      streaming: false,
    };

    const { reader, socket } = await connectUnix(this.execSocket);
    await writeFrame(socket, dataFrame(Buffer.from(JSON.stringify(request))));

    const frame = await reader.readFrame();
    socket.destroy();

    if (!frame) throw new Error("Exec server closed without response");
    if (frame.frameType === FrameType.Error) {
      throw new Error(`Exec error: ${frame.payload.toString()}`);
    }

    const output = JSON.parse(frame.payload.toString("utf-8"));
    const stdoutBuf = Buffer.from(output.stdout);
    const stderrBuf = Buffer.from(output.stderr);
    const elapsed = Date.now() - started;

    return {
      stdout: stdoutBuf.toString("utf-8"),
      stderr: stderrBuf.toString("utf-8"),
      exitCode: output.exit_code,
      metrics: {
        durationMs: elapsed,
        stdoutBytes: stdoutBuf.length,
        stderrBytes: stderrBuf.length,
      },
    };
  }

  /**
   * Execute a command in streaming mode.
   *
   * ```ts
   * for await (const event of sandbox.execStream("make", "build")) {
   *   if (event.type === "chunk") {
   *     process.stdout.write(event.chunk.data);
   *   }
   * }
   * ```
   */
  async execStream(cmd: string, ...args: string[]): Promise<StreamingExec> {
    const request = {
      cmd: [cmd, ...args],
      timeout_ns: 0,
      env: [],
      streaming: true,
    };

    const { reader, socket } = await connectUnix(this.execSocket);
    await writeFrame(socket, dataFrame(Buffer.from(JSON.stringify(request))));

    return new StreamingExec(reader);
  }

  /**
   * Upload a file into the sandbox.
   */
  async upload(data: Buffer, guestPath: string): Promise<void> {
    const request = {
      op: "Upload",
      guest_path: guestPath,
      data: data.toString("base64"),
    };

    const { reader, socket } = await connectUnix(this.execSocket);
    await writeFrame(socket, dataFrame(Buffer.from(JSON.stringify(request))));

    const frame = await reader.readFrame();
    socket.destroy();

    if (!frame) throw new Error("Exec server closed without response");
    const resp = JSON.parse(frame.payload.toString());
    if (!resp.success) {
      throw new Error(`Upload failed: ${resp.error ?? "unknown"}`);
    }
  }

  /**
   * Download a file from the sandbox.
   */
  async download(guestPath: string): Promise<Buffer> {
    const request = {
      op: "Download",
      guest_path: guestPath,
    };

    const { reader, socket } = await connectUnix(this.execSocket);
    await writeFrame(socket, dataFrame(Buffer.from(JSON.stringify(request))));

    const frame = await reader.readFrame();
    socket.destroy();

    if (!frame) throw new Error("Exec server closed without response");
    const resp = JSON.parse(frame.payload.toString());
    if (!resp.success) {
      throw new Error(`Download failed: ${resp.error ?? "unknown"}`);
    }

    return Buffer.from(resp.data, "base64");
  }

  /**
   * Stop the sandbox and release resources.
   */
  async stop(): Promise<void> {
    this.process?.kill();
  }

  toString(): string {
    return `Sandbox(id=${this.id}, name=${this.name})`;
  }
}
