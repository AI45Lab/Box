//! Programmable CI on a3s-box. A pipeline is a Rust program; a3s-box is the
//! execution backend — one Linux kernel per step, exit code = pass/fail.
//!
//! A thin, dependency-free wrapper over the `a3s-box` CLI (it owns the box
//! lifecycle + state, so we drive it rather than re-implementing that). It hides
//! the CLI footguns verified on a real KVM host: `run`/`exec` need `--` before
//! the command; `snapshot restore` yields a *created* box that must be `start`ed
//! before exec; `snapshot rm` keys on snapshot ID, not name.
//!
//! Model: warm a base box once (clone + install deps), `snapshot` it, then fork
//! the snapshot per step. With a3s-box's copy-on-write restore (overlay lower)
//! each fork shares the snapshot's pristine rootfs read-only and writes to its
//! own upper — near-instant, space-cheap, isolated. The DAG is your code:
//! sequence with plain calls, fan out with threads. No YAML, no engine.
//!
//! Set `A3S_BOX` to the CLI path if `a3s-box` is not on `PATH`.
//!
//! ```no_run
//! use a3s_box_ci::{warm_base, WarmBase, FileCache, Step};
//!
//! # fn main() -> Result<(), a3s_box_ci::CiError> {
//! let cache = FileCache::new(".ci-cache")?;
//! let mut base = warm_base(
//!     WarmBase::new("node:20", "git clone $REPO /w && cd /w && npm ci")
//!         .env("REPO", "https://github.com/me/app")
//!         .cache(&cache),
//! )?;
//! base.step(Step::new("lint", "cd /w && npm run lint"))?;
//! base.step(Step::new("test", "cd /w && npm test"))?; // nonzero exit -> Err (fail-fast)
//! base.step(Step::new("build", "cd /w && npm run build"))?;
//! base.dispose();
//! # Ok(()) }
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Output};

fn box_bin() -> String {
    std::env::var("A3S_BOX").unwrap_or_else(|_| "a3s-box".to_string())
}

/// Errors from driving the `a3s-box` CLI.
#[derive(Debug)]
pub enum CiError {
    /// Spawning `a3s-box` or a filesystem op failed.
    Io(std::io::Error),
    /// A box lifecycle command (run/exec/snapshot/start) exited non-zero.
    Cli {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    /// A pipeline step exited non-zero and `allow_failure` was not set.
    StepFailed {
        name: String,
        code: i32,
        logs: String,
    },
    /// A box never became exec-ready within the budget.
    NotReady(String),
}

impl fmt::Display for CiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CiError::Io(e) => write!(f, "a3s-box invocation failed: {e}"),
            CiError::Cli { args, code, stderr } => {
                write!(
                    f,
                    "`a3s-box {args}` failed (exit {code:?}): {}",
                    stderr.trim()
                )
            }
            CiError::StepFailed { name, code, logs } => {
                write!(f, "step '{name}' failed (exit {code})\n{}", logs.trim())
            }
            CiError::NotReady(b) => write!(f, "box '{b}' never became ready"),
        }
    }
}

impl std::error::Error for CiError {}

impl From<std::io::Error> for CiError {
    fn from(e: std::io::Error) -> Self {
        CiError::Io(e)
    }
}

type Result<T> = std::result::Result<T, CiError>;

/// Run an `a3s-box` subcommand, capturing output. `Ok` only on a zero exit.
fn box_run(args: &[&str]) -> Result<Output> {
    let out = Command::new(box_bin()).args(args).output()?;
    if !out.status.success() {
        return Err(CiError::Cli {
            args: args.join(" "),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(out)
}

/// Best-effort, silent cleanup — `rm -f` / `snapshot rm` of a missing box/snap is fine.
fn box_cleanup(args: &[&str]) {
    let _ = Command::new(box_bin()).args(args).output();
}

/// Content-addressed cache key: FNV-1a over NUL-delimited parts (stable, dep-free).
fn key(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0u8)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// Prefix shell `export`s so env doesn't depend on an `exec -e` flag existing.
fn with_env(cmd: &str, env: &BTreeMap<String, String>) -> String {
    if env.is_empty() {
        return cmd.to_string();
    }
    let mut s = String::new();
    for (k, v) in env {
        // single-quote escape: ' -> '\''
        s.push_str("export ");
        s.push_str(k);
        s.push_str("='");
        s.push_str(&v.replace('\'', "'\\''"));
        s.push_str("'; ");
    }
    s.push_str(cmd);
    s
}

/// Box-name-safe slug.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(40)
        .collect()
}

/// Poll `exec <box> -- true` until the box is ready (box has no "wait-ready" verb).
fn wait_ready(box_name: &str) -> Result<()> {
    wait_ready_inner(box_name, 60, 500)
}

/// Polling core, parameterized so tests can drive the timeout path without waiting.
fn wait_ready_inner(box_name: &str, tries: u32, delay_ms: u64) -> Result<()> {
    for _ in 0..tries {
        let ready = Command::new(box_bin())
            .args(["exec", box_name, "--", "true"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ready {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    Err(CiError::NotReady(box_name.to_string()))
}

/// Content-addressed step cache: one marker file per successful step key.
pub struct FileCache {
    dir: PathBuf,
}

impl FileCache {
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }
    fn has(&self, k: &str) -> bool {
        self.dir.join(k).exists()
    }
    fn put(&self, k: &str) {
        // Recreate the dir if it vanished after construction, so a marker is
        // never silently dropped (a dropped marker = a missed cache hit).
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::File::create(self.dir.join(k));
    }
}

/// Spec for warming a base box (clone + install deps once).
pub struct WarmBase<'a> {
    image: String,
    setup: String,
    env: BTreeMap<String, String>,
    cache: Option<&'a FileCache>,
}

impl<'a> WarmBase<'a> {
    pub fn new(image: impl Into<String>, setup: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            setup: setup.into(),
            env: BTreeMap::new(),
            cache: None,
        }
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    pub fn cache(mut self, cache: &'a FileCache) -> Self {
        self.cache = Some(cache);
        self
    }
}

/// A pipeline step: a command run in its own kernel forked from the base.
pub struct Step {
    name: String,
    cmd: String,
    inputs: Vec<String>,
    env: BTreeMap<String, String>,
    allow_failure: bool,
}

impl Step {
    pub fn new(name: impl Into<String>, cmd: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            cmd: cmd.into(),
            inputs: Vec::new(),
            env: BTreeMap::new(),
            allow_failure: false,
        }
    }
    /// Extra cache-key inputs: the step is skipped when name+cmd+inputs are unchanged.
    pub fn input(mut self, s: impl Into<String>) -> Self {
        self.inputs.push(s.into());
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
    /// Do not fail the pipeline if this step exits non-zero.
    pub fn allow_failure(mut self) -> Self {
        self.allow_failure = true;
        self
    }
}

/// Outcome of a step.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub name: String,
    pub exit_code: i32,
    pub logs: String,
    pub cached: bool,
}

/// A warmed, forkable base — a snapshot plus an optional step cache.
pub struct Base<'a> {
    snapshot_name: String,
    snapshot_id: String,
    cache: Option<&'a FileCache>,
    n: u32,
}

impl Base<'_> {
    /// Run one step in its own kernel forked from the warmed base. A non-zero exit
    /// returns `Err(StepFailed)` (fail-fast) unless `Step::allow_failure` was set.
    pub fn step(&mut self, step: Step) -> Result<StepResult> {
        let mut parts: Vec<&str> = vec![&step.name, &step.cmd];
        parts.extend(step.inputs.iter().map(|s| s.as_str()));
        let k = key(&parts);
        if let Some(c) = self.cache {
            if c.has(&k) {
                return Ok(StepResult {
                    name: step.name,
                    exit_code: 0,
                    logs: String::new(),
                    cached: true,
                });
            }
        }

        self.n += 1;
        let box_name = format!("{}-job{}-{}", self.snapshot_name, self.n, slug(&step.name));
        box_cleanup(&["rm", "-f", &box_name]); // idempotent: clear a crashed prior run

        // Fork = restore + start. Since a3s-box's CoW restore, this mounts the
        // snapshot's rootfs as a read-only overlay lower with a fresh per-box upper.
        box_run(&[
            "snapshot",
            "restore",
            &self.snapshot_name,
            "--name",
            &box_name,
        ])?;
        box_run(&["start", &box_name])?;
        wait_ready(&box_name)?;

        let full = with_env(&step.cmd, &step.env);
        let out = Command::new(box_bin())
            .args(["exec", &box_name, "--", "sh", "-c", &full])
            .output()?;
        box_cleanup(&["rm", "-f", &box_name]);

        let code = out.status.code().unwrap_or(-1);
        let mut logs = String::from_utf8_lossy(&out.stdout).into_owned();
        logs.push_str(&String::from_utf8_lossy(&out.stderr));

        if code == 0 {
            if let Some(c) = self.cache {
                c.put(&k);
            }
        }
        if code != 0 && !step.allow_failure {
            return Err(CiError::StepFailed {
                name: step.name,
                code,
                logs,
            });
        }
        Ok(StepResult {
            name: step.name,
            exit_code: code,
            logs,
            cached: false,
        })
    }

    /// Remove the snapshot (by ID — `snapshot rm` does not accept names).
    pub fn dispose(&self) {
        box_cleanup(&["snapshot", "rm", &self.snapshot_id]);
    }
}

/// Resolve a snapshot's ID by name from `snapshot ls` (first whitespace column).
fn snapshot_id(name: &str) -> Result<String> {
    let out = box_run(&["snapshot", "ls"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_snapshot_id(&text, name).ok_or_else(|| CiError::Cli {
        args: format!("snapshot ls (locate '{name}')"),
        code: None,
        stderr: "snapshot not found after create".into(),
    })
}

/// Find a snapshot's ID (first column) by name (second column) in `snapshot ls` output.
fn parse_snapshot_id(ls_output: &str, name: &str) -> Option<String> {
    for line in ls_output.lines() {
        let mut cols = line.split_whitespace();
        if let (Some(id), Some(nm)) = (cols.next(), cols.next()) {
            if nm == name {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Warm a base box: run `setup` once, snapshot the result, return a forkable [`Base`].
pub fn warm_base(spec: WarmBase<'_>) -> Result<Base<'_>> {
    let base_box = format!("ci-base-{}", key(&[&spec.image, &spec.setup]));
    let snap = format!("{base_box}-snap");

    box_cleanup(&["rm", "-f", &base_box]); // idempotent rerun
    if let Ok(old) = snapshot_id(&snap) {
        box_cleanup(&["snapshot", "rm", &old]);
    }

    box_run(&[
        "run",
        "-d",
        "--name",
        &base_box,
        &spec.image,
        "--",
        "sleep",
        "86400",
    ])?;
    let prepared = (|| -> Result<String> {
        wait_ready(&base_box)?;
        let setup = with_env(&spec.setup, &spec.env);
        box_run(&["exec", &base_box, "--", "sh", "-c", &setup])?; // clone + deps ONCE
        box_run(&["snapshot", "create", &base_box, "--name", &snap])?;
        snapshot_id(&snap)
    })();
    box_cleanup(&["rm", "-f", &base_box]);

    Ok(Base {
        snapshot_name: snap,
        snapshot_id: prepared?,
        cache: spec.cache,
        n: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_deterministic_and_order_sensitive() {
        assert_eq!(key(&["a", "b"]), key(&["a", "b"]));
        assert_ne!(key(&["a", "b"]), key(&["b", "a"]));
        // NUL-delimited: ("ab","c") must not collide with ("a","bc").
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
    }

    #[test]
    fn with_env_escapes_and_prefixes() {
        let mut e = BTreeMap::new();
        assert_eq!(with_env("run", &e), "run");
        e.insert("A".to_string(), "x y".to_string());
        assert_eq!(with_env("run", &e), "export A='x y'; run");
        let mut q = BTreeMap::new();
        q.insert("Q".to_string(), "a'b".to_string());
        assert_eq!(with_env("run", &q), "export Q='a'\\''b'; run");
    }

    #[test]
    fn slug_sanitizes() {
        assert_eq!(slug("a/b c"), "a-b-c");
    }

    #[test]
    fn file_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("a3s-ci-test-{}", key(&["cache"])));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = FileCache::new(&dir).unwrap();
        let k = key(&["x"]);
        assert!(!cache.has(&k));
        cache.put(&k);
        assert!(cache.has(&k));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_cache_put_recreates_missing_dir() {
        // If the cache dir vanishes after construction, put must not silently drop
        // the marker (regression: a dropped marker is a missed cache hit).
        let dir = std::env::temp_dir().join(format!("a3s-ci-test-{}", key(&["recreate"])));
        let cache = FileCache::new(&dir).unwrap();
        let k = key(&["y"]);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!cache.has(&k));
        cache.put(&k);
        assert!(
            cache.has(&k),
            "put must recreate the dir and write the marker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_snapshot_id_finds_by_name() {
        let ls = "SNAPSHOT ID                    NAME                 SOURCE BOX\n\
                  snap-111                       ci-base-aaa-snap     box-1\n\
                  snap-222                       ci-base-bbb-snap     box-2\n";
        assert_eq!(
            parse_snapshot_id(ls, "ci-base-bbb-snap").as_deref(),
            Some("snap-222")
        );
        assert_eq!(parse_snapshot_id(ls, "nope"), None);
        assert_eq!(parse_snapshot_id("", "x"), None);
        // lines with fewer than two columns are skipped, not panicked on
        assert_eq!(
            parse_snapshot_id("lonely\n\nsnap-9 found x\n", "found").as_deref(),
            Some("snap-9")
        );
    }

    #[test]
    fn ci_error_display_covers_each_variant() {
        let io = CiError::from(std::io::Error::other("boom"));
        assert!(format!("{io}").contains("boom"));
        let cli = CiError::Cli {
            args: "snapshot rm s".into(),
            code: Some(1),
            stderr: "still used".into(),
        };
        assert!(format!("{cli}").contains("still used"));
        let step = CiError::StepFailed {
            name: "test".into(),
            code: 7,
            logs: "oops".into(),
        };
        let s = format!("{step}");
        assert!(s.contains("test") && s.contains("exit 7"));
        assert!(format!("{}", CiError::NotReady("b1".into())).contains("b1"));
    }

    // A POSIX-sh stub standing in for `a3s-box`: enough to drive warm_base + step
    // through their happy paths (run/exec/start/rm succeed, `snapshot create`
    // records name->id in a state file that `snapshot ls` echoes, an exec whose
    // command contains "boom" exits 7). Lets us cover the orchestration locally,
    // without a real box or KVM.
    #[cfg(unix)]
    const FAKE_BOX: &str = r#"#!/bin/sh
state="$(dirname "$0")/.snaps"
case "$1" in
  run|start|rm) exit 0 ;;
  exec) last=""; for a in "$@"; do last="$a"; done
        case "$last" in *boom*) exit 7 ;; *) exit 0 ;; esac ;;
  snapshot)
    case "$2" in
      create) name=""; while [ "$#" -gt 0 ]; do [ "$1" = "--name" ] && name="$2"; shift; done
              printf '%s %s\n' "snap-fake1" "$name" >> "$state"; printf 'snap-fake1\n'; exit 0 ;;
      ls) printf 'ID NAME SRC\n'; cat "$state" 2>/dev/null; exit 0 ;;
      *) exit 0 ;;
    esac ;;
  *) exit 0 ;;
esac
"#;

    #[cfg(unix)]
    #[test]
    fn orchestration_drives_the_cli() {
        use std::os::unix::fs::PermissionsExt;
        // The ONLY test that touches the A3S_BOX env, so it cannot race a concurrent
        // box_bin() reader.
        let dir = std::env::temp_dir().join(format!("a3s-ci-fakebox-{}", key(&["fakebox"])));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("a3s-box");
        std::fs::write(&fake, FAKE_BOX).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Happy path against the stub.
        std::env::set_var("A3S_BOX", &fake);
        let cache = FileCache::new(dir.join("cache")).unwrap();
        let mut base = warm_base(WarmBase::new("img", "echo hi").cache(&cache)).expect("warm_base");
        let r = base.step(Step::new("build", "make")).expect("step");
        assert_eq!(r.exit_code, 0);
        assert!(!r.cached);
        assert!(base.step(Step::new("build", "make")).expect("step2").cached); // cache hit
        assert!(matches!(
            base.step(Step::new("fail", "boom")),
            Err(CiError::StepFailed { code: 7, .. })
        ));
        // box_run surfaces a non-zero exit as Cli (the stub exits 7 on a "boom" exec).
        assert!(matches!(
            box_run(&["exec", "x", "--", "boom"]),
            Err(CiError::Cli { code: Some(7), .. })
        ));
        // allow_failure: a non-zero step returns Ok with the code instead of Err.
        let allowed = base
            .step(Step::new("maybe", "boom").allow_failure())
            .expect("allow_failure step");
        assert_eq!(allowed.exit_code, 7);
        assert!(!allowed.cached);
        // A second warm_base exercises the stale-snapshot pre-clean path.
        let _ = warm_base(WarmBase::new("img", "echo hi").cache(&cache)).expect("warm_base #2");
        base.dispose();

        // A cache-less base exercises the no-cache branch in step().
        let mut nocache = warm_base(WarmBase::new("img", "echo hi")).expect("warm_base nocache");
        assert!(
            !nocache
                .step(Step::new("x", "make"))
                .expect("nocache step")
                .cached
        );
        nocache.dispose();

        // Failure path: a missing binary must surface as Err, never panic.
        std::env::set_var("A3S_BOX", "/nonexistent/a3s-box-xyzzy");
        assert!(matches!(box_run(&["version"]), Err(CiError::Io(_))));
        box_cleanup(&["rm", "-f", "x"]); // best-effort: must not panic
        assert!(snapshot_id("any").is_err());
        assert!(warm_base(WarmBase::new("img", "echo hi")).is_err());
        // wait_ready returns NotReady when the box never becomes ready (fast: 2 tries, 0ms).
        assert!(matches!(
            wait_ready_inner("b", 2, 0),
            Err(CiError::NotReady(_))
        ));

        std::env::remove_var("A3S_BOX");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builders_chain_all_options() {
        // Exercise every builder method (fields are private; chaining covers the bodies).
        let c = FileCache::new(std::env::temp_dir().join(format!("a3s-ci-bld-{}", key(&["bld"]))))
            .unwrap();
        let _ = WarmBase::new("img", "setup").env("A", "1").cache(&c);
        let _ = Step::new("n", "cmd")
            .input("x")
            .env("K", "V")
            .allow_failure();
    }
}
