/**
 * @a3s-lab/box — TypeScript SDK for A3S Box MicroVM sandboxes.
 */

export { BoxSdk } from "./sdk.js";
export { Sandbox } from "./sandbox.js";
export type { ExecResult, ExecMetrics, ExecOptions } from "./sandbox.js";
export type {
  SandboxOptions,
  MountSpec,
  PortForward,
  WorkspaceConfig,
} from "./options.js";
export { DEFAULT_OPTIONS } from "./options.js";
export { StreamingExec } from "./stream.js";
export { StreamType } from "./stream.js";
export type { ExecChunk, ExecExit, ExecEvent } from "./stream.js";
export {
  FrameType,
  FrameReader,
  encodeFrame,
  dataFrame,
  controlFrame,
  heartbeatFrame,
  errorFrame,
  connectUnix,
  writeFrame,
} from "./transport.js";
export type { Frame } from "./transport.js";
