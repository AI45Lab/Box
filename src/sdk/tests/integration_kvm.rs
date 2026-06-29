//! Real-microVM integration tests for the programmable-CI pipeline.
//!
//! `#[ignore]` by default. Run on a `/dev/kvm` host:
//! ```text
//! A3S_BOX=/path/to/a3s-box \
//!   cargo test -p a3s-box-sdk --test integration_kvm -- --ignored --nocapture --test-threads=1
//! ```
//! Override the image with `A3S_SDK_TEST_IMAGE` (default: a daocloud alpine mirror).
//! Each test self-skips if no usable `a3s-box`/virtualization is present.

use a3s_box_sdk::pipeline::{sweep_orphans, warm_base, FileCache, Step, WarmBase};
use std::process::Command;

fn a3s_box() -> String {
    std::env::var("A3S_BOX").unwrap_or_else(|_| "a3s-box".into())
}

fn image() -> String {
    std::env::var("A3S_SDK_TEST_IMAGE")
        .unwrap_or_else(|_| "docker.m.daocloud.io/library/alpine:latest".into())
}

/// Run `a3s-box <args>`, returning (success, stdout+stderr).
fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(a3s_box())
        .args(args)
        .output()
        .expect("spawn a3s-box");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// True only when a working `a3s-box` reports virtualization is available.
fn kvm_ready() -> bool {
    match Command::new(a3s_box()).arg("info").output() {
        Ok(o) => o.status.success() && String::from_utf8_lossy(&o.stdout).contains("available"),
        Err(_) => false,
    }
}

/// Resource names embed this process's pid; scope leak counts to `-<pid>-` so a
/// concurrent pipeline on the shared host can't perturb the assertions.
fn marker() -> String {
    format!("-{}-", std::process::id())
}

fn ci_base_boxes() -> usize {
    let m = marker();
    run(&["ps", "-a", "--format", "{{.Names}}"])
        .1
        .lines()
        .filter(|l| l.trim().starts_with("ci-base-") && l.contains(&m))
        .count()
}

fn ci_base_snaps() -> usize {
    let m = marker();
    run(&["snapshot", "ls"])
        .1
        .lines()
        .filter(|l| l.contains("ci-base-") && l.contains(&m))
        .count()
}

#[test]
#[ignore = "needs a real a3s-box + /dev/kvm"]
fn warm_fork_exec_and_cache() {
    if !kvm_ready() {
        eprintln!("SKIP warm_fork_exec_and_cache: no A3S_BOX/KVM");
        return;
    }
    let cdir = std::env::temp_dir().join(format!("a3s-sdk-it-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cdir);
    let cache = FileCache::new(&cdir).unwrap();

    let base = warm_base(WarmBase::new(image(), "echo DEPS-INSTALLED > /warmed").cache(&cache))
        .expect("warm_base");
    let r = base
        .step(Step::new("read", "cat /warmed"))
        .expect("step read");
    assert_eq!(r.exit_code, 0);
    assert!(r.stdout.contains("DEPS-INSTALLED"), "stdout={:?}", r.stdout);
    assert!(!r.cached);
    let r2 = base
        .step(Step::new("read", "cat /warmed"))
        .expect("step read#2");
    assert!(r2.cached, "an identical step must hit the cache");
    base.dispose();
    let _ = std::fs::remove_dir_all(&cdir);
}

#[test]
#[ignore = "needs a real a3s-box + /dev/kvm"]
fn parallel_collect_all_ordered_with_metrics() {
    if !kvm_ready() {
        eprintln!("SKIP parallel_collect_all_ordered_with_metrics: no A3S_BOX/KVM");
        return;
    }
    let base = warm_base(WarmBase::new(image(), "true")).expect("warm_base");
    let rep = base.run_parallel(
        vec![
            Step::new("a", "echo first"),
            Step::new(
                "perf",
                "echo '::metric duration_ms=12.5'; echo '::metric tests=7'",
            ),
            Step::new("b", "exit 3"),
            Step::new("c", "echo third"),
        ],
        4,
    );
    assert_eq!(rep.steps.len(), 4);
    assert_eq!(rep.steps[0].name, "a"); // input order preserved despite concurrency
    assert_eq!(rep.steps[1].name, "perf");
    assert_eq!(rep.steps[1].metrics.get("duration_ms"), Some(&12.5));
    assert_eq!(rep.steps[1].metrics.get("tests"), Some(&7.0));
    assert_eq!(rep.steps[2].exit_code, 3);
    assert!(!rep.passed);
    assert_eq!(rep.failures().len(), 1);
    assert!(rep.steps[0].duration_ms < 120_000);
    assert!(rep.to_json().contains("\"duration_ms\":12.5"));
    base.dispose();
}

#[test]
#[ignore = "needs a real a3s-box + /dev/kvm"]
fn forks_are_isolated() {
    if !kvm_ready() {
        eprintln!("SKIP forks_are_isolated: no A3S_BOX/KVM");
        return;
    }
    let base = warm_base(WarmBase::new(image(), "true")).expect("warm_base");
    // Each step is a fresh CoW fork of the base; a file written in one fork must
    // not be visible to a sibling.
    let rep = base.run_parallel(
        vec![
            Step::new("writer", "echo HELLO > /marker; cat /marker"),
            Step::new("reader", "cat /marker 2>/dev/null || echo MISSING"),
        ],
        2,
    );
    assert!(rep.steps[0].stdout.contains("HELLO"));
    assert!(
        rep.steps[1].stdout.contains("MISSING"),
        "sibling fork leaked state: {:?}",
        rep.steps[1].stdout
    );
    base.dispose();
}

#[test]
#[ignore = "needs a real a3s-box + /dev/kvm"]
fn leak_free_under_churn() {
    if !kvm_ready() {
        eprintln!("SKIP leak_free_under_churn: no A3S_BOX/KVM");
        return;
    }
    let boxes0 = ci_base_boxes();
    let snaps0 = ci_base_snaps();
    let base = warm_base(WarmBase::new(image(), "true")).expect("warm_base");
    for gen in 0..3 {
        let steps: Vec<Step> = (0..6)
            .map(|i| Step::new(format!("g{gen}s{i}"), "true"))
            .collect();
        let rep = base.run_parallel(steps, 4);
        assert!(rep.passed, "gen {gen} unexpectedly failed");
        let m = marker();
        let lingering = run(&["ps", "-a", "--format", "{{.Names}}"])
            .1
            .lines()
            .filter(|l| l.contains("-snap-job") && l.contains(&m))
            .count();
        assert_eq!(lingering, 0, "leaked fork boxes mid-churn at gen {gen}");
    }
    base.dispose();
    assert_eq!(ci_base_boxes(), boxes0, "leaked ci-base boxes after run");
    assert_eq!(
        ci_base_snaps(),
        snaps0,
        "leaked ci-base snapshots after dispose"
    );
}

#[test]
#[ignore = "needs a real a3s-box + /dev/kvm"]
fn sweep_reclaims_dead_pid_orphan_but_spares_live() {
    if !kvm_ready() {
        eprintln!("SKIP sweep_reclaims_dead_pid_orphan_but_spares_live: no A3S_BOX/KVM");
        return;
    }
    // A live base (owned by THIS pid) must survive the sweep.
    let base = warm_base(WarmBase::new(image(), "true")).expect("warm_base");

    // Forge a running box named as if owned by a reaped (dead) pid.
    let mut child = Command::new("true").spawn().expect("spawn true");
    let dead = child.id();
    child.wait().ok(); // reap -> /proc/<dead> goes away -> confirmed dead
    let orphan = format!("ci-base-deadbeef-{dead}-0-snap-job1-x");
    let (ok, out) = run(&[
        "run",
        "-d",
        "--name",
        &orphan,
        &image(),
        "--",
        "sleep",
        "300",
    ]);
    assert!(ok, "could not create forged orphan box: {out}");

    let removed = sweep_orphans();
    assert!(
        removed.iter().any(|n| n == &orphan),
        "sweep did not reclaim the dead-pid orphan; removed={removed:?}"
    );
    let names = run(&["ps", "-a", "--format", "{{.Names}}"]).1;
    assert!(
        !names.lines().any(|l| l.trim() == orphan),
        "orphan box still present after sweep"
    );
    // The live base was spared — a step on it still works.
    assert!(
        base.step(Step::new("alive", "true")).is_ok(),
        "sweep wrongly reclaimed a live base"
    );
    run(&["rm", "-f", &orphan]); // belt-and-suspenders
    base.dispose();
}
