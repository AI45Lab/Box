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
//! own upper — near-instant, space-cheap, isolated. Steps are *independent*
//! forks of the same base, so they run sequentially with [`Base::step`]
//! (fail-fast) or concurrently with [`Base::run_parallel`] (collect-all), which
//! returns a typed, JSON-serializable [`Report`]. The base auto-disposes its
//! snapshot on drop; per-step boxes are removed on every path (incl. panic).
//!
//! A step reports metrics by printing `::metric <key>=<number>` lines to stdout;
//! they surface as [`StepResult::metrics`] for scoring (e.g. an evolution loop).
//!
//! Set `A3S_BOX` to the CLI path if `a3s-box` is not on `PATH`.
//!
//! ```no_run
//! use a3s_box_sdk::pipeline::{warm_base, WarmBase, FileCache, Step};
//!
//! # fn main() -> Result<(), a3s_box_sdk::pipeline::PipelineError> {
//! let cache = FileCache::new(".ci-cache")?;
//! let base = warm_base(
//!     WarmBase::new("node:20", "git clone $REPO /w && cd /w && npm ci")
//!         .env("REPO", "https://github.com/me/app")
//!         .cache(&cache),
//! )?;
//! // Fan out in parallel (each step is an isolated CoW fork of the base),
//! // bounded to 4 at a time; collect a typed report.
//! let report = base.run_parallel(
//!     vec![
//!         Step::new("lint", "cd /w && npm run lint"),
//!         Step::new("test", "cd /w && npm test"),
//!         Step::new("build", "cd /w && npm run build"),
//!     ],
//!     4,
//! );
//! println!("{}", report.to_json()); // {"passed":true,"total_ms":...,"steps":[...]}
//! assert!(report.passed);
//! # Ok(()) } // base drops here -> snapshot auto-removed
//! ```

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default per-step cap on captured stdout/stderr — bounds in-memory [`Report`]
/// size when fanning hundreds of forks out. Override via [`WarmBase::max_output`].
const DEFAULT_MAX_OUTPUT: usize = 1 << 20; // 1 MiB

/// Default number of times to retry a fork that hits an *infrastructure* failure
/// (restore/start/boot/exec-spawn). The step's command never ran, so re-forking is
/// safe and idempotent; this absorbs the transient boot hiccups that surface under
/// sustained, highly-concurrent churn. Override via [`WarmBase::infra_retries`].
const DEFAULT_INFRA_RETRIES: usize = 2;

/// `exec_step` returns this exit code when the step never ran because of an
/// *infrastructure* failure (restore/start/exec), distinct from any real exit
/// code (a host process killed by a signal surfaces as -1, a guest `exit -1` as
/// 255), so a scorer can tell "never ran" from "ran and failed".
pub const INFRA_FAILURE: i32 = i32::MIN;

/// Per-process instance seed. Folding it (plus the pid) into box/snapshot NAMES
/// gives two Bases from the same image+setup — or two processes on one host —
/// DISJOINT names. The CLI enforces name uniqueness, and a shared name would let
/// one Base's cleanup tear down another's live box (cross-talk → false CI).
static INSTANCE_SEQ: AtomicU32 = AtomicU32::new(0);

fn instance_token() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn box_bin() -> String {
    std::env::var("A3S_BOX").unwrap_or_else(|_| "a3s-box".to_string())
}

/// Errors from driving the `a3s-box` CLI.
#[derive(Debug)]
pub enum PipelineError {
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

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Io(e) => write!(f, "a3s-box invocation failed: {e}"),
            PipelineError::Cli { args, code, stderr } => {
                write!(
                    f,
                    "`a3s-box {args}` failed (exit {code:?}): {}",
                    stderr.trim()
                )
            }
            PipelineError::StepFailed { name, code, logs } => {
                write!(f, "step '{name}' failed (exit {code})\n{}", logs.trim())
            }
            PipelineError::NotReady(b) => write!(f, "box '{b}' never became ready"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<std::io::Error> for PipelineError {
    fn from(e: std::io::Error) -> Self {
        PipelineError::Io(e)
    }
}

type Result<T> = std::result::Result<T, PipelineError>;

/// Run an `a3s-box` subcommand, capturing output. `Ok` only on a zero exit.
fn box_run(args: &[&str]) -> Result<Output> {
    let out = Command::new(box_bin()).args(args).output()?;
    if !out.status.success() {
        return Err(PipelineError::Cli {
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

/// RAII: removes a per-step box on every exit path (normal return, `?`, panic),
/// so a step that fails after `start`/before exec never leaks a box.
struct BoxGuard(String);
impl Drop for BoxGuard {
    fn drop(&mut self) {
        box_cleanup(&["rm", "-f", &self.0]);
    }
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

/// Truncate output to `cap` bytes on a UTF-8 boundary, appending a marker, so one
/// chatty/looping step can't balloon an in-memory [`Report`] during large fan-out.
fn cap_output(mut s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    s.truncate(end);
    s.push_str(&format!("\n[..truncated {dropped} bytes]"));
    s
}

/// Parse `::metric <key>=<number>` lines from a step's stdout into a metric map.
/// Non-numeric and non-finite (NaN/inf) values are ignored; this is the scoring
/// channel for callers (e.g. an evolution loop that selects on `perf_ms`/`tests`).
fn parse_metrics(stdout: &str) -> BTreeMap<String, f64> {
    let mut m = BTreeMap::new();
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("::metric ") {
            if let Some((k, v)) = rest.split_once('=') {
                if let Ok(val) = v.trim().parse::<f64>() {
                    if val.is_finite() {
                        m.insert(k.trim().to_string(), val);
                    }
                }
            }
        }
    }
    m
}

/// Minimal, correct JSON string encoder (dep-free): quotes + escapes.
fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A finite JSON number, or `null` for NaN/±inf (which are not valid JSON).
fn json_num(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".to_string()
    }
}

/// Poll `exec <box> -- true` until the box is ready (box has no "wait-ready" verb).
fn wait_ready(box_name: &str) -> Result<()> {
    // ~30s total budget. A box is usually ready in ~100-300ms, so poll fast first
    // and back off — far lower per-step latency than a fixed long sleep.
    wait_ready_inner(box_name, std::time::Duration::from_secs(30))
}

/// Polling core: exponential backoff (25ms → … → 500ms cap) bounded by `budget`.
/// Parameterized so tests can drive the timeout path with a tiny budget.
fn wait_ready_inner(box_name: &str, budget: std::time::Duration) -> Result<()> {
    let start = std::time::Instant::now();
    let mut delay = std::time::Duration::from_millis(25);
    let cap = std::time::Duration::from_millis(500);
    loop {
        let ready = Command::new(box_bin())
            .args(["exec", box_name, "--", "true"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ready {
            return Ok(());
        }
        if start.elapsed() >= budget {
            return Err(PipelineError::NotReady(box_name.to_string()));
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(cap);
    }
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
    max_output: usize,
    infra_retries: usize,
}

impl<'a> WarmBase<'a> {
    pub fn new(image: impl Into<String>, setup: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            setup: setup.into(),
            env: BTreeMap::new(),
            cache: None,
            max_output: DEFAULT_MAX_OUTPUT,
            infra_retries: DEFAULT_INFRA_RETRIES,
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
    /// Cap captured stdout/stderr per step (default 1 MiB) to bound report memory.
    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output = bytes;
        self
    }
    /// Retries for a fork that hits an infrastructure failure (default 2). The
    /// step's command never ran on such a failure, so re-forking is idempotent.
    pub fn infra_retries(mut self, n: usize) -> Self {
        self.infra_retries = n;
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

/// Outcome of a step. `stdout`/`stderr` are separated (and capped, see
/// [`WarmBase::max_output`]); `metrics` are parsed from `::metric k=v` stdout
/// lines; `duration_ms` is the command's wall-clock time. An infrastructure
/// failure surfaces as `exit_code == `[`INFRA_FAILURE`].
#[derive(Debug, Clone)]
pub struct StepResult {
    pub name: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub cached: bool,
    pub allow_failure: bool,
    pub duration_ms: u128,
    pub metrics: BTreeMap<String, f64>,
}

impl StepResult {
    /// A passing step: exit 0, or a non-zero step that opted into `allow_failure`.
    pub fn ok(&self) -> bool {
        self.exit_code == 0 || self.allow_failure
    }
    /// stdout followed by stderr (the legacy combined log view).
    pub fn combined(&self) -> String {
        let mut s = self.stdout.clone();
        s.push_str(&self.stderr);
        s
    }
    /// Serialize to a JSON object (dep-free).
    pub fn to_json(&self) -> String {
        let mut metrics = String::from("{");
        for (i, (k, v)) in self.metrics.iter().enumerate() {
            if i > 0 {
                metrics.push(',');
            }
            metrics.push_str(&json_str(k));
            metrics.push(':');
            metrics.push_str(&json_num(*v));
        }
        metrics.push('}');
        format!(
            "{{\"name\":{},\"exit_code\":{},\"cached\":{},\"allow_failure\":{},\"duration_ms\":{},\"metrics\":{},\"stdout\":{},\"stderr\":{}}}",
            json_str(&self.name),
            self.exit_code,
            self.cached,
            self.allow_failure,
            self.duration_ms,
            metrics,
            json_str(&self.stdout),
            json_str(&self.stderr),
        )
    }
}

/// Aggregate outcome of a [`Base::run_parallel`] batch: per-step results in input
/// order, total wall-clock, and a `passed` gate (all steps [`StepResult::ok`]).
#[derive(Debug, Clone)]
pub struct Report {
    pub steps: Vec<StepResult>,
    pub total_ms: u128,
    pub passed: bool,
}

impl Report {
    /// The steps that did not pass (non-zero exit without `allow_failure`).
    pub fn failures(&self) -> Vec<&StepResult> {
        self.steps.iter().filter(|r| !r.ok()).collect()
    }
    /// Serialize the whole report to a JSON object (dep-free) for an agent/scorer.
    pub fn to_json(&self) -> String {
        let steps: Vec<String> = self.steps.iter().map(|s| s.to_json()).collect();
        format!(
            "{{\"passed\":{},\"total_ms\":{},\"steps\":[{}]}}",
            self.passed,
            self.total_ms,
            steps.join(",")
        )
    }
}

/// A warmed, forkable base — a snapshot plus an optional step cache. Methods take
/// `&self` (the fork counter is atomic), so steps can run concurrently across
/// threads. The snapshot is removed on drop (or eagerly via [`Base::dispose`]).
pub struct Base<'a> {
    snapshot_name: String,
    snapshot_id: String,
    cache: Option<&'a FileCache>,
    max_output: usize,
    infra_retries: usize,
    n: AtomicU32,
    disposed: AtomicBool,
}

impl Base<'_> {
    /// Fork the base, run one step in its own kernel, return its [`StepResult`].
    /// `Err` only on an infrastructure failure (restore/start/exec); a non-zero
    /// *step* exit is returned as a `StepResult` (caller decides what it means).
    fn exec_step(&self, step: &Step) -> Result<StepResult> {
        let mut parts: Vec<&str> = vec![&step.name, &step.cmd];
        parts.extend(step.inputs.iter().map(|s| s.as_str()));
        let k = key(&parts);
        if let Some(c) = self.cache {
            if c.has(&k) {
                return Ok(StepResult {
                    name: step.name.clone(),
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    cached: true,
                    allow_failure: step.allow_failure,
                    duration_ms: 0,
                    metrics: BTreeMap::new(),
                });
            }
        }

        // Names are globally unique (snapshot_name carries pid+instance entropy,
        // `i` is per-fork), so no cross-process pre-clean is needed; the guard
        // removes the fork on every exit path.
        let i = self.n.fetch_add(1, Ordering::Relaxed) + 1;
        let box_name = format!("{}-job{}-{}", self.snapshot_name, i, slug(&step.name));
        let _guard = BoxGuard(box_name.clone());

        // Fork = restore + start. Thanks to a3s-box's CoW restore, this mounts the
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
        let t = Instant::now();
        let out = Command::new(box_bin())
            .args(["exec", &box_name, "--", "sh", "-c", &full])
            .output()?;
        let duration_ms = t.elapsed().as_millis();

        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let metrics = parse_metrics(&stdout); // parse BEFORE truncating

        if code == 0 {
            if let Some(c) = self.cache {
                c.put(&k);
            }
        }
        Ok(StepResult {
            name: step.name.clone(),
            exit_code: code,
            stdout: cap_output(stdout, self.max_output),
            stderr: cap_output(stderr, self.max_output),
            cached: false,
            allow_failure: step.allow_failure,
            duration_ms,
            metrics,
        })
    }

    /// `exec_step` with bounded retry on *infrastructure* failure (the step's
    /// command never ran, so re-forking is idempotent). A non-zero step exit is
    /// returned as `Ok` and never retried — only a failed fork is.
    fn exec_step_retrying(&self, step: &Step) -> Result<StepResult> {
        let mut last: Option<PipelineError> = None;
        for attempt in 0..=self.infra_retries {
            match self.exec_step(step) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    last = Some(e);
                    if attempt < self.infra_retries {
                        std::thread::sleep(Duration::from_millis(150 * (attempt as u64 + 1)));
                    }
                }
            }
        }
        Err(last.expect("loop runs at least once"))
    }

    /// Run one step, **fail-fast**: a non-zero exit returns `Err(StepFailed)`
    /// unless `Step::allow_failure` was set. Use for a sequential pipeline.
    pub fn step(&self, step: Step) -> Result<StepResult> {
        let r = self.exec_step_retrying(&step)?;
        if r.exit_code != 0 && !step.allow_failure {
            return Err(PipelineError::StepFailed {
                name: r.name.clone(),
                code: r.exit_code,
                logs: r.combined(),
            });
        }
        Ok(r)
    }

    /// Run one step for a batch: **never** propagates — an infra failure becomes a
    /// `StepResult{ exit_code: `[`INFRA_FAILURE`]`, stderr: <error> }` so one bad
    /// fork can't abort the batch.
    fn run_one(&self, step: Step) -> StepResult {
        match self.exec_step_retrying(&step) {
            Ok(r) => r,
            Err(e) => StepResult {
                name: step.name.clone(),
                exit_code: INFRA_FAILURE,
                stdout: String::new(),
                stderr: e.to_string(),
                cached: false,
                allow_failure: step.allow_failure,
                duration_ms: 0,
                metrics: BTreeMap::new(),
            },
        }
    }

    /// Fan steps out concurrently — each is an isolated CoW fork of the base —
    /// bounded to `max_concurrency` at a time. **Collect-all** (not fail-fast):
    /// every step runs and is returned in input order. This is the primitive that
    /// makes a3s-box's cheap fork (~ms) usable for matrix / evolution-scale CI.
    /// Do not call [`Base::dispose`] while a `run_parallel` is in flight.
    pub fn run_parallel(&self, steps: Vec<Step>, max_concurrency: usize) -> Report {
        let start = Instant::now();
        let n = steps.len();
        if n == 0 {
            return Report {
                steps: Vec::new(),
                total_ms: 0,
                passed: true,
            };
        }
        let conc = max_concurrency.max(1).min(n);
        let queue: Mutex<VecDeque<(usize, Step)>> =
            Mutex::new(steps.into_iter().enumerate().collect());
        let results: Mutex<Vec<(usize, StepResult)>> = Mutex::new(Vec::with_capacity(n));

        std::thread::scope(|s| {
            for _ in 0..conc {
                s.spawn(|| loop {
                    let next = { queue.lock().unwrap().pop_front() };
                    let Some((idx, step)) = next else { break };
                    let r = self.run_one(step);
                    results.lock().unwrap().push((idx, r));
                });
            }
        });

        let mut v = results.into_inner().unwrap();
        v.sort_by_key(|(i, _)| *i);
        let steps: Vec<StepResult> = v.into_iter().map(|(_, r)| r).collect();
        let passed = steps.iter().all(|r| r.ok());
        Report {
            steps,
            total_ms: start.elapsed().as_millis(),
            passed,
        }
    }

    /// Eagerly remove the snapshot (by ID, `--force` since forks are CoW lowers).
    /// Idempotent with the `Drop` impl, so calling it early is safe.
    pub fn dispose(&self) {
        if !self.disposed.swap(true, Ordering::SeqCst) {
            box_cleanup(&["snapshot", "rm", "--force", &self.snapshot_id]);
        }
    }
}

impl Drop for Base<'_> {
    fn drop(&mut self) {
        if !self.disposed.swap(true, Ordering::SeqCst) {
            box_cleanup(&["snapshot", "rm", "--force", &self.snapshot_id]);
        }
    }
}

/// Resolve a snapshot's ID by name from `snapshot ls` (first whitespace column).
fn snapshot_id(name: &str) -> Result<String> {
    let out = box_run(&["snapshot", "ls"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_snapshot_id(&text, name).ok_or_else(|| PipelineError::Cli {
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
    // pid+instance entropy: two warms of the same spec get disjoint names, so they
    // can't tear down each other's box/snapshot. Hence no shared-name pre-clean.
    let base_box = format!(
        "ci-base-{}-{}",
        key(&[&spec.image, &spec.setup]),
        instance_token()
    );
    let snap = format!("{base_box}-snap");

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
    // If `snapshot create` succeeded but ID resolution failed, no Base is built,
    // so Drop never fires — best-effort remove the orphan snapshot.
    if prepared.is_err() {
        if let Ok(id) = snapshot_id(&snap) {
            box_cleanup(&["snapshot", "rm", "--force", &id]);
        }
    }

    Ok(Base {
        snapshot_name: snap,
        snapshot_id: prepared?,
        cache: spec.cache,
        max_output: spec.max_output,
        infra_retries: spec.infra_retries,
        n: AtomicU32::new(0),
        disposed: AtomicBool::new(false),
    })
}

/// The owner pid embedded in a `ci-base-<key>-<pid>-<seq>...` resource name.
fn parse_owner_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("ci-base-")?;
    let mut it = rest.split('-');
    it.next()?; // key hash
    it.next()?.parse().ok()
}

/// `Some(true/false)` where pid-liveness is knowable (Linux `/proc`); `None`
/// otherwise — so a sweep never reclaims resources whose owner it can't confirm dead.
fn pid_alive(pid: u32) -> Option<bool> {
    if !std::path::Path::new("/proc").is_dir() {
        return None;
    }
    Some(std::path::Path::new(&format!("/proc/{pid}")).exists())
}

/// If `name` is a ci-base resource owned by a confirmed-DEAD pid other than `me`,
/// return that pid (it is an orphan safe to reclaim).
fn orphan_pid(name: &str, me: u32) -> Option<u32> {
    let pid = parse_owner_pid(name)?;
    if pid == me {
        return None; // ours — never sweep a live self
    }
    match pid_alive(pid) {
        Some(false) => Some(pid), // confirmed dead
        _ => None,                // alive or unknowable — leave it
    }
}

/// Reclaim `ci-base-*` boxes and snapshots leaked by a **crashed** pipeline
/// process. `Drop`/guards clean up graceful exits, but `SIGKILL`/OOM/power-loss
/// skip them; resource names embed the owner pid, so a resource whose pid is no
/// longer alive (and isn't this process) is removed. Returns the names reclaimed.
/// Safe to call concurrently with live pipelines: it only touches dead-pid orphans.
pub fn sweep_orphans() -> Vec<String> {
    let me = std::process::id();
    let mut removed = Vec::new();

    if let Ok(out) = box_run(&["ps", "-a", "--format", "{{.Names}}"]) {
        for name in String::from_utf8_lossy(&out.stdout).lines() {
            let name = name.trim();
            if orphan_pid(name, me).is_some() {
                box_cleanup(&["rm", "-f", name]);
                removed.push(name.to_string());
            }
        }
    }
    if let Ok(out) = box_run(&["snapshot", "ls"]) {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let mut cols = line.split_whitespace();
            if let (Some(id), Some(name)) = (cols.next(), cols.next()) {
                if orphan_pid(name, me).is_some() {
                    box_cleanup(&["snapshot", "rm", "--force", id]);
                    removed.push(name.to_string());
                }
            }
        }
    }
    removed
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
    fn instance_token_is_unique_per_call() {
        assert_ne!(instance_token(), instance_token());
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
    fn cap_output_truncates_on_char_boundary() {
        assert_eq!(cap_output("hello".into(), 10), "hello"); // under cap: untouched
        let capped = cap_output("a".repeat(100), 10);
        assert!(capped.starts_with("aaaaaaaaaa"));
        assert!(capped.contains("[..truncated 90 bytes]"));
        // a multi-byte char ("é" = 2 bytes) must not be split mid-codepoint.
        let c = cap_output("é".repeat(10), 5); // floors to a 4-byte boundary
        assert!(std::str::from_utf8(c.as_bytes()).is_ok());
        assert!(c.contains("truncated"));
    }

    #[test]
    fn parse_metrics_extracts_finite_numeric_only() {
        let m = parse_metrics(
            "noise\n::metric perf_ms=84.2\n  ::metric tests=412 \n::metric bad=xyz\n\
             ::metric inf_m=inf\n::metric nan_m=NaN\ntrailing",
        );
        assert_eq!(m.get("perf_ms"), Some(&84.2));
        assert_eq!(m.get("tests"), Some(&412.0));
        assert!(!m.contains_key("bad")); // non-numeric ignored
        assert!(!m.contains_key("inf_m")); // non-finite ignored
        assert!(!m.contains_key("nan_m"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn json_str_escapes_quotes_backslash_control() {
        assert_eq!(json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(json_str("\u{0001}"), "\"\\u0001\"");
        assert_eq!(json_num(1.5), "1.5");
        assert_eq!(json_num(f64::NAN), "null");
    }

    #[test]
    fn report_and_step_json_shape() {
        let mut metrics = BTreeMap::new();
        metrics.insert("k".to_string(), 2.0);
        let rep = Report {
            passed: false,
            total_ms: 5,
            steps: vec![StepResult {
                name: "t".into(),
                exit_code: 1,
                stdout: "o".into(),
                stderr: "e\"x".into(),
                cached: false,
                allow_failure: false,
                duration_ms: 3,
                metrics,
            }],
        };
        let j = rep.to_json();
        assert!(j.contains("\"passed\":false"));
        assert!(j.contains("\"total_ms\":5"));
        assert!(j.contains("\"name\":\"t\""));
        assert!(j.contains("\"exit_code\":1"));
        assert!(j.contains("\"metrics\":{\"k\":2}"));
        assert!(j.contains("\"stderr\":\"e\\\"x\"")); // escaped quote in stderr
        assert_eq!(rep.failures().len(), 1);
    }

    #[test]
    fn step_result_ok_respects_allow_failure() {
        let mk = |code: i32, allow: bool| StepResult {
            name: "s".into(),
            exit_code: code,
            stdout: String::new(),
            stderr: String::new(),
            cached: false,
            allow_failure: allow,
            duration_ms: 0,
            metrics: BTreeMap::new(),
        };
        assert!(mk(0, false).ok());
        assert!(!mk(1, false).ok());
        assert!(mk(1, true).ok()); // allowed failure still "ok" for the gate
        assert!(!mk(INFRA_FAILURE, false).ok());
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
    fn parse_owner_pid_and_orphan_detection() {
        assert_eq!(parse_owner_pid("ci-base-deadbeef-4242-0-snap"), Some(4242));
        assert_eq!(
            parse_owner_pid("ci-base-deadbeef-4242-7-snap-job3-make"),
            Some(4242)
        );
        assert_eq!(parse_owner_pid("other-box"), None);
        assert_eq!(parse_owner_pid("ci-base-onlykey"), None);
        // this process's own resources are never orphans, regardless of liveness.
        let me = std::process::id();
        let mine = format!("ci-base-deadbeef-{me}-0-snap");
        assert_eq!(orphan_pid(&mine, me), None);
        // a confirmed-dead pid is an orphan — but only where liveness is knowable
        // (Linux /proc); on hosts where it isn't, sweep must leave it untouched.
        if pid_alive(4_000_000_000).is_some() {
            assert_eq!(
                orphan_pid("ci-base-deadbeef-4000000000-0-snap", me),
                Some(4_000_000_000)
            );
        } else {
            assert_eq!(orphan_pid("ci-base-deadbeef-4000000000-0-snap", me), None);
        }
    }

    #[test]
    fn ci_error_display_covers_each_variant() {
        let io = PipelineError::from(std::io::Error::other("boom"));
        assert!(format!("{io}").contains("boom"));
        let cli = PipelineError::Cli {
            args: "snapshot rm s".into(),
            code: Some(1),
            stderr: "still used".into(),
        };
        assert!(format!("{cli}").contains("still used"));
        let step = PipelineError::StepFailed {
            name: "test".into(),
            code: 7,
            logs: "oops".into(),
        };
        let s = format!("{step}");
        assert!(s.contains("test") && s.contains("exit 7"));
        assert!(format!("{}", PipelineError::NotReady("b1".into())).contains("b1"));
    }

    // A POSIX-sh stub standing in for `a3s-box`: enough to drive warm_base + step
    // through their happy paths (run/exec/start/rm succeed; `snapshot create`
    // records name->id in a state file that `snapshot ls` echoes; an exec whose
    // command contains "boom" exits 7, "emitmetric" prints a `::metric` line;
    // a `snapshot restore` whose target name contains "infrafail" exits 1).
    #[cfg(unix)]
    const FAKE_BOX: &str = r#"#!/bin/sh
state="$(dirname "$0")/.snaps"
case "$1" in
  run|start|rm) exit 0 ;;
  exec) last=""; for a in "$@"; do last="$a"; done
        case "$last" in
          *emitmetric*) printf '::metric score=9\n'; exit 0 ;;
          *boom*) exit 7 ;;
          *) exit 0 ;;
        esac ;;
  snapshot)
    case "$2" in
      create) name=""; while [ "$#" -gt 0 ]; do [ "$1" = "--name" ] && name="$2"; shift; done
              printf '%s %s\n' "snap-fake1" "$name" >> "$state"; printf 'snap-fake1\n'; exit 0 ;;
      restore)
        for a in "$@"; do
          case "$a" in
            *infrafail*) exit 1 ;;
            *flaky*) c="$(dirname "$0")/.flaky"; n=$(cat "$c" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$c"; [ "$n" -le 2 ] && exit 1 || exit 0 ;;
          esac
        done
        exit 0 ;;
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
        let base = warm_base(WarmBase::new("img", "echo hi").cache(&cache)).expect("warm_base");
        let r = base.step(Step::new("build", "make")).expect("step");
        assert_eq!(r.exit_code, 0);
        assert!(!r.cached);
        assert!(base.step(Step::new("build", "make")).expect("step2").cached); // cache hit
        assert!(matches!(
            base.step(Step::new("fail", "boom")),
            Err(PipelineError::StepFailed { code: 7, .. })
        ));
        // allow_failure: a non-zero step returns Ok with the code instead of Err.
        let allowed = base
            .step(Step::new("maybe", "boom").allow_failure())
            .expect("allow_failure step");
        assert_eq!(allowed.exit_code, 7);

        // run_parallel: collect-all, input order preserved, metrics parsed, gate derived.
        let rep = base.run_parallel(
            vec![
                Step::new("a", "make"),
                Step::new("b", "boom"),       // exits 7 -> not ok
                Step::new("m", "emitmetric"), // prints ::metric score=9
            ],
            2,
        );
        assert_eq!(rep.steps.len(), 3);
        assert_eq!(rep.steps[0].name, "a"); // order preserved despite concurrency
        assert_eq!(rep.steps[1].exit_code, 7);
        assert_eq!(rep.steps[2].metrics.get("score"), Some(&9.0));
        assert!(!rep.passed);
        assert_eq!(rep.failures().len(), 1);
        assert!(rep.to_json().contains("\"score\":9"));

        // an allowed-failure step must NOT fail the batch gate.
        let rep2 = base.run_parallel(
            vec![
                Step::new("ok", "make"),
                Step::new("soft", "boom").allow_failure(),
            ],
            2,
        );
        assert!(rep2.passed, "allow_failure step must not fail the batch");

        // an infra failure (restore refuses) surfaces as INFRA_FAILURE, not a panic
        // (it is retried infra_retries times first, then gives up).
        let rep3 = base.run_parallel(vec![Step::new("infrafail", "make")], 1);
        assert_eq!(rep3.steps[0].exit_code, INFRA_FAILURE);
        assert!(!rep3.steps[0].stderr.is_empty());
        assert!(!rep3.passed);

        // a TRANSIENT infra failure is retried: "flaky" restore fails twice then
        // succeeds, so with the default 2 retries (3 attempts) the step recovers.
        let _ = std::fs::remove_file(dir.join(".flaky"));
        let okr = base.run_parallel(vec![Step::new("flaky", "make")], 1);
        assert_eq!(
            okr.steps[0].exit_code, 0,
            "transient infra failure should be retried to success"
        );
        assert!(okr.passed);

        // empty batch is trivially passed.
        assert!(base.run_parallel(Vec::new(), 4).passed);

        // Two Bases from the SAME spec must get disjoint names (no cross-talk).
        let b1 = warm_base(WarmBase::new("u", "echo").cache(&cache)).expect("b1");
        let b2 = warm_base(WarmBase::new("u", "echo").cache(&cache)).expect("b2");
        assert_ne!(
            b1.snapshot_name, b2.snapshot_name,
            "same spec must yield disjoint snapshot names"
        );
        b1.dispose();
        b2.dispose();

        base.dispose();
        base.dispose(); // idempotent: second dispose + Drop must not double-remove

        // A cache-less base exercises the no-cache branch in step().
        let nocache = warm_base(WarmBase::new("img", "echo hi")).expect("warm_base nocache");
        assert!(
            !nocache
                .step(Step::new("x", "make"))
                .expect("nocache step")
                .cached
        );
        nocache.dispose();

        // Failure path: a missing binary must surface as Err, never panic.
        std::env::set_var("A3S_BOX", "/nonexistent/a3s-box-xyzzy");
        assert!(matches!(box_run(&["version"]), Err(PipelineError::Io(_))));
        box_cleanup(&["rm", "-f", "x"]); // best-effort: must not panic
        assert!(snapshot_id("any").is_err());
        assert!(warm_base(WarmBase::new("img", "echo hi")).is_err());
        // wait_ready times out (NotReady) when the box never readies (fast: 0 budget).
        assert!(matches!(
            wait_ready_inner("b", std::time::Duration::from_millis(0)),
            Err(PipelineError::NotReady(_))
        ));

        std::env::remove_var("A3S_BOX");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builders_chain_all_options() {
        // Exercise every builder method (fields are private; chaining covers the bodies).
        let c = FileCache::new(std::env::temp_dir().join(format!("a3s-ci-bld-{}", key(&["bld"]))))
            .unwrap();
        let _ = WarmBase::new("img", "setup")
            .env("A", "1")
            .cache(&c)
            .max_output(4096)
            .infra_retries(1);
        let _ = Step::new("n", "cmd")
            .input("x")
            .env("K", "V")
            .allow_failure();
    }
}
