"""Streaming exec types and handler."""

from __future__ import annotations

import enum
import json
import time
from dataclasses import dataclass

from a3s_box.transport import Frame, FrameReader, FrameType


class StreamType(enum.Enum):
    """Which output stream a chunk belongs to."""

    STDOUT = "Stdout"
    STDERR = "Stderr"


@dataclass
class ExecChunk:
    """A chunk of streaming output."""

    stream: StreamType
    data: bytes


@dataclass
class ExecExit:
    """Final exit notification."""

    exit_code: int


@dataclass
class ExecEvent:
    """A streaming exec event — either a chunk or exit."""

    chunk: ExecChunk | None = None
    exit: ExecExit | None = None

    @property
    def is_chunk(self) -> bool:
        return self.chunk is not None

    @property
    def is_exit(self) -> bool:
        return self.exit is not None


class StreamingExec:
    """Handle for reading streaming exec events."""

    def __init__(self, reader: FrameReader) -> None:
        self._reader = reader
        self._started = time.monotonic()
        self._stdout_bytes = 0
        self._stderr_bytes = 0
        self._done = False

    @property
    def is_done(self) -> bool:
        return self._done

    @property
    def stdout_bytes(self) -> int:
        return self._stdout_bytes

    @property
    def stderr_bytes(self) -> int:
        return self._stderr_bytes

    @property
    def elapsed_ms(self) -> int:
        return int((time.monotonic() - self._started) * 1000)

    async def next_event(self) -> ExecEvent | None:
        """Read the next event. Returns None when done."""
        if self._done:
            return None

        frame = await self._reader.read_frame()
        if frame is None:
            self._done = True
            return None

        if frame.frame_type == FrameType.DATA:
            obj = json.loads(frame.payload)
            stream = StreamType(obj["stream"])
            data = bytes(obj["data"])
            if stream == StreamType.STDOUT:
                self._stdout_bytes += len(data)
            else:
                self._stderr_bytes += len(data)
            return ExecEvent(chunk=ExecChunk(stream=stream, data=data))

        if frame.frame_type == FrameType.CONTROL:
            obj = json.loads(frame.payload)
            self._done = True
            return ExecEvent(exit=ExecExit(exit_code=obj["exit_code"]))

        if frame.frame_type == FrameType.ERROR:
            self._done = True
            msg = frame.payload.decode("utf-8", errors="replace")
            raise RuntimeError(f"Streaming exec error: {msg}")

        raise RuntimeError(f"Unexpected frame type: {frame.frame_type}")

    async def collect(self) -> tuple[bytes, bytes, int]:
        """Collect all output and return (stdout, stderr, exit_code)."""
        stdout = bytearray()
        stderr = bytearray()
        exit_code = -1

        while True:
            event = await self.next_event()
            if event is None:
                break
            if event.is_chunk:
                chunk = event.chunk
                assert chunk is not None
                if chunk.stream == StreamType.STDOUT:
                    stdout.extend(chunk.data)
                else:
                    stderr.extend(chunk.data)
            elif event.is_exit:
                assert event.exit is not None
                exit_code = event.exit.exit_code

        return bytes(stdout), bytes(stderr), exit_code

    def __aiter__(self):
        return self

    async def __anext__(self) -> ExecEvent:
        event = await self.next_event()
        if event is None:
            raise StopAsyncIteration
        return event
