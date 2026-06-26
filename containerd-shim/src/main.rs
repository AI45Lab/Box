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
    containerd_shim::asynchronous::run::<Service>("io.containerd.a3s-box.v2", None).await;
}
