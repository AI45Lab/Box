# A3S Box Pause Container

This is the pause/sandbox container used by a3s-box CRI for Kubernetes pod infrastructure.

## Purpose

In Kubernetes, each pod needs an "infrastructure container" (also called pause container) that:
- Holds the network namespace for the pod
- Keeps the pod alive even if all application containers exit
- Provides a stable PID namespace
- Serves as the parent process for all containers in the pod

## Design

This pause container is based on Alpine Linux and uses `tini` as the init process.
It simply sleeps forever, holding the namespaces for the pod.

## Building

```bash
cd /Users/roylin/Desktop/code/a3s/crates/box/images/pause
docker build -t a3s-box/pause:1.0.0 .
docker tag a3s-box/pause:1.0.0 a3s-box/pause:latest
```

## Using with a3s-box CRI

```bash
# Start a3s-box CRI with the pause image
a3s-box-cri \
  --socket /var/run/a3s-box/a3s-box.sock \
  --sandbox-image a3s-box/pause:latest
```

## Image Details

- **Base:** Alpine Linux 3.19
- **Size:** ~8 MB
- **User:** 65534:65534 (nobody)
- **Command:** `/pause` (sleeps forever with tini)

## Security

- Runs as non-root user (UID 65534)
- Minimal attack surface (only tini and sleep)
- No unnecessary packages or tools
- Read-only root filesystem compatible

## Version History

- **1.0.0** (2026-05-04): Initial release
