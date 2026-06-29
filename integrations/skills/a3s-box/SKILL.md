---
name: a3s-box
description: Drive the a3s-box microVM sandbox CLI — a Docker-like runtime that runs OCI/container images in hardware-isolated microVMs (libkrun). Use when the user wants to run, build, exec into, snapshot, or tear down containers with a3s-box, sandbox untrusted or agent-generated code, spin up throwaway isolated environments, or mentions a3s-box / microVM / libkrun / "run this safely in a sandbox". Teaches the box lifecycle, the `--` argv separator, the networking model, and error recovery. Not for plain Docker/Podman (different CLI, same verbs).
allowed-tools: Bash(a3s-box*), Bash(curl*), Read(*), Grep(*)
---

# a3s-box — drive the microVM sandbox

`a3s-box` runs OCI images inside hardware-isolated microVMs with a Docker-like
CLI (verbs mirror Docker). **Preflight once:** `a3s-box info` (confirms
virtualization + home dir).

## Mental model (these cause silent failures)

- **Lifecycle:** `created → running → paused → stopped → dead`.
  - `run` = create **and** start. `create` stops at `created` → `start` it.
    `snapshot restore` → `created` → `start` it.
  - A box dies when its **PID 1 exits** — it needs a foreground process.
    `run -d alpine` runs `sh`, which exits at once → `dead`. Use a long-lived
    command: `run -d nginx`, or `run -d alpine -- sleep 3600`.
  - `exec`/`shell`/`cp`/`top`/`attach` need a **running** box. `start` accepts
    only `created|stopped|dead`. `rm` removes `created`/`stopped`/`dead` without
    `-f`; `-f` is only for a `running`/`paused` box.
- **The `--` rule:** the in-box command goes **after `--`**.
  `a3s-box exec <box> -- <cmd>` (required); `a3s-box run <image> [-- <cmd>]`
  (optional override). Missing it → `error: unexpected argument '…' found`.
- **`exec` has a 5-second default timeout** — long builds/installs/tests get
  killed mid-run (and look like a failure). Use `--timeout 300` (or `--timeout 0`
  to disable): `a3s-box exec --timeout 300 web -- <cmd>`.
- **Detached boxes need the monitor.** `run -d` prints the id and exits; its
  health/restart task dies with it. For `--restart` policies and health checks to
  fire, run `a3s-box monitor` in the background first.
- **No in-guest localhost by default.** A box can't reach its own service on
  `127.0.0.1` (so `--health-cmd 'curl localhost'` and in-box curl fail). To make
  in-guest localhost work, attach a bridge network (recipe below). To check a
  service from your side, curl the **published HOST port** (see Ports).
- **Ports:** publish with `-p HOST:GUEST` (TCP). But `ps`'s PORTS column and
  `a3s-box port <box>` render `GUEST -> 0.0.0.0:HOST` — the host port to curl is
  the one after `0.0.0.0:`. Read it with `a3s-box port <box>`.
- **Output streams:** `run -d` prints a human `Creating box <name> (<id>)...`
  line, the full box id, and (when uncached) image-pull progress — all to
  **stdout**; only tracing/WARN/ERROR go to **stderr**. Don't parse the id from
  stdout — reference boxes by `--name`. JSON where offered: `inspect`,
  `image-inspect`, `snapshot ls --json`, and `ps --format json` (2.6+ only; on
  2.0 use `ps --format '{{.ID}} {{.Status}} {{.Names}}'`).

## Run → verify → exec → teardown

```sh
a3s-box info
a3s-box run -d --name web -p 8080:80 app:dev    # reference by --name, not stdout
a3s-box ps -a --filter name=web                  # MUST use -a; expect STATUS=running
a3s-box port web                                 # host port = value after 0.0.0.0:
curl -fsS http://localhost:8080/                 # confirm SERVING from the host
a3s-box exec --timeout 60 web -- env             # in-box command after --
a3s-box logs web                                 # logs print on stderr
a3s-box stop web && a3s-box rm web
```

A box can boot then die — always verify with `ps -a` (a `dead`/gone box does
**not** appear in plain `ps`; empty output means dead-or-gone, not "name typo").
If STATUS=`dead`, its main process exited: read `a3s-box inspect web`
(`.State.ExitCode`, summary `dead (Exit N)`) and `a3s-box logs web`.

## Errors → fix

| Error (on stderr) | Cause → fix |
|---|---|
| `error: unexpected argument '…' found` (Usage … `-- <CMD>`) | missing `--` → `exec <box> -- <cmd>` |
| `Box X is not running` | stopped/created/dead → `a3s-box start X`; if just run, it died on boot → `inspect`/`logs X` |
| `Box X is not running (status: dead)` | PID 1 exited → `a3s-box inspect X` (`.State.ExitCode`); run a long-lived command |
| `No such box: X` | wrong ref → `a3s-box ps -a` to find name/id |
| `WARN … heartbeat failed, exec will not be available` | guest booted but exec channel never came up → unhealthy; `logs X`, recreate |
| `libkrun call failed status=-17 … krun_add_vsock_port2` / `VM boot failed` | started an already-running/stale box → `a3s-box ps`; if running just `exec`; if wedged `stop X` then `start X` |

## Core commands (non-obvious; full verb list: `a3s-box --help`)

| Goal | Command |
|---|---|
| Run one command, throwaway | `a3s-box run --rm alpine -- echo hi` |
| Create then start | `a3s-box create --name w nginx` → `a3s-box start w` |
| Exec / interactive shell | `a3s-box exec -it web -- /bin/sh` · `a3s-box shell web` |
| List, custom columns | `a3s-box ps -a --format '{{.ID}} {{.Status}} {{.Names}}'` |
| Build image | `a3s-box build -t app:dev .` |
| Copy in / out | `a3s-box cp ./f web:/data/f` · `a3s-box cp web:/data/f ./f` |
| Commit box → image | `a3s-box commit web app:snap` |

Resource/isolation flags on `run`/`create` — `--cpus` (default 2), `--memory`
(default 512m), `-e K=V`, `-v host:guest`, `--read-only`,
`--cap-drop ALL --cap-add NET_BIND_SERVICE`, `--pids-limit`, `--network`,
`--init`, … : `a3s-box run --help`.

## Recipes

**Working in-guest localhost (bridge network)**
```sh
a3s-box network create mynet --subnet 10.89.0.0/24
a3s-box run -d --name api --network mynet -p 8080:80 app:dev
```

**Snapshot → restore** (filesystem snapshot; restored box lands in `created`)
```sh
a3s-box snapshot create web                 # create from a running/stopped box
a3s-box snapshot ls --json
a3s-box snapshot restore <snap-id> --name restored   # name it → no ps scraping
a3s-box ps -a --filter name=restored && a3s-box start restored
```

## Finding exact flags & versions

`a3s-box <command> --help` works for every command, and nested help works too:
`a3s-box snapshot create --help`, `a3s-box network create --help`, etc. Check the
build with `a3s-box version`; newer builds (≥2.4) add `pool run`,
`snapshot prune`, and `monitor --install/--metrics-addr`.

## More

Other areas — `network`/`volume`/`compose`/`pool` (warm pre-boot VMs), registry
`login`/`push`, `events`/`audit`/`df`, and TEE (`--tee`/`--tee-simulate`,
`attest`/`seal`/`unseal`/`inject-secret`, `--sidecar`): `a3s-box --help`, then
`a3s-box <cmd> --help`.
