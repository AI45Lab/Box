"""Tests for the a3s-box Python SDK."""

import asyncio
import json
import struct
import pytest

from a3s_box.transport import Frame, FrameType, FrameReader, FrameWriter, HEADER_FMT
from a3s_box.options import SandboxOptions, MountSpec, PortForward, WorkspaceConfig
from a3s_box.sandbox import ExecResult, ExecMetrics, Sandbox
from a3s_box.stream import StreamType, ExecChunk, ExecExit, ExecEvent, StreamingExec
from a3s_box.sdk import BoxSdk


# --- Transport ---

class TestFrame:
    def test_data_frame_encode(self):
        frame = Frame.data(b"hello")
        encoded = frame.encode()
        assert encoded[0] == FrameType.DATA
        length = struct.unpack("!I", encoded[1:5])[0]
        assert length == 5
        assert encoded[5:] == b"hello"

    def test_heartbeat_frame(self):
        frame = Frame.heartbeat()
        assert frame.frame_type == FrameType.HEARTBEAT
        assert frame.payload == b""

    def test_error_frame(self):
        frame = Frame.error("oops")
        assert frame.frame_type == FrameType.ERROR
        assert frame.payload == b"oops"

    def test_control_frame(self):
        frame = Frame.control(b'{"exit_code":0}')
        assert frame.frame_type == FrameType.CONTROL

    def test_roundtrip_encode_decode(self):
        original = Frame.data(b"test payload")
        encoded = original.encode()
        ft, length = struct.unpack(HEADER_FMT, encoded[:5])
        assert ft == FrameType.DATA
        assert length == len(b"test payload")
        assert encoded[5:] == b"test payload"


class TestFrameReaderWriter:
    @pytest.mark.asyncio
    async def test_read_write_roundtrip(self):
        reader_stream = asyncio.StreamReader()
        frame = Frame.data(b"hello world")
        reader_stream.feed_data(frame.encode())
        reader_stream.feed_eof()

        fr = FrameReader(reader_stream)
        result = await fr.read_frame()
        assert result is not None
        assert result.frame_type == FrameType.DATA
        assert result.payload == b"hello world"

    @pytest.mark.asyncio
    async def test_read_eof(self):
        reader_stream = asyncio.StreamReader()
        reader_stream.feed_eof()
        fr = FrameReader(reader_stream)
        result = await fr.read_frame()
        assert result is None

    @pytest.mark.asyncio
    async def test_read_multiple_frames(self):
        reader_stream = asyncio.StreamReader()
        reader_stream.feed_data(Frame.data(b"one").encode())
        reader_stream.feed_data(Frame.data(b"two").encode())
        reader_stream.feed_eof()

        fr = FrameReader(reader_stream)
        f1 = await fr.read_frame()
        f2 = await fr.read_frame()
        assert f1 is not None and f1.payload == b"one"
        assert f2 is not None and f2.payload == b"two"


# --- Options ---

class TestOptions:
    def test_default_options(self):
        opts = SandboxOptions()
        assert opts.image == "alpine:latest"
        assert opts.cpus == 1
        assert opts.memory_mb == 256
        assert opts.network is True
        assert opts.tee is False
        assert opts.env == {}
        assert opts.mounts == []
        assert opts.port_forwards == []
        assert opts.workspace is None

    def test_custom_options(self):
        opts = SandboxOptions(
            image="python:3.12-slim",
            cpus=4,
            memory_mb=1024,
            env={"KEY": "val"},
            port_forwards=[PortForward(guest_port=8080, host_port=3000)],
            workspace=WorkspaceConfig(name="my-project"),
        )
        assert opts.image == "python:3.12-slim"
        assert opts.cpus == 4
        assert opts.port_forwards[0].guest_port == 8080
        assert opts.workspace is not None
        assert opts.workspace.name == "my-project"
        assert opts.workspace.guest_path == "/workspace"

    def test_mount_spec(self):
        m = MountSpec(host_path="/tmp/data", guest_path="/data", readonly=True)
        assert m.readonly is True

    def test_port_forward_defaults(self):
        pf = PortForward(guest_port=8080)
        assert pf.host_port == 0
        assert pf.protocol == "tcp"


# --- ExecResult ---

class TestExecResult:
    def test_default_metrics(self):
        m = ExecMetrics()
        assert m.duration_ms == 0
        assert m.stdout_bytes == 0
        assert m.stderr_bytes == 0

    def test_exec_result(self):
        r = ExecResult(
            stdout="hello\n",
            stderr="",
            exit_code=0,
            metrics=ExecMetrics(duration_ms=42, stdout_bytes=6),
        )
        assert r.exit_code == 0
        assert r.metrics.duration_ms == 42


# --- Streaming ---

class TestStreamTypes:
    def test_stream_type(self):
        assert StreamType.STDOUT.value == "Stdout"
        assert StreamType.STDERR.value == "Stderr"

    def test_exec_chunk(self):
        chunk = ExecChunk(stream=StreamType.STDOUT, data=b"hello")
        assert chunk.stream == StreamType.STDOUT
        assert chunk.data == b"hello"

    def test_exec_exit(self):
        exit = ExecExit(exit_code=42)
        assert exit.exit_code == 42

    def test_exec_event_chunk(self):
        event = ExecEvent(chunk=ExecChunk(StreamType.STDOUT, b"data"))
        assert event.is_chunk
        assert not event.is_exit

    def test_exec_event_exit(self):
        event = ExecEvent(exit=ExecExit(0))
        assert event.is_exit
        assert not event.is_chunk


class TestStreamingExec:
    @pytest.mark.asyncio
    async def test_collect(self):
        reader_stream = asyncio.StreamReader()

        # Feed stdout chunk
        chunk1 = json.dumps({"stream": "Stdout", "data": list(b"hello ")}).encode()
        reader_stream.feed_data(Frame.data(chunk1).encode())

        # Feed stderr chunk
        chunk2 = json.dumps({"stream": "Stderr", "data": list(b"warn")}).encode()
        reader_stream.feed_data(Frame.data(chunk2).encode())

        # Feed exit
        exit_payload = json.dumps({"exit_code": 0}).encode()
        reader_stream.feed_data(Frame.control(exit_payload).encode())
        reader_stream.feed_eof()

        stream = StreamingExec(FrameReader(reader_stream))
        stdout, stderr, code = await stream.collect()
        assert stdout == b"hello "
        assert stderr == b"warn"
        assert code == 0
        assert stream.is_done

    @pytest.mark.asyncio
    async def test_async_iter(self):
        reader_stream = asyncio.StreamReader()
        chunk = json.dumps({"stream": "Stdout", "data": list(b"hi")}).encode()
        reader_stream.feed_data(Frame.data(chunk).encode())
        exit_payload = json.dumps({"exit_code": 1}).encode()
        reader_stream.feed_data(Frame.control(exit_payload).encode())
        reader_stream.feed_eof()

        events = []
        async for event in StreamingExec(FrameReader(reader_stream)):
            events.append(event)
        assert len(events) == 2
        assert events[0].is_chunk
        assert events[1].is_exit
        assert events[1].exit.exit_code == 1


# --- SDK ---

class TestBoxSdk:
    def test_init_creates_dirs(self, tmp_path):
        sdk = BoxSdk(home_dir=tmp_path / ".a3s")
        assert (tmp_path / ".a3s" / "images").exists()
        assert (tmp_path / ".a3s" / "sandboxes").exists()
        assert (tmp_path / ".a3s" / "workspaces").exists()
        assert sdk.home_dir == tmp_path / ".a3s"

    def test_build_run_command(self, tmp_path):
        sdk = BoxSdk(home_dir=tmp_path / ".a3s")
        opts = SandboxOptions(
            image="python:3.12",
            cpus=2,
            memory_mb=512,
            env={"FOO": "bar"},
            port_forwards=[PortForward(guest_port=8080, host_port=3000)],
        )
        cmd = sdk._build_run_command(opts, "test-id", "test-name")
        assert "a3s-box" in cmd[0]
        assert "python:3.12" in cmd
        assert "--cpus" in cmd
        assert "2" in cmd
        assert "-e" in cmd
        assert "FOO=bar" in cmd
        assert "-p" in cmd
        assert "3000:8080" in cmd

    def test_build_run_command_workspace(self, tmp_path):
        sdk = BoxSdk(home_dir=tmp_path / ".a3s")
        opts = SandboxOptions(
            image="alpine:latest",
            workspace=WorkspaceConfig(name="proj"),
        )
        cmd = sdk._build_run_command(opts, "id", "name")
        ws_dir = tmp_path / ".a3s" / "workspaces" / "proj"
        assert ws_dir.exists()
        assert any("proj" in c and "/workspace" in c for c in cmd)

    def test_repr(self, tmp_path):
        sdk = BoxSdk(home_dir=tmp_path / ".a3s")
        assert ".a3s" in repr(sdk)
