# a3s-box-ci

Programmable CI on a3s-box. A pipeline is a **Rust program**, not a YAML file —
`a3s-box-ci` runs each step in its own MicroVM (one Linux kernel per step, so an
untrusted step can't escape to the host or a sibling step). The DAG is your code:
sequence with plain calls, fan out with threads. No engine, no scheduler.

Dependency-free: a thin wrapper over the `a3s-box` CLI (which owns the box
lifecycle and state). Set `A3S_BOX` if `a3s-box` is not on `PATH`.

## Model

Warm a base box **once** (clone + install deps), snapshot it, fork per step:

```rust
use a3s_box_ci::{warm_base, WarmBase, FileCache, Step};

fn main() -> Result<(), a3s_box_ci::CiError> {
    let cache = FileCache::new(".ci-cache")?;          // skip a step when its inputs are unchanged
    let mut base = warm_base(
        WarmBase::new("node:20", "git clone $REPO /w && cd /w && npm ci")   // runs ONCE
            .env("REPO", "https://github.com/me/app")
            .cache(&cache),
    )?;
    base.step(Step::new("lint", "cd /w && npm run lint"))?;
    base.step(Step::new("test", "cd /w && npm test"))?;   // nonzero exit -> Err (fail-fast)
    base.step(Step::new("build", "cd /w && npm run build"))?;
    base.dispose();                                       // drops the snapshot
    Ok(())
}
```

`Step::allow_failure()` keeps the pipeline going on a non-zero exit; `Step::input(..)`
adds extra cache-key parts. Parallel steps = spawn threads (each `step` is blocking).

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
cargo test -p a3s-box-ci                                            # offline unit tests
A3S_BOX=/path/to/a3s-box cargo run -p a3s-box-ci --example pipeline  # live, needs /dev/kvm
```
