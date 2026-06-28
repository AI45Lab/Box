//! Soak test: sustained fork-eval churn must stay leak-free and memory-stable.
//!
//! `#[ignore]` by default. Run on a `/dev/kvm` host:
//! ```text
//! A3S_BOX=/path/to/a3s-box A3S_SDK_SOAK_FORKS=2000 \
//!   cargo test -p a3s-box-sdk --test soak_kvm -- --ignored --nocapture --test-threads=1
//! ```
//! Knobs: `A3S_SDK_SOAK_FORKS` (total fork-evals, default 200),
//! `A3S_SDK_SOAK_CONC` (max concurrency, default 8), `A3S_SDK_TEST_IMAGE`.

use a3s_box_sdk::pipeline::{warm_base, Step, WarmBase};
use std::process::Command;
use std::time::Instant;

fn a3s_box() -> String {
    std::env::var("A3S_BOX").unwrap_or_else(|_| "a3s-box".into())
}

fn image() -> String {
    std::env::var("A3S_SDK_TEST_IMAGE")
        .unwrap_or_else(|_| "docker.m.daocloud.io/library/alpine:latest".into())
}

fn kvm_ready() -> bool {
    match Command::new(a3s_box()).arg("info").output() {
        Ok(o) => o.status.success() && String::from_utf8_lossy(&o.stdout).contains("available"),
        Err(_) => false,
    }
}

/// Resource names embed this process's pid (`ci-base-<key>-<pid>-<seq>...`); scope
/// every leak count to `-<pid>-` so a *concurrent* pipeline on the same host can't
/// perturb the assertions (the global namespace is shared).
fn marker() -> String {
    format!("-{}-", std::process::id())
}

/// Count THIS run's boxes whose name also contains `needle`.
fn count_box_names(needle: &str) -> usize {
    let m = marker();
    let out = Command::new(a3s_box())
        .args(["ps", "-a", "--format", "{{.Names}}"])
        .output()
        .expect("a3s-box ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(needle) && l.contains(&m))
        .count()
}

/// Count THIS run's ci-base snapshots.
fn count_my_snaps() -> usize {
    let m = marker();
    let out = Command::new(a3s_box())
        .args(["snapshot", "ls"])
        .output()
        .expect("a3s-box snapshot ls");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("ci-base-") && l.contains(&m))
        .count()
}

fn rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "needs a real a3s-box + /dev/kvm; long-running"]
fn soak_fork_eval_is_leak_free_and_stable() {
    if !kvm_ready() {
        eprintln!("SKIP soak_fork_eval_is_leak_free_and_stable: no A3S_BOX/KVM");
        return;
    }
    let target = env_usize("A3S_SDK_SOAK_FORKS", 200);
    let conc = env_usize("A3S_SDK_SOAK_CONC", 8);
    let batch = 20usize;

    let snaps0 = count_my_snaps();
    let rss0 = rss_kib();

    let base = warm_base(WarmBase::new(image(), "true")).expect("warm_base");
    let snap_base = count_my_snaps();
    assert_eq!(
        snap_base,
        snaps0 + 1,
        "warm_base should add exactly one snapshot"
    );

    let start = Instant::now();
    let mut done = 0usize;
    let mut gen = 0usize;
    while done < target {
        let n = batch.min(target - done);
        let steps: Vec<Step> = (0..n)
            .map(|i| Step::new(format!("g{gen}s{i}"), "echo '::metric ok=1'"))
            .collect();
        let rep = base.run_parallel(steps, conc);
        assert!(
            rep.passed,
            "gen {gen} had failures: {:?}",
            rep.failures()
                .iter()
                .map(|s| {
                    (
                        s.name.clone(),
                        s.exit_code,
                        s.stderr.chars().take(160).collect::<String>(),
                    )
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(rep.steps.len(), n);
        // every step parsed its metric (proves the channel survives churn).
        assert!(rep.steps.iter().all(|s| s.metrics.get("ok") == Some(&1.0)));
        // leak gate per generation: no fork box lingers, snapshot count stays flat.
        assert_eq!(
            count_box_names("-snap-job"),
            0,
            "leaked fork boxes after gen {gen}"
        );
        assert_eq!(count_my_snaps(), snap_base, "snapshot drift at gen {gen}");
        done += n;
        gen += 1;
    }

    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "SOAK: {done} fork-evals across {gen} generations in {elapsed:.1}s = {:.1} forks/s",
        done as f64 / elapsed
    );

    base.dispose();
    assert_eq!(count_my_snaps(), snaps0, "snapshot leaked after dispose");
    assert_eq!(
        count_box_names("ci-base-"),
        0,
        "ci-base box leaked after soak"
    );

    if let (Some(a), Some(b)) = (rss0, rss_kib()) {
        let grow = b.saturating_sub(a);
        eprintln!("SOAK RSS: {a} KiB -> {b} KiB (+{grow} KiB over {done} fork-evals)");
        assert!(
            grow < 200_000,
            "RSS grew {grow} KiB over {done} fork-evals — possible orchestrator leak"
        );
    }
}
