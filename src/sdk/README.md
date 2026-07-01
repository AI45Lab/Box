# a3s-box-sdk

The Rust SDK for **a3s-box**. Today it provides a **programmable CI/CD pipeline** API
(`a3s_box_sdk::pipeline`) — a pipeline is a Rust program, not a YAML file, and each step
runs in its own MicroVM (one Linux kernel per step, so an untrusted step can't escape to
the host or a sibling step). More capabilities will be added over time; the crate is
intentionally not limited to CI.

Dependency-free: a thin wrapper over the `a3s-box` CLI (which owns the box lifecycle and
state). Set `A3S_BOX` if `a3s-box` is not on `PATH`.

## Pipelines

Warm a base box **once** (clone + install deps), snapshot it, fork per step.
Run steps **sequentially** (fail-fast) or **in parallel** (collect-all → a typed
report):

```rust
use a3s_box_sdk::pipeline::{warm_base, WarmBase, FileCache, Step};

fn main() -> Result<(), a3s_box_sdk::pipeline::PipelineError> {
    let cache = FileCache::new(".ci-cache")?;          // skip a step when its inputs are unchanged
    let base = warm_base(
        WarmBase::new("node:20", "git clone $REPO /w && cd /w && npm ci")   // runs ONCE
            .env("REPO", "https://github.com/me/app")
            .cache(&cache),
    )?;

    // Sequential, fail-fast: a non-zero exit returns Err.
    base.step(Step::new("lint", "cd /w && npm run lint"))?;

    // Parallel, collect-all: each step is an isolated CoW fork; <=4 at a time.
    let report = base.run_parallel(vec![
        Step::new("test",  "cd /w && npm test"),
        Step::new("build", "cd /w && npm run build"),
    ], 4);

    println!("{}", report.to_json());   // {"passed":..,"total_ms":..,"steps":[..]}
    if !report.passed { /* inspect report.failures() */ }
    Ok(())  // `base` drops here -> snapshot auto-removed (or call base.dispose())
}
```

- `run_parallel` is the way to use a3s-box's cheap (~ms) CoW fork at scale — a
  matrix / evolution-style batch — without hand-rolling threads (every method
  takes `&self`).
- A step reports a metric by printing `::metric <key>=<number>` to stdout; it
  surfaces as `StepResult::metrics` (the scoring channel for a selection loop).
- `StepResult` carries separated `stdout`/`stderr`, `duration_ms`, and `cached`;
  `Report::to_json()` is the machine-readable handoff to an agent/scorer.
- `Step::allow_failure()` keeps a non-zero step from failing the run; `Step::input(..)`
  adds extra cache-key parts.

The base **auto-disposes** its snapshot on drop, and each per-step box is removed
on every path (including a panic), so a long-running batch doesn't leak.

## Why forking is cheap

a3s-box's `snapshot restore` is **copy-on-write**: each fork mounts the snapshot's
pristine rootfs as a read-only overlay lower with its own upper — near-instant,
a few MB per fork, and isolated. So snapshot-per-step fan-out costs almost nothing.

## What it hides

CLI footguns verified on a real KVM host, so you don't hit them: `run`/`exec` need
`--` before the command; `snapshot restore` yields a *created* box (started before
exec); `snapshot rm` keys on snapshot ID, not name; `rm -f` of a missing box is a
no-op here (idempotent reruns).

## Run the example

```bash
cargo test -p a3s-box-sdk                                            # offline unit tests
A3S_BOX=/path/to/a3s-box cargo run -p a3s-box-sdk --example pipeline  # live, needs /dev/kvm
```
