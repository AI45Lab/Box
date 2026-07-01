# a3s-box benchmarks

The performance and "leak-free" claims in the README/CHANGELOG (cold boot,
snapshot-fork, warm-pool acquire, churn leak-freeness) were previously prose
with no reproducible source. [`bench.sh`](./bench.sh) makes them **independently
reproducible**: it drives the real `a3s-box` CLI end-to-end and reports
wall-clock latencies plus a hard leak assertion — so anyone can re-measure on
their own hardware instead of trusting a number in a doc.

## Requirements

A Linux host with **`/dev/kvm`** (real microVMs only boot there) and `a3s-box`
on `PATH` (or set `A3S_BOX`). The boot benchmarks are meaningless without KVM.

## Usage

```bash
bench/bench.sh            # all four benchmarks
bench/bench.sh cold       # cold-boot latency only
bench/bench.sh warm       # warm-pool acquire latency
bench/bench.sh fork       # snapshot-fork pool fill (cold-fill vs CoW restore)
bench/bench.sh leak       # churn + leak assertion (exit != 0 on leak)
```

Tunables (env):

| Var | Default | Meaning |
|-----|---------|---------|
| `A3S_BOX` | `a3s-box` | binary under test |
| `IMAGE` | `alpine:latest` | OCI image to benchmark |
| `RUNS` | `20` | samples per latency benchmark |
| `POOL_SIZE` | `16` | warm-pool / fork fill size |
| `CHURN` | `30` | create/run/remove cycles for the leak test |

## What it measures

- **cold** — `run --rm IMAGE -- true` wall-clock, reported as p50 / p90 / min
  over `RUNS` samples.
- **warm** — `pool start` then `pool run` acquire latency (p50 / p90 / min).
- **fork** — `pool start --size N` fill time **without** vs **with**
  `--snapshot-fork`, as total + amortized-per-VM, so the CoW speedup is a
  measured ratio rather than an asserted one.
- **leak** — snapshots host-side resources a leak would grow (orphan
  `a3s-box-shim` processes, overlay mounts under `~/.a3s/boxes`, box dirs),
  runs `CHURN` `run --rm` cycles, then asserts they return to baseline.
  **Exits non-zero on any leak**, so it is CI-gateable.

## Wiring into CI

The leak assertion's non-zero exit makes it a natural gate on the self-hosted
KVM runner (see [`../docs/ci-kvm-runner.md`](../docs/ci-kvm-runner.md)): add a
`bench/bench.sh leak` step to the `integration-kvm` job to catch a resource leak
regression automatically, instead of relying on a manual churn run.

## Updating the published numbers

When you quote a number in the README, regenerate it here first and paste the
harness output. A claim with a reproducible command behind it is worth more than
a polished number with no source.
