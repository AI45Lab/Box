"""BoxSdk — main entry point for creating sandboxes."""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import uuid
from pathlib import Path

from a3s_box.options import SandboxOptions
from a3s_box.sandbox import Sandbox


class BoxSdk:
    """SDK entry point for creating and managing MicroVM sandboxes.

    Example::

        sdk = BoxSdk()
        sandbox = await sdk.create(SandboxOptions(image="alpine:latest"))
        result = await sandbox.exec("echo", "hello")
        print(result.stdout)
        await sandbox.stop()
    """

    def __init__(self, home_dir: str | Path | None = None) -> None:
        if home_dir is None:
            home_dir = Path.home() / ".a3s"
        self._home = Path(home_dir)

        for d in ("images", "rootfs-cache", "sandboxes", "workspaces"):
            (self._home / d).mkdir(parents=True, exist_ok=True)

    @property
    def home_dir(self) -> Path:
        return self._home

    async def create(self, options: SandboxOptions | None = None) -> Sandbox:
        """Create a new sandbox.

        Launches an a3s-box instance with the given options and returns
        a Sandbox handle for command execution.

        Args:
            options: Sandbox configuration. Defaults to alpine:latest.

        Returns:
            A running Sandbox instance.
        """
        if options is None:
            options = SandboxOptions()

        sandbox_id = str(uuid.uuid4())
        name = options.name or f"sandbox-{sandbox_id[:8]}"

        # Build a3s-box run command
        cmd = self._build_run_command(options, sandbox_id, name)

        # Start the sandbox process
        process = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        # Determine socket paths
        sandbox_dir = self._home / "sandboxes" / sandbox_id
        exec_socket = str(sandbox_dir / "exec.sock")
        pty_socket = str(sandbox_dir / "pty.sock")

        # Wait for exec socket to appear
        await self._wait_for_socket(exec_socket, timeout=30.0)

        return Sandbox(
            sandbox_id=sandbox_id,
            name=name,
            exec_socket=exec_socket,
            pty_socket=pty_socket,
            _process=process,
        )

    def _build_run_command(
        self, options: SandboxOptions, sandbox_id: str, name: str
    ) -> list[str]:
        """Build the a3s-box run CLI command."""
        cmd = ["a3s-box", "run", options.image, "--name", name]

        cmd.extend(["--cpus", str(options.cpus)])
        cmd.extend(["--memory", str(options.memory_mb)])

        for key, value in options.env.items():
            cmd.extend(["-e", f"{key}={value}"])

        if options.workdir:
            cmd.extend(["--workdir", options.workdir])

        for mount in options.mounts:
            spec = f"{mount.host_path}:{mount.guest_path}"
            if mount.readonly:
                spec += ":ro"
            cmd.extend(["-v", spec])

        for pf in options.port_forwards:
            cmd.extend(["-p", f"{pf.host_port}:{pf.guest_port}"])

        if options.workspace:
            ws_dir = self._home / "workspaces" / options.workspace.name
            ws_dir.mkdir(parents=True, exist_ok=True)
            cmd.extend(["-v", f"{ws_dir}:{options.workspace.guest_path}"])

        if options.tee:
            cmd.extend(["--tee", "snp"])

        return cmd

    async def _wait_for_socket(self, path: str, timeout: float = 30.0) -> None:
        """Wait for a Unix socket to appear on disk."""
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            if os.path.exists(path):
                return
            await asyncio.sleep(0.1)
        raise TimeoutError(f"Socket {path} did not appear within {timeout}s")

    def __repr__(self) -> str:
        return f"BoxSdk(home={self._home})"
