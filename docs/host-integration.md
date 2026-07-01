# Host Integration Smoke Guide

This guide defines the macOS and Linux validation path for a3s-box. Run these
commands from the crate repository root (`crates/box`), not from the monorepo
root.

## Validation ladder

| Level | Host requirements | Command |
| --- | --- | --- |
| Stub baseline | macOS or Linux with Rust, C compiler, and protoc | `scripts/host-integration-smoke.sh` |
| Core MicroVM smoke | macOS Apple Silicon/HVF or Linux KVM, libkrun, Linux guest init, runnable image | `scripts/host-integration-smoke.sh --core` |
| Host command matrix | Same as core smoke; optional registry credentials for push coverage | `scripts/host-integration-smoke.sh --host` |
| Linux Dockerfile `RUN` | Linux, root, chroot-capable filesystem, local Alpine OCI archive | `sudo -E scripts/host-integration-smoke.sh --linux-run --no-pure` |
| CRI smoke | macOS or Linux MicroVM host, `crictl`, CRI images | `scripts/host-integration-smoke.sh --cri` |
| Host soak | Same as the selected host-backed suites; enough time to expose leaks and lost updates | `scripts/host-integration-smoke.sh --no-pure --core --host --soak` |
| Production cluster validation | Explicitly enrolled production Linux nodes with `/dev/kvm`, containerd RuntimeClass wiring, labels, taints, and rollback prepared | See [`production-cluster-tests.md`](./production-cluster-tests.md) |

The default command runs formatting, clippy, unit tests, and integration test
compilation with `A3S_DEPS_STUB=1`. It does not require a hypervisor and should
be safe on developer laptops and CI workers. Host-backed `--core` and `--host`
runs require an OCI archive by default; set `A3S_BOX_ALLOW_REGISTRY_PULL=1` only
when you intentionally want live registry pulls.

## macOS core smoke

Use Apple Silicon. Intel macOS is not a supported runtime target.

```bash
cd crates/box

# Optional but recommended for offline/reproducible runs.
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_IMAGE_TAR="$A3S_BOX_TEST_ALPINE_TAR"
export A3S_BOX_SMOKE_SKIP_PULL=1
export A3S_BOX_SMOKE_TIMEOUT_SECS=300

scripts/host-integration-smoke.sh --core
```

If you do not have an offline archive and want to pull from the registry during
the run, add:

```bash
export A3S_BOX_ALLOW_REGISTRY_PULL=1
```

If the Linux guest init binary is missing, build it for the guest target before
running the smoke:

```bash
rustup target add aarch64-unknown-linux-musl
cargo build -p a3s-box-guest-init --target aarch64-unknown-linux-musl
scripts/host-integration-smoke.sh --core
```

If direct cross-build linking fails on the host, install `cargo-zigbuild` and
use `cargo zigbuild -p a3s-box-guest-init --target aarch64-unknown-linux-musl`
instead.

On macOS, `src/target/debug/a3s-box-guest-init` is a host Mach-O binary and is
not accepted as a guest artifact. The runner expects the Linux binary under
`src/target/<linux-musl-target>/{debug,release}/a3s-box-guest-init`.

## Linux core smoke

Use a host with `/dev/kvm` available to the current user. For offline runs, use
the same OCI archive variables as macOS.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_IMAGE_TAR="$A3S_BOX_TEST_ALPINE_TAR"
export A3S_BOX_SMOKE_SKIP_PULL=1

scripts/host-integration-smoke.sh --core
```

Use `A3S_BOX_ALLOW_REGISTRY_PULL=1` instead of the archive variables only for
network-backed validation; offline archive runs are the release gate default.

If `/dev/kvm` is permission denied, add the user to the `kvm` group and start a
new login session:

```bash
sudo usermod -aG kvm "$USER"
```

## Linux Dockerfile `RUN` smoke

Dockerfile `RUN` uses an isolated Linux chroot path. It is intentionally
Linux-only and requires root. The smoke test must use a local Alpine OCI
archive because it validates the chroot build path, not registry access.

```bash
cd crates/box
sudo -E env A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar \
  scripts/host-integration-smoke.sh --linux-run --no-pure
```

macOS does not run Dockerfile `RUN` by default. The unsafe host execution path
is only for local experiments and requires `A3S_BOX_UNSAFE_HOST_RUN=1`; it is
not part of the product smoke matrix.

## Host command matrix

The host matrix extends the core smoke with VM lifecycle commands, Compose,
copy, stats, snapshots, network operations, image tagging/saving, local build,
and optional registry push coverage.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_HOST_SMOKE_TIMEOUT_SECS=300

scripts/host-integration-smoke.sh --host
```

Enable registry push coverage only against a disposable tag template:

```bash
export A3S_BOX_PUSH_TEST_REF='registry.example/a3s/box-push-test:{tag}'
export A3S_BOX_PUSH_USERNAME='...'
export A3S_BOX_PUSH_PASSWORD='...'
scripts/host-integration-smoke.sh --host
```

## CRI smoke

The CRI smoke is experimental and intentionally opt-in. It starts the
`a3s-box-cri` server, drives it through `crictl`, and launches a pod sandbox
with two containers.

```bash
cd crates/box
export A3S_BOX_CRI_CRICTL=/path/to/crictl
export A3S_BOX_CRI_SMOKE_IMAGE=busybox:latest
export A3S_BOX_CRI_SMOKE_AGENT_IMAGE=ghcr.io/a3s-box/code:v0.1.0

scripts/host-integration-smoke.sh --cri
```

Use `A3S_BOX_CRI_SMOKE_SKIP_PULL=1` and `A3S_BOX_CRI_SMOKE_IMAGE_DIR` when the
image store is preloaded and the run must stay offline.

## Host soak

Use `--soak` after the single-pass host-backed suites are already green. The
runner repeats the selected real suites, runs `bench/bench.sh leak` and
`bench/bench.sh race` by default, samples host resource counts, and writes an
evidence directory under `src/target/a3s-box-soak/`.

```bash
cd crates/box
export A3S_BOX_TEST_ALPINE_TAR=/path/to/alpine-oci.tar
export A3S_BOX_SMOKE_SKIP_PULL=1
export A3S_BOX_HOST_SMOKE_TIMEOUT_SECS=300
export IMAGE=docker.m.daocloud.io/library/alpine:latest
export CHURN=2500
export RACE=32

scripts/host-integration-smoke.sh \
  --no-pure \
  --core \
  --host \
  --soak \
  --soak-duration 7200 \
  --soak-verify-min-duration-secs 7200 \
  --soak-verify-min-sample-span-secs 7200 \
  --soak-verify-min-samples 4
```

For a short rehearsal, cap the loop instead of waiting for the time limit:

```bash
scripts/host-integration-smoke.sh \
  --no-pure \
  --core \
  --host \
  --soak \
  --soak-iterations 1 \
  --soak-duration 0
```

The evidence directory contains `metadata.txt`, `resource-samples.tsv`, per-step
iteration logs, CLI state snapshots, `summary.txt`, and `verify.out`. Keep the
directory with the release candidate when the soak is used as a gate. The runner
verifies the bundle before returning success, including resource counters and
required snapshot/log files. `metadata.txt` must include parseable `started_at`,
non-negative integer soak gate fields, and `selected_suites` flags for `core`,
`host`, `linux_run`, `cri`, and `bench`; every selected suite must have its
corresponding per-iteration log, with bench requiring both `bench-leak` and
`bench-race` logs. `resource-samples.tsv` timestamps must be parseable and
monotonic, counters must be non-negative integers, there must be exactly one
`start` row and one `final` row, and `summary.txt` duration must not be shorter
than the sampled time span. Pass the `--soak-verify-*` options on release-gate
runs so the runner also enforces minimum duration, sample span, and sample count
before returning success. Saved bundles keep those gate values in `metadata.txt`,
so later verifier runs enforce the recorded gates even when the re-check command
does not repeat every `--min-*` option.
Failed host soaks write `result=fail` plus the failed
iteration count and, when available, `exit_code`, `failed_at`, and
`failed_command`; to re-check a saved bundle, run:

```bash
deploy/scripts/verify-soak-evidence.sh \
  --kind host \
  --min-duration-secs 7200 \
  --min-sample-span-secs 7200 \
  --min-samples 4 \
  <evidence-dir>
```

Before using the verifier as a release gate after script changes, run the local
self-test:

```bash
deploy/scripts/soak-evidence-self-test.sh
```

## Result recording

When a host-backed run passes, record:

- host OS and architecture;
- `a3s-box info` output;
- exact command and environment variables;
- image archive digest or registry image digests;
- test summary line from Cargo.

Keep macOS HVF and Linux KVM records separate because bridge networking and
Dockerfile `RUN` behavior intentionally differ by platform.

## Production Cluster Validation

The single-host ladder above is the prerequisite for production-cluster testing.
For production Linux servers, use
[`production-cluster-tests.md`](./production-cluster-tests.md). It adds the
cluster safety model, node admission checklist, RuntimeClass smoke, integration
matrix, 2-hour guardrail soak, 24-hour release soak, 72-hour endurance soak,
stop conditions, and evidence bundle required before widening RuntimeClass use.
The cluster RuntimeClass churn loop is executable through
`deploy/scripts/runtimeclass-soak.sh`; saved host or cluster evidence bundles
can be re-checked with `deploy/scripts/verify-soak-evidence.sh`. Cluster
evidence is accepted only when the final state has selected nodes, completed
jobs, no failed pods/jobs, no unexpected pod restarts, and no Pending/Unknown
pods or active jobs left behind. The `final-pod-runtimeclasses.tsv` and
`job-runtimeclass.txt` artifacts must also cover the same unique final pod set
and prove `runtimeClassName: a3s-box`; `runtimeclass.yaml` must show the
RuntimeClass object is named `a3s-box`, uses handler `a3s-box`, and keeps
`scheduling.nodeSelector.a3s-box.io/runtime: "true"`;
`smoke-exec.txt` must prove `kubectl exec` succeeds on every selected node;
`complex-exec.txt` must prove exec succeeds against every long-lived workload
(`redis`, `postgres`, `nginx`, and `python`);
`complex-logs.txt` must include workload-prefixed `REDIS_SOAK`, `PG_SOAK`,
`NGINX_SOAK`, and `PY_SOAK` markers;
`job-pod-statuses.tsv` must prove exactly the declared number of Succeeded churn
pods with zero restarts on selected nodes, and those pods must be covered by the
final pod evidence; `job-logs.txt` must include `A3S_BOX_JOB_START`,
`A3S_BOX_JOB_RUNTIME_CLASS=a3s-box`, and `A3S_BOX_JOB_DONE` markers exactly
matching the declared Job completion count;
`selected-node-labels.tsv` must prove every selected node carries the required
production-soak labels; `final-pod-nodes.tsv` must show all final pods stayed on
the unique, count-matched selected node list; and `final-pod-statuses.tsv` must
show only `Running` or `Succeeded` final pods with zero restarts. `events.tsv`
must contain only `Normal` validation namespace events. Structural Kubernetes
artifacts must not contain captured `kubectl` connection or API errors. When
`--cleanup` is enabled, the runner waits up to `--cleanup-timeout` seconds and
`post-cleanup-counts.tsv` must be a single timestamped row, not earlier than the
final resource sample, showing zero generated smoke, complex, and churn
workloads left behind.
