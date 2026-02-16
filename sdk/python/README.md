# A3S Box Python SDK

Python SDK for A3S Box — embedded MicroVM sandboxes.

## Install

```bash
pip install a3s-box
```

## Quick Start

```python
import asyncio
from a3s_box import BoxSdk, SandboxOptions

async def main():
    sdk = BoxSdk()
    sandbox = await sdk.create(SandboxOptions(image="alpine:latest"))

    result = await sandbox.exec("echo", "hello")
    print(result.stdout)  # "hello\n"
    print(result.metrics.duration_ms)  # execution time

    await sandbox.stop()

asyncio.run(main())
```

## Streaming Exec

```python
async for event in sandbox.exec_stream("make", "build"):
    if event.is_chunk:
        print(event.chunk.data.decode(), end="")
    elif event.is_exit:
        print(f"Exit: {event.exit.exit_code}")
```

## File Transfer

```python
await sandbox.upload(b"hello world", "/tmp/test.txt")
data = await sandbox.download("/tmp/test.txt")
```

## Context Manager

```python
async with await sdk.create(SandboxOptions(image="python:3.12-slim")) as sandbox:
    result = await sandbox.exec("python", "-c", "print('hi')")
    print(result.stdout)
# sandbox is automatically stopped
```

## Options

```python
SandboxOptions(
    image="python:3.12-slim",
    cpus=4,
    memory_mb=2048,
    env={"API_KEY": "secret"},
    mounts=[MountSpec("/local/data", "/data", readonly=True)],
    port_forwards=[PortForward(guest_port=8080, host_port=3000)],
    workspace=WorkspaceConfig(name="my-project"),
)
```

## License

MIT
