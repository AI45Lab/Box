/**
 * Sandbox configuration options.
 */

export interface MountSpec {
  hostPath: string;
  guestPath: string;
  readonly?: boolean;
}

export interface PortForward {
  guestPort: number;
  hostPort?: number;
  protocol?: string;
}

export interface WorkspaceConfig {
  name: string;
  guestPath?: string;
}

export interface SandboxOptions {
  image?: string;
  cpus?: number;
  memoryMb?: number;
  env?: Record<string, string>;
  workdir?: string;
  mounts?: MountSpec[];
  network?: boolean;
  tee?: boolean;
  name?: string;
  portForwards?: PortForward[];
  workspace?: WorkspaceConfig;
}

export const DEFAULT_OPTIONS: Required<
  Pick<SandboxOptions, "image" | "cpus" | "memoryMb" | "network" | "tee">
> = {
  image: "alpine:latest",
  cpus: 1,
  memoryMb: 256,
  network: true,
  tee: false,
};
