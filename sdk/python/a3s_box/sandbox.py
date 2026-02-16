"""Sandbox — a running MicroVM instance."""

from __future__ import annotations

import base64
import json
import time
from dataclasses import dataclass, field

from a3s_box.stream import StreamingExec
from a3s_box.transport import Frame, FrameReader, FrameWriter, FrameType


@dataclass
class ExecMetrics:
    """Metrics collected during command execution."""

    duration_ms: int = 0
    stdout_bytes: int = 0
    stderr_bytes: int = 0


@dataclass
class ExecResult:
    """Result of executing a command in a sandbox."""

    stdout: str = ""
    stderr: str = ""
    exit_code: int = 0
    metrics: ExecMetrics = field(default_factory=ExecMetrics)


class Sandbox:
    """A running MicroVM sandbox.

    Provides methods to execute commands, stream output, transfer files,
    and manage the sandbox lifecycle.
    """

    def __init__(
        self,
        sandbox_id: str,
        name: str,
        exec_socket: str,
        pty_socket: str,
        *,
        _process: object | None = None,
    ) -> None:
        self._id = sandbox_id
        self._name = name
        self._exec_socket = exec_socket
        self._pty_socket = pty_socket
        self._process = _process

    @property
    def id(self) -> str:
        return self._id

    @property
    def name(self) -> str:
        return self._name

    async def _connect_exec(self) -> tuple[FrameReader, FrameWriter]:
        """Connect to the exec server Unix socket."""
        import asyncio

        reader, writer = await asyncio.open_unix_connection(self._exec_socket)
        return FrameReader(reader), FrameWriter(writer)

    async def exec(self, cmd: str, *args: str, **kwargs) -> ExecResult:
        """Execute a command and wait for completion.

        Args:
            cmd: Command to execute.
            *args: Command arguments.
            **kwargs: Optional env (list[str]), working_dir (str),
                      stdin (bytes), user (str).

        Returns:
            ExecResult with stdout, stderr, exit_code, and metrics.
        """
        started = time.monotonic()
        cmd_parts = [cmd, *args]

        request = {
            "cmd": cmd_parts,
            "timeout_ns": 0,
            "env": kwargs.get("env", []),
            "working_dir": kwargs.get("working_dir"),
            "stdin": list(kwargs["stdin"]) if kwargs.get("stdin") else None,
            "user": kwargs.get("user"),
            "streaming": False,
        }

        reader, writer = await self._connect_exec()
        await writer.write_data(json.dumps(request).encode())

        frame = await reader.read_frame()
        if frame is None:
            raise RuntimeError("Exec server closed without response")

        if frame.frame_type == FrameType.ERROR:
            raise RuntimeError(f"Exec error: {frame.payload.decode()}")

        output = json.loads(frame.payload)
        elapsed = int((time.monotonic() - started) * 1000)

        stdout_bytes = bytes(output["stdout"])
        stderr_bytes = bytes(output["stderr"])

        return ExecResult(
            stdout=stdout_bytes.decode("utf-8", errors="replace"),
            stderr=stderr_bytes.decode("utf-8", errors="replace"),
            exit_code=output["exit_code"],
            metrics=ExecMetrics(
                duration_ms=elapsed,
                stdout_bytes=len(stdout_bytes),
                stderr_bytes=len(stderr_bytes),
            ),
        )

    async def exec_stream(self, cmd: str, *args: str) -> StreamingExec:
        """Execute a command in streaming mode.

        Returns a StreamingExec handle that yields output chunks
        as they arrive. Supports async iteration::

            async for event in sandbox.exec_stream("make", "build"):
                if event.is_chunk:
                    print(event.chunk.data.decode(), end="")
                elif event.is_exit:
                    print(f"Exit: {event.exit.exit_code}")
        """
        request = {
            "cmd": [cmd, *args],
            "timeout_ns": 0,
            "env": [],
            "streaming": True,
        }

        reader, writer = await self._connect_exec()
        await writer.write_data(json.dumps(request).encode())

        return StreamingExec(reader)

    async def upload(self, data: bytes, guest_path: str) -> None:
        """Upload a file into the sandbox.

        Args:
            data: File contents.
            guest_path: Destination path inside the sandbox.
        """
        request = {
            "op": "Upload",
            "guest_path": guest_path,
            "data": base64.b64encode(data).decode("ascii"),
        }

        reader, writer = await self._connect_exec()
        await writer.write_data(json.dumps(request).encode())

        frame = await reader.read_frame()
        if frame is None:
            raise RuntimeError("Exec server closed without response")

        resp = json.loads(frame.payload)
        if not resp.get("success"):
            raise RuntimeError(f"Upload failed: {resp.get('error', 'unknown')}")

    async def download(self, guest_path: str) -> bytes:
        """Download a file from the sandbox.

        Args:
            guest_path: Path inside the sandbox.

        Returns:
            Raw file contents.
        """
        request = {
            "op": "Download",
            "guest_path": guest_path,
        }

        reader, writer = await self._connect_exec()
        await writer.write_data(json.dumps(request).encode())

        frame = await reader.read_frame()
        if frame is None:
            raise RuntimeError("Exec server closed without response")

        resp = json.loads(frame.payload)
        if not resp.get("success"):
            raise RuntimeError(f"Download failed: {resp.get('error', 'unknown')}")

        return base64.b64decode(resp["data"])

    async def stop(self) -> None:
        """Stop the sandbox and release resources."""
        # Send a close signal if process is managed
        if self._process is not None and hasattr(self._process, "terminate"):
            self._process.terminate()

    async def __aenter__(self) -> "Sandbox":
        return self

    async def __aexit__(self, *exc) -> None:
        await self.stop()

    def __repr__(self) -> str:
        return f"Sandbox(id={self._id!r}, name={self._name!r})"
