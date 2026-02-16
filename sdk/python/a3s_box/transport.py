"""Frame wire protocol — compatible with a3s-transport.

Wire format: [type:u8][length:u32 big-endian][payload:length bytes]
"""

from __future__ import annotations

import asyncio
import enum
import struct
from dataclasses import dataclass

HEADER_SIZE = 5
HEADER_FMT = "!BI"  # u8 type + u32 length (big-endian)


class FrameType(enum.IntEnum):
    """Frame type identifiers matching a3s-transport."""

    DATA = 0x01
    CONTROL = 0x02
    HEARTBEAT = 0x03
    ERROR = 0x04
    CLOSE = 0x05


@dataclass
class Frame:
    """A single wire frame."""

    frame_type: FrameType
    payload: bytes

    def encode(self) -> bytes:
        """Encode to wire format."""
        header = struct.pack(HEADER_FMT, self.frame_type, len(self.payload))
        return header + self.payload

    @staticmethod
    def data(payload: bytes) -> "Frame":
        return Frame(FrameType.DATA, payload)

    @staticmethod
    def control(payload: bytes) -> "Frame":
        return Frame(FrameType.CONTROL, payload)

    @staticmethod
    def heartbeat() -> "Frame":
        return Frame(FrameType.HEARTBEAT, b"")

    @staticmethod
    def error(message: str) -> "Frame":
        return Frame(FrameType.ERROR, message.encode("utf-8"))


class FrameReader:
    """Async frame reader from a StreamReader."""

    def __init__(self, reader: asyncio.StreamReader) -> None:
        self._reader = reader

    async def read_frame(self) -> Frame | None:
        """Read the next frame. Returns None on EOF."""
        header = await self._reader.read(HEADER_SIZE)
        if len(header) < HEADER_SIZE:
            return None
        frame_type_byte, length = struct.unpack(HEADER_FMT, header)
        try:
            frame_type = FrameType(frame_type_byte)
        except ValueError:
            frame_type = FrameType.DATA
        payload = b""
        if length > 0:
            payload = await self._reader.readexactly(length)
        return Frame(frame_type, payload)


class FrameWriter:
    """Async frame writer to a StreamWriter."""

    def __init__(self, writer: asyncio.StreamWriter) -> None:
        self._writer = writer

    async def write_frame(self, frame: Frame) -> None:
        """Write a frame to the stream."""
        self._writer.write(frame.encode())
        await self._writer.drain()

    async def write_data(self, payload: bytes) -> None:
        """Write a Data frame."""
        await self.write_frame(Frame.data(payload))
