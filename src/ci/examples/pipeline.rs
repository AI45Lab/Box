//! A tiny CI pipeline driven from Rust. Needs `a3s-box` + /dev/kvm.
//!
//! Run: `A3S_BOX=/path/to/a3s-box cargo run -p a3s-box-ci --example pipeline`
//! (set `DEMO_IMAGE` to override the alpine default).

use a3s_box_ci::{warm_base, FileCache, Step, WarmBase};

fn main() -> Result<(), a3s_box_ci::CiError> {
    let image = std::env::var("DEMO_IMAGE")
        .unwrap_or_else(|_| "docker.m.daocloud.io/library/alpine:latest".to_string());
    let _ = std::fs::remove_dir_all("/tmp/.a3s-ci-demo"); // fresh cache for the demo (before create)
    let cache = FileCache::new("/tmp/.a3s-ci-demo")?;

    // Warm the base once (here: write a marker = "deps installed"), snapshot it.
    let mut base = warm_base(WarmBase::new(image, "echo DEPS-INSTALLED > /warmed").cache(&cache))?;

    let r = base.step(Step::new("read", "cat /warmed"))?;
    println!(
        "read   -> code={} out={:?} cached={}",
        r.exit_code,
        r.logs.trim(),
        r.cached
    );

    let r2 = base.step(Step::new("read", "cat /warmed"))?; // identical -> cache hit
    println!("read#2 -> cached={}", r2.cached);

    match base.step(Step::new("fail", "exit 7")) {
        Err(e) => println!("fail-fast ok: {e}"),
        Ok(_) => println!("ERROR: fail step did not error"),
    }

    base.dispose();
    println!("demo ok");
    Ok(())
}
