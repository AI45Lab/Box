/**
 * BoxSdk — main entry point for creating sandboxes.
 */

import { spawn, type ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import type { SandboxOptions } from "./options.js";
import { DEFAULT_OPTIONS } from "./options.js";
import { Sandbox } from "./sandbox.js";

/**
 * SDK entry point for creating and managing MicroVM sandboxes.
 *
 * ```ts
 * const sdk = new BoxSdk();
 * const sandbox = await sdk.create({ image: "alpine:latest" });
 * const result = await sandbox.exec("echo", "hello");
 * console.log(result.stdout);
 * await sandbox.stop();
 * ```
 */
export class BoxSdk {
  readonly homeDir: string;

  constructor(homeDir?: string) {
    this.homeDir = homeDir ?? path.join(os.homedir(), ".a3s");

    for (const dir of ["images", "rootfs-cache", "sandboxes", "workspaces"]) {
      fs.mkdirSync(path.join(this.homeDir, dir), { recursive: true });
    }
  }

  /**
   * Create a new sandbox.
   */
  async create(options?: SandboxOptions): Promise<Sandbox> {
    const opts = { ...DEFAULT_OPTIONS, ...options };
    const sandboxId = crypto.randomUUID();
    const name = opts.name ?? `sandbox-${sandboxId.slice(0, 8)}`;

    const cmd = this.buildRunCommand(opts, sandboxId, name);

    const proc = spawn(cmd[0], cmd.slice(1), {
      stdio: ["ignore", "pipe", "pipe"],
    });

    const sandboxDir = path.join(this.homeDir, "sandboxes", sandboxId);
    const execSocket = path.join(sandboxDir, "exec.sock");
    const ptySocket = path.join(sandboxDir, "pty.sock");

    await this.waitForSocket(execSocket, 30_000);

    return new Sandbox(sandboxId, name, execSocket, ptySocket, proc);
  }

  private buildRunCommand(
    opts: SandboxOptions & typeof DEFAULT_OPTIONS,
    sandboxId: string,
    name: string,
  ): string[] {
    const image = opts.image ?? DEFAULT_OPTIONS.image;
    const cmd = ["a3s-box", "run", image, "--name", name];

    cmd.push("--cpus", String(opts.cpus ?? DEFAULT_OPTIONS.cpus));
    cmd.push("--memory", String(opts.memoryMb ?? DEFAULT_OPTIONS.memoryMb));

    if (opts.env) {
      for (const [key, value] of Object.entries(opts.env)) {
        cmd.push("-e", `${key}=${value}`);
      }
    }

    if (opts.workdir) {
      cmd.push("--workdir", opts.workdir);
    }

    if (opts.mounts) {
      for (const m of opts.mounts) {
        let spec = `${m.hostPath}:${m.guestPath}`;
        if (m.readonly) spec += ":ro";
        cmd.push("-v", spec);
      }
    }

    if (opts.portForwards) {
      for (const pf of opts.portForwards) {
        cmd.push("-p", `${pf.hostPort ?? 0}:${pf.guestPort}`);
      }
    }

    if (opts.workspace) {
      const wsDir = path.join(this.homeDir, "workspaces", opts.workspace.name);
      fs.mkdirSync(wsDir, { recursive: true });
      const guestPath = opts.workspace.guestPath ?? "/workspace";
      cmd.push("-v", `${wsDir}:${guestPath}`);
    }

    if (opts.tee) {
      cmd.push("--tee", "snp");
    }

    return cmd;
  }

  private waitForSocket(socketPath: string, timeoutMs: number): Promise<void> {
    return new Promise((resolve, reject) => {
      const deadline = Date.now() + timeoutMs;
      const check = () => {
        if (fs.existsSync(socketPath)) {
          resolve();
          return;
        }
        if (Date.now() >= deadline) {
          reject(new Error(`Socket ${socketPath} did not appear within ${timeoutMs}ms`));
          return;
        }
        setTimeout(check, 100);
      };
      check();
    });
  }

  toString(): string {
    return `BoxSdk(home=${this.homeDir})`;
  }
}
