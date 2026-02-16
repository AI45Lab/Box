"""A3S Box Python SDK — embedded MicroVM sandboxes.

Create, execute commands in, and manage MicroVM sandboxes from Python.
Communicates with the a3s-box runtime over Unix socket using the Frame protocol.

Quick Start::

    import asyncio
    from a3s_box import BoxSdk, SandboxOptions

    async def main():
        sdk = BoxSdk()
        sandbox = await sdk.create(SandboxOptions(image="alpine:latest"))

        result = await sandbox.exec("echo", "hello")
        print(result.stdout)

        await sandbox.stop()

    asyncio.run(main())
"""

from a3s_box.sdk import BoxSdk
from a3s_box.sandbox import Sandbox, ExecResult, ExecMetrics
from a3s_box.options import SandboxOptions, MountSpec, PortForward, WorkspaceConfig
from a3s_box.stream import StreamingExec, ExecEvent, ExecChunk, ExecExit, StreamType
from a3s_box.transport import FrameReader, FrameWriter, Frame, FrameType

__version__ = "0.2.0"

__all__ = [
    "BoxSdk",
    "Sandbox",
    "SandboxOptions",
    "MountSpec",
    "PortForward",
    "WorkspaceConfig",
    "ExecResult",
    "ExecMetrics",
    "StreamingExec",
    "ExecEvent",
    "ExecChunk",
    "ExecExit",
    "StreamType",
    "FrameReader",
    "FrameWriter",
    "Frame",
    "FrameType",
]
