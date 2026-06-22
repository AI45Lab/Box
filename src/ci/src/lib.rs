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
    for _ in 0..60 {
        let ready = Command::new(box_bin())
            .args(["exec", box_name, "--", "true"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ready {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
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
    for line in text.lines() {
        let mut cols = line.split_whitespace();
        if let (Some(id), Some(nm)) = (cols.next(), cols.next()) {
            if nm == name {
                return Ok(id.to_string());
            }
        }
    }
    Err(CiError::Cli {
        args: format!("snapshot ls (locate '{name}')"),
        code: None,
        stderr: "snapshot not found after create".into(),
    })
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
}
