"""Sandbox configuration options."""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class MountSpec:
    """A host-to-guest mount specification."""

    host_path: str
    guest_path: str
    readonly: bool = False


@dataclass
class PortForward:
    """Port forwarding rule: expose a guest port on the host."""

    guest_port: int
    host_port: int = 0
    protocol: str = "tcp"


@dataclass
class WorkspaceConfig:
    """Persistent workspace configuration."""

    name: str
    guest_path: str = "/workspace"


@dataclass
class SandboxOptions:
    """Options for creating a new sandbox."""

    image: str = "alpine:latest"
    cpus: int = 1
    memory_mb: int = 256
    env: dict[str, str] = field(default_factory=dict)
    workdir: str | None = None
    mounts: list[MountSpec] = field(default_factory=list)
    network: bool = True
    tee: bool = False
    name: str | None = None
    port_forwards: list[PortForward] = field(default_factory=list)
    workspace: WorkspaceConfig | None = None
