/**
 * Streaming exec types and handler.
 */

import { FrameReader, FrameType, type Frame } from "./transport.js";

export enum StreamType {
  Stdout = "Stdout",
  Stderr = "Stderr",
}

export interface ExecChunk {
  stream: StreamType;
  data: Buffer;
}

export interface ExecExit {
  exitCode: number;
}

export type ExecEvent =
  | { type: "chunk"; chunk: ExecChunk }
  | { type: "exit"; exit: ExecExit };

/**
 * Handle for reading streaming exec events.
 */
export class StreamingExec {
  private reader: FrameReader;
  private started: number;
  private _stdoutBytes = 0;
  private _stderrBytes = 0;
  private _done = false;

  constructor(reader: FrameReader) {
    this.reader = reader;
    this.started = Date.now();
  }

  get isDone(): boolean {
    return this._done;
  }

  get stdoutBytes(): number {
    return this._stdoutBytes;
  }

  get stderrBytes(): number {
    return this._stderrBytes;
  }

  get elapsedMs(): number {
    return Date.now() - this.started;
  }

  async nextEvent(): Promise<ExecEvent | null> {
    if (this._done) return null;

    const frame = await this.reader.readFrame();
    if (!frame) {
      this._done = true;
      return null;
    }

    if (frame.frameType === FrameType.Data) {
      const obj = JSON.parse(frame.payload.toString("utf-8"));
      const stream = obj.stream as StreamType;
      const data = Buffer.from(obj.data);
      if (stream === StreamType.Stdout) {
        this._stdoutBytes += data.length;
      } else {
        this._stderrBytes += data.length;
      }
      return { type: "chunk", chunk: { stream, data } };
    }

    if (frame.frameType === FrameType.Control) {
      const obj = JSON.parse(frame.payload.toString("utf-8"));
      this._done = true;
      return { type: "exit", exit: { exitCode: obj.exit_code } };
    }

    if (frame.frameType === FrameType.Error) {
      this._done = true;
      throw new Error(`Streaming exec error: ${frame.payload.toString()}`);
    }

    throw new Error(`Unexpected frame type: ${frame.frameType}`);
  }

  async collect(): Promise<{ stdout: Buffer; stderr: Buffer; exitCode: number }> {
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let exitCode = -1;

    while (true) {
      const event = await this.nextEvent();
      if (!event) break;
      if (event.type === "chunk") {
        if (event.chunk.stream === StreamType.Stdout) {
          stdout.push(event.chunk.data);
        } else {
          stderr.push(event.chunk.data);
        }
      } else {
        exitCode = event.exit.exitCode;
      }
    }

    return {
      stdout: Buffer.concat(stdout),
      stderr: Buffer.concat(stderr),
      exitCode,
    };
  }

  /**
   * Async iterator support.
   */
  async *[Symbol.asyncIterator](): AsyncIterableIterator<ExecEvent> {
    while (true) {
      const event = await this.nextEvent();
      if (!event) return;
      yield event;
    }
  }
}
