//! containerd-shim-a3s-box-v2 — a containerd runtime shim v2 that routes
//! `RuntimeClass=a3s-box` pods to the a3s-box MicroVM runtime.
//!
//! Model: containerd maps the runtime handler `a3s-box` (runtime_type
//! `io.containerd.a3s-box.v2`) to this binary. It is a THIN shim: it drives the
//! installed `a3s-box` CLI rather than linking the runtime, so it builds fast
//! and never perturbs the runtime's build. Each workload container becomes one
//! `a3s-box run` MicroVM; the CRI pod-sandbox (pause) container is a lightweight
//! placeholder (a3s-box has no pause container — the VM itself is the sandbox).
//!
//! Known MVP limitation: a3s-box manages its own image rootfs, so the shim
//! resolves the image from the CRI annotation `io.kubernetes.cri.image-name`
//! and lets a3s-box pull it, rather than consuming containerd's prepared
//! rootfs. This works for the RuntimeClass pod path; raw `ctr run` (no image
//! annotation) is not supported.

mod logic;
mod service;

use service::Service;

#[tokio::main]
async fn main() {
    // Pass the Config via `opts` (NOT from Shim::new — the framework reads it to
    // set up signals + subreaper BEFORE calling Shim::new, so changes there are
    // no-ops). Disable the framework's child reaper and subreaper: it sets
    // PR_SET_CHILD_SUBREAPER and a SIGCHLD `waitpid(-1)` loop that reaps the
    // short-lived `a3s-box run -d` CLI and publishes a spurious container-exit
    // seconds after start, making containerd kill the still-running box. We hold
    // each task's process as a tokio Child and reap it ourselves in Task.wait().
    let config = containerd_shim::Config {
        no_reaper: true,
        no_sub_reaper: true,
        ..Default::default()
    };
    containerd_shim::asynchronous::run::<Service>("io.containerd.a3s-box.v2", Some(config)).await;
}
