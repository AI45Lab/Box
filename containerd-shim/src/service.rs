//! Thin containerd shim v2 service for a3s-box.
//!
//! Maps the containerd runtime-v2 Task API onto the `a3s-box` CLI:
//!   * CRI pod-sandbox (pause) container  -> a host `sleep` placeholder (a3s-box
//!     has no pause container; the MicroVM itself is the sandbox).
//!   * workload container                 -> `a3s-box run -d --name <id> <image> -- <args>`
//!     (detached so the box registers in a3s-box state and is exec-able).
//!   * kubectl exec                       -> `a3s-box exec <id> -- <cmd>` (Task.exec/start
//!     create+run an exec process with stdio wired to the containerd FIFOs).
//!
//! State for all tasks served by this shim process lives in a shared map.
//! `Shim::wait` blocks until `shutdown` fires the ExitSignal.

use std::collections::HashMap;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use containerd_shim::asynchronous::{spawn, ExitSignal, Shim};
use containerd_shim::publisher::RemotePublisher;
use containerd_shim::{Config, Error, Flags, StartOpts, TtrpcContext, TtrpcResult};
use containerd_shim_protos::shim_async::Task;
use containerd_shim_protos::{api, protobuf, ttrpc};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::logic;

/// CLI binary; override with A3S_BOX_BIN for non-PATH installs.
fn a3s_box_bin() -> String {
    std::env::var("A3S_BOX_BIN").unwrap_or_else(|_| "a3s-box".to_string())
}

/// Build an `a3s-box` command with a STABLE A3S_HOME so run/exec/wait/stop/rm all
/// share one boxes.json. containerd hands the shim a near-empty env (often no
/// HOME), so without this each invocation resolves a different state file and
/// `exec` reports "No such box". Override with A3S_HOME if the host sets one.
fn a3s_box_cmd() -> Command {
    let mut c = Command::new(a3s_box_bin());
    c.env("A3S_HOME", logic::a3s_home());
    c
}

#[derive(Default)]
struct Proc {
    bundle: String,
    is_sandbox: bool,
    image: Option<String>,
    args: Vec<String>,
    env: Vec<String>,
    cpus: Option<u32>,
    memory_mb: Option<u64>,
    stdout: String,
    stderr: String,
    pid: u32,
    status: i32, // api::Status enum value: 0 unknown,1 created,2 running,3 stopped
    exit_code: u32,
    child: Option<Child>,
}

/// An exec process (kubectl exec) running inside a box via `a3s-box exec`.
#[derive(Default)]
struct ExecProc {
    container_id: String,
    args: Vec<String>,
    stdin: String,
    stdout: String,
    stderr: String,
    pid: u32,
    exit_code: u32,
    child: Option<Child>,
}

#[derive(Default)]
struct State {
    procs: HashMap<String, Proc>,
    execs: HashMap<String, ExecProc>, // keyed by exec_id (globally unique)
}

#[derive(Clone)]
pub struct Service {
    id: String,
    namespace: String,
    // ExitSignal isn't Clone; share one across the Shim + its task service.
    exit: Arc<ExitSignal>,
    state: Arc<Mutex<State>>,
}

// ---- helpers ---------------------------------------------------------------

/// Parse the OCI bundle's config.json for the fields we need.
fn read_spec(bundle: &str) -> serde_json::Value {
    let path = format!("{bundle}/config.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Open a containerd stdio FIFO path for writing and hand it to a child as Stdio.
/// Empty path => inherit null (detached).
fn open_fifo_write(path: &str) -> Stdio {
    if path.is_empty() {
        return Stdio::null();
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        // SAFETY: we own the fd and pass it straight to the child.
        Ok(f) => unsafe { Stdio::from_raw_fd(f.into_raw_fd()) },
        Err(_) => Stdio::null(),
    }
}

/// Open a containerd stdin FIFO path for reading. Empty path => null.
fn open_fifo_read(path: &str) -> Stdio {
    if path.is_empty() {
        return Stdio::null();
    }
    match std::fs::OpenOptions::new().read(true).open(path) {
        // SAFETY: we own the fd and pass it straight to the child.
        Ok(f) => unsafe { Stdio::from_raw_fd(f.into_raw_fd()) },
        Err(_) => Stdio::null(),
    }
}

impl Service {
    async fn populate_from_bundle(&self, id: &str, bundle: &str, stdout: &str, stderr: &str) {
        let parsed = logic::parse_spec(&read_spec(bundle));
        let mut st = self.state.lock().await;
        st.procs.insert(
            id.to_string(),
            Proc {
                bundle: bundle.to_string(),
                is_sandbox: parsed.is_sandbox,
                image: parsed.image,
                args: parsed.args,
                env: parsed.env,
                cpus: parsed.cpus,
                memory_mb: parsed.memory_mb,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
                status: 1, // CREATED
                ..Default::default()
            },
        );
    }
}

// ---- Shim (process lifecycle) ---------------------------------------------

#[async_trait]
impl Shim for Service {
    type T = Service;

    async fn new(_runtime_id: &str, args: &Flags, _config: &mut Config) -> Self {
        Service {
            id: args.id.clone(),
            namespace: args.namespace.clone(),
            exit: Arc::new(ExitSignal::default()),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    async fn start_shim(&mut self, opts: StartOpts) -> Result<String, Error> {
        let grouping = opts.id.clone();
        let address = spawn(opts, &grouping, Vec::new()).await?;
        Ok(address)
    }

    async fn delete_shim(&mut self) -> Result<api::DeleteResponse, Error> {
        // Out-of-band cleanup: force-remove the box if it lingers.
        let _ = a3s_box_cmd()
            .args(["rm", "-f", &self.id])
            .output()
            .await;
        let mut r = api::DeleteResponse::new();
        r.set_exit_status(0);
        Ok(r)
    }

    async fn wait(&mut self) {
        self.exit.wait().await;
    }

    async fn create_task_service(&self, _publisher: RemotePublisher) -> Self::T {
        self.clone()
    }
}

// ---- Task (per-container RPCs) --------------------------------------------

#[async_trait]
impl Task for Service {
    async fn create(
        &self,
        _ctx: &TtrpcContext,
        req: api::CreateTaskRequest,
    ) -> TtrpcResult<api::CreateTaskResponse> {
        self.populate_from_bundle(req.id(), req.bundle(), req.stdout(), req.stderr())
            .await;
        let mut resp = api::CreateTaskResponse::new();
        resp.set_pid(std::process::id()); // provisional; real pid set on start
        Ok(resp)
    }

    async fn start(
        &self,
        _ctx: &TtrpcContext,
        req: api::StartRequest,
    ) -> TtrpcResult<api::StartResponse> {
        let id = req.id().to_string();
        let exec_id = req.exec_id().to_string();

        // kubectl exec: start a previously-created exec process inside the box.
        if !exec_id.is_empty() {
            let mut st = self.state.lock().await;
            let e = st
                .execs
                .get_mut(&exec_id)
                .ok_or_else(|| ttrpc_err(format!("unknown exec {exec_id}")))?;
            let mut cmd = a3s_box_cmd();
            cmd.args(logic::exec_args(&e.container_id, &e.args));
            cmd.stdin(open_fifo_read(&e.stdin))
                .stdout(open_fifo_write(&e.stdout))
                .stderr(open_fifo_write(&e.stderr));
            let child = cmd
                .spawn()
                .map_err(|err| ttrpc_err(format!("a3s-box exec spawn failed: {err}")))?;
            e.pid = child.id().unwrap_or(0);
            e.child = Some(child);
            let mut resp = api::StartResponse::new();
            resp.set_pid(e.pid);
            return Ok(resp);
        }

        let mut st = self.state.lock().await;
        let p = st
            .procs
            .get_mut(&id)
            .ok_or_else(|| ttrpc_err(format!("unknown container {id}")))?;

        if p.is_sandbox {
            // Pod sandbox: no MicroVM; hold a placeholder process for lifetime.
            let child = Command::new("sleep")
                .arg("infinity")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| ttrpc_err(format!("spawn sandbox placeholder failed: {e}")))?;
            p.pid = child.id().unwrap_or(0);
            p.child = Some(child);
        } else {
            // Workload: launch DETACHED via `setsid a3s-box run -d`, fire-and-forget.
            // This replicates the interactive-shell `run -d` that BINDS exec.sock and
            // orphans the libkrun shim to init. (Foreground run — and a plain
            // shim-spawned `run -d` held as a child — do NOT bind exec.sock, so
            // kubectl exec can't reach the guest.) We then poll the box record until
            // it reports running, by which point boot's exec-readiness has completed.
            let image = p
                .image
                .clone()
                .ok_or_else(|| ttrpc_err("no image annotation; raw ctr is unsupported".into()))?;
            let cpus = p.cpus;
            let memory = p.memory_mb;
            let env = p.env.clone();
            let args = p.args.clone();
            drop(st); // release the lock across spawn + readiness poll

            let mut cmd = Command::new("setsid");
            cmd.env("A3S_HOME", logic::a3s_home());
            cmd.arg(a3s_box_bin());
            cmd.args(logic::run_args(&id, &image, cpus, memory, &env, &args));
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            cmd.spawn()
                .map_err(|e| ttrpc_err(format!("setsid a3s-box run -d spawn failed: {e}")))?;

            let mut running = false;
            for _ in 0..120 {
                if let Ok(o) = a3s_box_cmd().args(["inspect", &id]).output().await {
                    if o.status.success() && logic::is_running(&String::from_utf8_lossy(&o.stdout)) {
                        running = true;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
            if !running {
                return Err(ttrpc_err(format!("box {id} did not become running")));
            }
            let mut st = self.state.lock().await;
            if let Some(p) = st.procs.get_mut(&id) {
                p.status = 2; // RUNNING
                p.pid = std::process::id();
            }
            let mut resp = api::StartResponse::new();
            resp.set_pid(std::process::id());
            return Ok(resp);
        }
        p.status = 2; // RUNNING (sandbox)

        let mut resp = api::StartResponse::new();
        resp.set_pid(p.pid);
        Ok(resp)
    }

    async fn exec(
        &self,
        _ctx: &TtrpcContext,
        req: api::ExecProcessRequest,
    ) -> TtrpcResult<api::Empty> {
        let container_id = req.id().to_string();
        let exec_id = req.exec_id().to_string();
        // containerd marshals the OCI Process spec as JSON inside the protobuf Any.
        let args = logic::parse_exec_command(&req.spec().value);
        let mut st = self.state.lock().await;
        st.execs.insert(
            exec_id,
            ExecProc {
                container_id,
                args,
                stdin: req.stdin().to_string(),
                stdout: req.stdout().to_string(),
                stderr: req.stderr().to_string(),
                ..Default::default()
            },
        );
        Ok(api::Empty::new())
    }

    async fn wait(
        &self,
        _ctx: &TtrpcContext,
        req: api::WaitRequest,
    ) -> TtrpcResult<api::WaitResponse> {
        let id = req.id().to_string();
        let exec_id = req.exec_id().to_string();

        // kubectl exec: wait for the exec process to exit.
        if !exec_id.is_empty() {
            let child = {
                let mut st = self.state.lock().await;
                st.execs.get_mut(&exec_id).and_then(|e| e.child.take())
            };
            let code = if let Some(mut c) = child {
                c.wait().await.ok().and_then(|s| s.code()).unwrap_or(0) as u32
            } else {
                0
            };
            let mut st = self.state.lock().await;
            if let Some(e) = st.execs.get_mut(&exec_id) {
                e.exit_code = code;
            }
            let mut resp = api::WaitResponse::new();
            resp.set_exit_status(code);
            return Ok(resp);
        }

        // Take the child out so we can await it without holding the lock.
        let child = {
            let mut st = self.state.lock().await;
            st.procs.get_mut(&id).and_then(|p| p.child.take())
        };
        let code = if let Some(mut c) = child {
            c.wait().await.ok().and_then(|s| s.code()).unwrap_or(0) as u32
        } else {
            // Workload box (detached): block on `a3s-box wait <id>` until it exits.
            a3s_box_cmd()
                .args(["wait", &id])
                .output()
                .await
                .ok()
                .map(|o| logic::parse_wait_exit_code(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or(0)
        };
        let mut st = self.state.lock().await;
        if let Some(p) = st.procs.get_mut(&id) {
            p.status = 3; // STOPPED
            p.exit_code = code;
        }
        let mut resp = api::WaitResponse::new();
        resp.set_exit_status(code);
        Ok(resp)
    }

    async fn state(
        &self,
        _ctx: &TtrpcContext,
        req: api::StateRequest,
    ) -> TtrpcResult<api::StateResponse> {
        let exec_id = req.exec_id().to_string();
        let st = self.state.lock().await;
        let mut resp = api::StateResponse::new();
        resp.set_id(req.id().to_string());
        if !exec_id.is_empty() {
            resp.set_exec_id(exec_id.clone());
            if let Some(e) = st.execs.get(&exec_id) {
                resp.set_pid(e.pid);
                resp.set_exit_status(e.exit_code);
                resp.set_stdin(e.stdin.clone());
                resp.set_stdout(e.stdout.clone());
                resp.set_stderr(e.stderr.clone());
                let s = if e.child.is_some() {
                    2 // running
                } else if e.pid > 0 {
                    3 // stopped
                } else {
                    1 // created
                };
                resp.status = protobuf_enum_status(s);
            }
            return Ok(resp);
        }
        if let Some(p) = st.procs.get(req.id()) {
            resp.set_pid(p.pid);
            resp.set_exit_status(p.exit_code);
            resp.set_bundle(p.bundle.clone());
            resp.set_stdout(p.stdout.clone());
            resp.set_stderr(p.stderr.clone());
            resp.status = protobuf_enum_status(p.status);
        }
        Ok(resp)
    }

    async fn kill(&self, _ctx: &TtrpcContext, req: api::KillRequest) -> TtrpcResult<api::Empty> {
        let id = req.id().to_string();
        let exec_id = req.exec_id().to_string();

        // kubectl exec cancel/timeout: signal just the exec process.
        if !exec_id.is_empty() {
            let st = self.state.lock().await;
            if let Some(e) = st.execs.get(&exec_id) {
                if e.pid > 0 {
                    let _ = Command::new("kill").arg(e.pid.to_string()).output().await;
                }
            }
            return Ok(api::Empty::new());
        }

        let st = self.state.lock().await;
        if let Some(p) = st.procs.get(&id) {
            if p.is_sandbox {
                let _ = Command::new("kill").arg(p.pid.to_string()).output().await;
            } else {
                let _ = a3s_box_cmd()
                    .args(["stop", &id])
                    .output()
                    .await;
            }
        }
        Ok(api::Empty::new())
    }

    async fn delete(
        &self,
        _ctx: &TtrpcContext,
        req: api::DeleteRequest,
    ) -> TtrpcResult<api::DeleteResponse> {
        let id = req.id().to_string();
        let exec_id = req.exec_id().to_string();

        // kubectl exec teardown: just drop the exec record.
        if !exec_id.is_empty() {
            let mut st = self.state.lock().await;
            let (pid, code) = st
                .execs
                .remove(&exec_id)
                .map(|e| (e.pid, e.exit_code))
                .unwrap_or((0, 0));
            let mut resp = api::DeleteResponse::new();
            resp.set_pid(pid);
            resp.set_exit_status(code);
            return Ok(resp);
        }

        let mut st = self.state.lock().await;
        let (pid, code) = st
            .procs
            .remove(&id)
            .map(|p| (p.pid, p.exit_code))
            .unwrap_or((0, 0));
        drop(st);
        let _ = a3s_box_cmd()
            .args(["rm", "-f", &id])
            .output()
            .await;
        let mut resp = api::DeleteResponse::new();
        resp.set_pid(pid);
        resp.set_exit_status(code);
        Ok(resp)
    }

    async fn connect(
        &self,
        _ctx: &TtrpcContext,
        _req: api::ConnectRequest,
    ) -> TtrpcResult<api::ConnectResponse> {
        let mut resp = api::ConnectResponse::new();
        resp.set_shim_pid(std::process::id());
        Ok(resp)
    }

    async fn shutdown(
        &self,
        _ctx: &TtrpcContext,
        _req: api::ShutdownRequest,
    ) -> TtrpcResult<api::Empty> {
        self.exit.signal();
        Ok(api::Empty::new())
    }
}

fn ttrpc_err(msg: String) -> ttrpc::Error {
    ttrpc::Error::Others(msg)
}

/// Map our small status int to the protobuf Status enum field type.
fn protobuf_enum_status(s: i32) -> protobuf::EnumOrUnknown<api::Status> {
    let st = match s {
        1 => api::Status::CREATED,
        2 => api::Status::RUNNING,
        3 => api::Status::STOPPED,
        _ => api::Status::UNKNOWN,
    };
    protobuf::EnumOrUnknown::new(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use containerd_shim::TtrpcContext;

    fn svc() -> Service {
        Service {
            id: "shim-test".into(),
            namespace: "k8s.io".into(),
            exit: Arc::new(ExitSignal::default()),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn ctx() -> TtrpcContext {
        TtrpcContext {
            mh: Default::default(),
            metadata: Default::default(),
            timeout_nano: 0,
        }
    }

    // Write a minimal OCI bundle (config.json) and return its dir.
    fn bundle(json: &str) -> String {
        let dir = std::env::temp_dir().join(format!("a3sshim-{}", std::process::id()));
        let sub = dir.join(format!("{:p}", json));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("config.json"), json).unwrap();
        sub.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn create_workload_then_state() {
        let s = svc();
        let b = bundle(
            r#"{"annotations":{"io.kubernetes.cri.image-name":"alpine",
                "io.kubernetes.cri.container-type":"container"},
                "process":{"args":["sleep","1"]}}"#,
        );
        let mut req = api::CreateTaskRequest::new();
        req.set_id("c1".into());
        req.set_bundle(b);
        let resp = s.create(&ctx(), req).await.unwrap();
        assert!(resp.pid() > 0);
        // state reports the created container
        let mut sreq = api::StateRequest::new();
        sreq.set_id("c1".into());
        let st = s.state(&ctx(), sreq).await.unwrap();
        assert_eq!(st.id(), "c1");
    }

    #[tokio::test]
    async fn create_sandbox_is_marked() {
        let s = svc();
        let b = bundle(r#"{"annotations":{"io.kubernetes.cri.container-type":"sandbox"}}"#);
        let mut req = api::CreateTaskRequest::new();
        req.set_id("sb".into());
        req.set_bundle(b);
        s.create(&ctx(), req).await.unwrap();
        let g = s.state.lock().await;
        assert!(g.procs.get("sb").unwrap().is_sandbox);
    }

    #[tokio::test]
    async fn exec_registers_then_delete_removes() {
        let s = svc();
        let mut req = api::ExecProcessRequest::new();
        req.set_id("c1".into());
        req.set_exec_id("e1".into());
        let mut any = containerd_shim_protos::protobuf::well_known_types::any::Any::new();
        any.value = br#"{"args":["echo","hi"]}"#.to_vec();
        req.spec = containerd_shim_protos::protobuf::MessageField::some(any);
        s.exec(&ctx(), req).await.unwrap();
        {
            let g = s.state.lock().await;
            assert_eq!(g.execs.get("e1").unwrap().args, vec!["echo", "hi"]);
        }
        // delete exec branch removes the record
        let mut dreq = api::DeleteRequest::new();
        dreq.set_id("c1".into());
        dreq.set_exec_id("e1".into());
        s.delete(&ctx(), dreq).await.unwrap();
        assert!(s.state.lock().await.execs.get("e1").is_none());
    }

    #[tokio::test]
    async fn state_exec_branch_reports_created() {
        let s = svc();
        s.state.lock().await.execs.insert(
            "e9".into(),
            ExecProc { container_id: "c1".into(), ..Default::default() },
        );
        let mut req = api::StateRequest::new();
        req.set_id("c1".into());
        req.set_exec_id("e9".into());
        let st = s.state(&ctx(), req).await.unwrap();
        assert_eq!(st.exec_id(), "e9");
    }

    #[tokio::test]
    async fn connect_and_shutdown() {
        let s = svc();
        let c = s.connect(&ctx(), api::ConnectRequest::new()).await.unwrap();
        assert!(c.shim_pid() > 0);
        s.shutdown(&ctx(), api::ShutdownRequest::new()).await.unwrap();
        // shutdown fired the exit signal; Shim::wait would now return.
    }

    #[tokio::test]
    async fn kill_exec_branch_no_pid_is_noop() {
        let s = svc();
        s.state.lock().await.execs.insert(
            "e0".into(),
            ExecProc { container_id: "c1".into(), pid: 0, ..Default::default() },
        );
        let mut req = api::KillRequest::new();
        req.set_id("c1".into());
        req.set_exec_id("e0".into());
        s.kill(&ctx(), req).await.unwrap(); // pid 0 => no signal sent
    }

    #[tokio::test]
    async fn start_unknown_container_errors() {
        let s = svc();
        let mut req = api::StartRequest::new();
        req.set_id("missing".into());
        assert!(s.start(&ctx(), req).await.is_err());
    }

    #[tokio::test]
    async fn start_unknown_exec_errors() {
        let s = svc();
        let mut req = api::StartRequest::new();
        req.set_id("c1".into());
        req.set_exec_id("nope".into());
        assert!(s.start(&ctx(), req).await.is_err());
    }

    #[tokio::test]
    async fn wait_exec_no_child_returns_zero() {
        let s = svc();
        s.state.lock().await.execs.insert(
            "e2".into(),
            ExecProc { container_id: "c1".into(), ..Default::default() },
        );
        let mut req = api::WaitRequest::new();
        req.set_id("c1".into());
        req.set_exec_id("e2".into());
        let r = s.wait(&ctx(), req).await.unwrap();
        assert_eq!(r.exit_status(), 0);
    }

    #[tokio::test]
    async fn start_sandbox_then_kill_reaps() {
        let s = svc();
        s.state.lock().await.procs.insert(
            "sb".into(),
            Proc { is_sandbox: true, status: 1, ..Default::default() },
        );
        let mut sreq = api::StartRequest::new();
        sreq.set_id("sb".into());
        let r = s.start(&ctx(), sreq).await.unwrap();
        assert!(r.pid() > 0);
        // kill sandbox branch signals the placeholder pid.
        let mut kreq = api::KillRequest::new();
        kreq.set_id("sb".into());
        s.kill(&ctx(), kreq).await.unwrap();
    }

    #[tokio::test]
    async fn start_exec_spawns_then_wait_reaps() {
        let s = svc();
        // `a3s-box exec` on a missing box errors fast, but the spawn + fifo paths
        // are exercised. stdout points at a real temp file (open-for-write branch).
        let tmp = std::env::temp_dir().join(format!("a3sout-{}.log", std::process::id()));
        let _ = std::fs::File::create(&tmp);
        s.state.lock().await.execs.insert(
            "ex".into(),
            ExecProc {
                container_id: "nobox".into(),
                args: vec!["true".into()],
                stdout: tmp.to_string_lossy().to_string(),
                ..Default::default()
            },
        );
        let mut sreq = api::StartRequest::new();
        sreq.set_id("nobox".into());
        sreq.set_exec_id("ex".into());
        assert!(s.start(&ctx(), sreq).await.unwrap().pid() > 0);
        let mut wreq = api::WaitRequest::new();
        wreq.set_id("nobox".into());
        wreq.set_exec_id("ex".into());
        let _ = s.wait(&ctx(), wreq).await.unwrap(); // reaps the exec child
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn delete_container_runs_rm() {
        let s = svc();
        s.state
            .lock()
            .await
            .procs
            .insert("cdel".into(), Proc { status: 2, ..Default::default() });
        let mut req = api::DeleteRequest::new();
        req.set_id("cdel".into());
        s.delete(&ctx(), req).await.unwrap(); // spawns `a3s-box rm -f cdel`
        assert!(s.state.lock().await.procs.get("cdel").is_none());
    }

    #[tokio::test]
    async fn kill_container_runs_stop() {
        let s = svc();
        s.state.lock().await.procs.insert(
            "ckill".into(),
            Proc { is_sandbox: false, status: 2, ..Default::default() },
        );
        let mut req = api::KillRequest::new();
        req.set_id("ckill".into());
        s.kill(&ctx(), req).await.unwrap(); // spawns `a3s-box stop ckill`
    }

    #[tokio::test]
    async fn delete_shim_runs_rm() {
        let mut s = svc();
        let r = s.delete_shim().await.unwrap();
        assert_eq!(r.exit_status(), 0);
    }
}
