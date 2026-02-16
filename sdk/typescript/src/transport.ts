/**
 * Frame wire protocol — compatible with a3s-transport.
 *
 * Wire format: [type:u8][length:u32 big-endian][payload:length bytes]
 */

import * as net from "node:net";

export const HEADER_SIZE = 5;

export enum FrameType {
  Data = 0x01,
  Control = 0x02,
  Heartbeat = 0x03,
  Error = 0x04,
  Close = 0x05,
}

export interface Frame {
  frameType: FrameType;
  payload: Buffer;
}

export function encodeFrame(frame: Frame): Buffer {
  const header = Buffer.alloc(HEADER_SIZE);
  header.writeUInt8(frame.frameType, 0);
  header.writeUInt32BE(frame.payload.length, 1);
  return Buffer.concat([header, frame.payload]);
}

export function dataFrame(payload: Buffer): Frame {
  return { frameType: FrameType.Data, payload };
}

export function controlFrame(payload: Buffer): Frame {
  return { frameType: FrameType.Control, payload };
}

export function heartbeatFrame(): Frame {
  return { frameType: FrameType.Heartbeat, payload: Buffer.alloc(0) };
}

export function errorFrame(message: string): Frame {
  return { frameType: FrameType.Error, payload: Buffer.from(message, "utf-8") };
}

/**
 * Async frame reader from a socket.
 */
export class FrameReader {
  private buffer = Buffer.alloc(0);
  private socket: net.Socket;
  private resolveWait: ((value: void) => void) | null = null;
  private ended = false;

  constructor(socket: net.Socket) {
    this.socket = socket;
    socket.on("data", (chunk: Buffer) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      if (this.resolveWait) {
        const resolve = this.resolveWait;
        this.resolveWait = null;
        resolve();
      }
    });
    socket.on("end", () => {
      this.ended = true;
      if (this.resolveWait) {
        const resolve = this.resolveWait;
        this.resolveWait = null;
        resolve();
      }
    });
  }

  private waitForData(): Promise<void> {
    return new Promise((resolve) => {
      this.resolveWait = resolve;
    });
  }

  async readFrame(): Promise<Frame | null> {
    // Wait for header
    while (this.buffer.length < HEADER_SIZE) {
      if (this.ended) return null;
      await this.waitForData();
      if (this.ended && this.buffer.length < HEADER_SIZE) return null;
    }

    const frameType = this.buffer.readUInt8(0) as FrameType;
    const length = this.buffer.readUInt32BE(1);
    const totalSize = HEADER_SIZE + length;

    // Wait for full payload
    while (this.buffer.length < totalSize) {
      if (this.ended) return null;
      await this.waitForData();
      if (this.ended && this.buffer.length < totalSize) return null;
    }

    const payload = this.buffer.subarray(HEADER_SIZE, totalSize);
    this.buffer = this.buffer.subarray(totalSize);

    return { frameType, payload: Buffer.from(payload) };
  }
}

/**
 * Connect to a Unix socket and return a FrameReader + raw socket for writing.
 */
export function connectUnix(socketPath: string): Promise<{ reader: FrameReader; socket: net.Socket }> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath, () => {
      resolve({ reader: new FrameReader(socket), socket });
    });
    socket.on("error", reject);
  });
}

/**
 * Write a frame to a socket.
 */
export function writeFrame(socket: net.Socket, frame: Frame): Promise<void> {
  return new Promise((resolve, reject) => {
    socket.write(encodeFrame(frame), (err) => {
      if (err) reject(err);
      else resolve();
    });
  });
}
