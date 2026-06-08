//! Unit tests for the CRI runtime service.

use super::*;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::time::{sleep, Duration};

use crate::streaming::StreamingServer;

#[test]
fn test_kept_capabilities() {
    let cap = |add: &[&str], drop: &[&str]| Capability {
        add_capabilities: add.iter().map(|s| s.to_string()).collect(),
        drop_capabilities: drop.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let has = |v: &[String], c: &str| v.iter().any(|x| x == c);

    // No capabilities field -> the runtime default set (no NET_ADMIN/SYS_ADMIN).
    let kept = kept_capabilities(None).unwrap();
    assert!(has(&kept, "CHOWN") && has(&kept, "NET_BIND_SERVICE"));
    assert!(!has(&kept, "NET_ADMIN") && !has(&kept, "SYS_ADMIN"));

    // add NET_ADMIN -> present; drop CHOWN -> removed.
    let kept = kept_capabilities(Some(&cap(&["CAP_NET_ADMIN"], &["CHOWN"]))).unwrap();
    assert!(has(&kept, "NET_ADMIN") && !has(&kept, "CHOWN"));

    // drop ALL (no add) -> empty.
    assert!(kept_capabilities(Some(&cap(&[], &["ALL"])))
        .unwrap()
        .is_empty());

    // drop ALL + add NET_ADMIN -> only NET_ADMIN.
    assert_eq!(
        kept_capabilities(Some(&cap(&["NET_ADMIN"], &["ALL"]))).unwrap(),
        vec!["NET_ADMIN".to_string()]
    );

    // add ALL -> None (keep the full set, no restriction emitted).
    assert!(kept_capabilities(Some(&cap(&["ALL"], &[]))).is_none());
}

/// Create a BoxRuntimeService for testing.
/// Uses NoopStateStore (no disk I/O) and a dummy StreamingHandle.
fn make_test_service() -> BoxRuntimeService {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let streaming_server = StreamingServer::new(addr);
    let handle = streaming_server.handle();
    let (image_store, image_store_tempdir) = make_test_image_store();
    let (network_store, network_store_tempdir) = make_test_network_store();

    BoxRuntimeService {
        store: Arc::new(PersistentCriStore::new(Arc::new(NoopStateStore))),
        image_store,
        network_store,
        _image_store_tempdir: Some(image_store_tempdir),
        _network_store_tempdir: Some(network_store_tempdir),
        vm_managers: Arc::new(RwLock::new(HashMap::new())),
        streaming: handle,
        attach_streams: Arc::new(RwLock::new(HashMap::new())),
        workload_stdins: Arc::new(RwLock::new(HashMap::new())),
        workload_stops: Arc::new(RwLock::new(HashMap::new())),
        log_reopens: Arc::new(RwLock::new(HashMap::new())),
        container_events: broadcast::channel(CONTAINER_EVENT_BUFFER).0,
        warm_pool: None,
        runtime_options: CriRuntimeOptions::default(),
        test_vm_acquire_error: None,
        test_vm_exec_socket_path: None,
    }
}

fn make_test_image_store() -> (Arc<ImageStore>, Arc<tempfile::TempDir>) {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("images");
    let store = Arc::new(ImageStore::new(&store_dir, 100 * 1024 * 1024).unwrap());
    (store, Arc::new(tmp))
}

fn make_test_network_store() -> (Arc<NetworkStore>, Arc<tempfile::TempDir>) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(NetworkStore::new(tmp.path().join("networks.json")));
    (store, Arc::new(tmp))
}

#[test]
fn test_sandbox_network_status_from_annotations() {
    let annotations = HashMap::from([
        (ANN_POD_IP.to_string(), "10.244.0.12".to_string()),
        (
            ANN_ADDITIONAL_POD_IPS.to_string(),
            "fd00::12, 10.244.0.13".to_string(),
        ),
    ]);

    let (network_ip, additional_ips) =
        sandbox_network_status_from_annotations(&annotations).unwrap();

    assert_eq!(network_ip, "10.244.0.12");
    assert_eq!(additional_ips, vec!["fd00::12", "10.244.0.13"]);
}

#[test]
fn test_sandbox_network_status_rejects_invalid_primary_ip() {
    let annotations = HashMap::from([(ANN_POD_IP.to_string(), "not-an-ip".to_string())]);

    let err = sandbox_network_status_from_annotations(&annotations).unwrap_err();

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("Invalid CRI sandbox IP"));
}

#[test]
fn test_sandbox_network_status_requires_primary_for_additional_ips() {
    let annotations = HashMap::from([(ANN_ADDITIONAL_POD_IPS.to_string(), "fd00::12".to_string())]);

    let err = sandbox_network_status_from_annotations(&annotations).unwrap_err();

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains(ANN_POD_IP));
}

#[test]
fn test_connect_sandbox_to_network_store_allocates_pod_ip() {
    let dir = tempfile::tempdir().unwrap();
    let store = NetworkStore::new(dir.path().join("networks.json"));
    store
        .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
        .unwrap();

    let allocation =
        connect_sandbox_to_network_store(&store, "cri-net", "sb-1", "pod-sb-1").unwrap();

    assert_eq!(allocation.network_name, "cri-net");
    assert_eq!(allocation.ip, "10.244.0.2");

    let network = store.get("cri-net").unwrap().unwrap();
    let endpoint = network.endpoints.get("sb-1").unwrap();
    assert_eq!(endpoint.box_name, "pod-sb-1");
    assert_eq!(endpoint.ip_address.to_string(), "10.244.0.2");
}

#[test]
fn test_disconnect_sandbox_from_network_store_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = NetworkStore::new(dir.path().join("networks.json"));
    store
        .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
        .unwrap();
    connect_sandbox_to_network_store(&store, "cri-net", "sb-1", "pod-sb-1").unwrap();

    disconnect_sandbox_from_network_store(&store, "cri-net", "sb-1").unwrap();
    disconnect_sandbox_from_network_store(&store, "cri-net", "sb-1").unwrap();

    let network = store.get("cri-net").unwrap().unwrap();
    assert!(network.endpoints.is_empty());
}

#[tokio::test]
async fn test_run_pod_sandbox_cleans_network_endpoint_on_pod_ip_mismatch() {
    let svc = make_test_service();
    svc.network_store
        .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
        .unwrap();

    let result = svc
        .run_pod_sandbox(Request::new(RunPodSandboxRequest {
            config: Some(PodSandboxConfig {
                metadata: Some(PodSandboxMetadata {
                    name: "pod-sb-1".to_string(),
                    uid: "uid-sb-1".to_string(),
                    namespace: "default".to_string(),
                    attempt: 0,
                }),
                log_directory: "/var/log/pods".to_string(),
                annotations: HashMap::from([
                    (ANN_NETWORK.to_string(), "cri-net".to_string()),
                    (ANN_POD_IP.to_string(), "10.244.0.99".to_string()),
                ]),
                ..Default::default()
            }),
            runtime_handler: "a3s".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err
        .message()
        .contains("does not match allocated network IP"));

    let network = svc.network_store.get("cri-net").unwrap().unwrap();
    assert!(network.endpoints.is_empty());
    assert!(svc.store.sandboxes.list(None).await.is_empty());
}

#[tokio::test]
async fn test_run_pod_sandbox_cleans_network_endpoint_on_vm_acquire_failure() {
    let mut svc = make_test_service();
    svc.test_vm_acquire_error = Some("forced VM acquire failure".to_string());
    svc.network_store
        .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
        .unwrap();

    let result = svc
        .run_pod_sandbox(Request::new(RunPodSandboxRequest {
            config: Some(PodSandboxConfig {
                metadata: Some(PodSandboxMetadata {
                    name: "pod-sb-1".to_string(),
                    uid: "uid-sb-1".to_string(),
                    namespace: "default".to_string(),
                    attempt: 0,
                }),
                log_directory: "/var/log/pods".to_string(),
                annotations: HashMap::from([(ANN_NETWORK.to_string(), "cri-net".to_string())]),
                ..Default::default()
            }),
            runtime_handler: "a3s".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Internal);
    assert!(err.message().contains("forced VM acquire failure"));

    let network = svc.network_store.get("cri-net").unwrap().unwrap();
    assert!(network.endpoints.is_empty());
    assert!(svc.store.sandboxes.list(None).await.is_empty());
}

#[tokio::test]
async fn test_cri_one_container_pod_smoke_flow() {
    let mut svc = make_test_service();
    svc.network_store
        .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
        .unwrap();
    put_test_oci_image(&svc.image_store, "example.com/app:latest").await;

    let expected_exec = Arc::new(std::sync::Mutex::new(
        None::<(Vec<String>, Vec<String>, String)>,
    ));
    let expected_exec_for_server = expected_exec.clone();
    let Some(exec_server) = spawn_exec_stream_server_with_assert(
        b"ready\n",
        b"",
        0,
        Duration::from_millis(100),
        move |request| {
            let expected = expected_exec_for_server.lock().unwrap();
            let (cmd, env, rootfs) = expected
                .as_ref()
                .expect("expected exec request should be set before StartContainer");
            assert_eq!(request.cmd.as_slice(), cmd.as_slice());
            assert_eq!(request.env.as_slice(), env.as_slice());
            assert_eq!(request.working_dir.as_deref(), Some("/image"));
            assert_eq!(request.user.as_deref(), Some("2000:2000"));
            assert_eq!(request.rootfs.as_deref(), Some(rootfs.as_str()));
        },
    )
    .await
    else {
        return;
    };
    svc.test_vm_exec_socket_path = Some(exec_server.socket_path.clone());

    let sandbox_id = svc
        .run_pod_sandbox(Request::new(RunPodSandboxRequest {
            config: Some(PodSandboxConfig {
                metadata: Some(PodSandboxMetadata {
                    name: "pod-smoke".to_string(),
                    uid: "uid-smoke".to_string(),
                    namespace: "default".to_string(),
                    attempt: 0,
                }),
                log_directory: "/var/log/pods".to_string(),
                annotations: HashMap::from([(ANN_NETWORK.to_string(), "cri-net".to_string())]),
                ..Default::default()
            }),
            runtime_handler: "a3s".to_string(),
        }))
        .await
        .unwrap()
        .into_inner()
        .pod_sandbox_id;

    let sandbox_status = svc
        .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
            pod_sandbox_id: sandbox_id.clone(),
            verbose: false,
        }))
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(sandbox_status.network.unwrap().ip, "10.244.0.2");
    assert!(svc
        .network_store
        .get("cri-net")
        .unwrap()
        .unwrap()
        .endpoints
        .contains_key(&sandbox_id));

    let log_dir = tempfile::tempdir().unwrap();
    let log_path = log_dir.path().join("container.log");
    let container_id = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: sandbox_id.clone(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "app".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "example.com/app:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                log_path: log_path.to_string_lossy().to_string(),
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner()
        .container_id;

    let container = svc.store.containers.get(&container_id).await.unwrap();
    *expected_exec.lock().unwrap() = Some((
        vec!["/usr/local/bin/app".to_string(), "serve".to_string()],
        vec![
            "PATH=/usr/local/bin:/usr/bin:/bin".to_string(),
            "ENV=image".to_string(),
        ],
        container.rootfs_guest_path.clone(),
    ));

    svc.start_container(Request::new(StartContainerRequest {
        container_id: container_id.clone(),
    }))
    .await
    .unwrap();

    let exited = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let container = svc.store.containers.get(&container_id).await.unwrap();
            if container.state == ContainerState::Exited {
                break container;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("container should exit under background supervision");
    assert_eq!(exited.exit_code, 0);

    let log = tokio::fs::read_to_string(&log_path).await.unwrap();
    assert!(log.contains(" stdout F ready\n"));

    // Avoid destroying the attached current test process; lifecycle state
    // and network cleanup are still covered by the stop/remove calls.
    svc.vm_managers.write().await.remove(&sandbox_id);

    svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
        pod_sandbox_id: sandbox_id.clone(),
    }))
    .await
    .unwrap();
    assert!(svc
        .network_store
        .get("cri-net")
        .unwrap()
        .unwrap()
        .endpoints
        .is_empty());

    svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
        pod_sandbox_id: sandbox_id.clone(),
    }))
    .await
    .unwrap();
    assert!(svc.store.sandboxes.get(&sandbox_id).await.is_none());
    assert!(svc.store.containers.get(&container_id).await.is_none());
}

struct TestExecServer {
    _tmp: tempfile::TempDir,
    socket_path: PathBuf,
}

struct TestPtyServer {
    _tmp: tempfile::TempDir,
    exec_socket_path: PathBuf,
    pty_socket_path: PathBuf,
}

fn tempdir_for_unix_socket(prefix: &str) -> tempfile::TempDir {
    let base = if Path::new("/private/tmp").exists() {
        Path::new("/private/tmp")
    } else {
        Path::new("/tmp")
    };

    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .unwrap()
}

fn bind_test_exec_listener(path: &Path) -> Option<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Some(listener),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "skipping Unix socket test; sandbox denied bind at {}: {}",
                path.display(),
                error
            );
            None
        }
        Err(error) => panic!("failed to bind test socket {}: {}", path.display(), error),
    }
}

async fn spawn_exec_stream_server_with_assert<F>(
    stdout: &'static [u8],
    stderr: &'static [u8],
    exit_code: i32,
    exit_delay: Duration,
    assert_request: F,
) -> Option<TestExecServer>
where
    F: FnOnce(&a3s_box_core::exec::ExecRequest) + Send + 'static,
{
    let tmp = tempdir_for_unix_socket("a3s-cri-exec-test");
    let socket_path = tmp.path().join("exec.sock");
    let listener = bind_test_exec_listener(&socket_path)?;

    tokio::spawn(async move {
        let mut assert_request = Some(assert_request);

        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            match reader.read_frame().await.unwrap() {
                None => continue,
                Some(frame) if frame.frame_type == a3s_transport::FrameType::Heartbeat => {
                    let heartbeat = a3s_transport::Frame::heartbeat();
                    let encoded = heartbeat.encode().unwrap();
                    writer.into_inner().write_all(&encoded).await.unwrap();
                }
                Some(frame) if frame.frame_type == a3s_transport::FrameType::Data => {
                    let request: a3s_box_core::exec::ExecRequest =
                        serde_json::from_slice(&frame.payload).unwrap();
                    assert!(request.streaming);
                    let Some(assert_request) = assert_request.take() else {
                        panic!("exec stream server received more than one request");
                    };
                    assert_request(&request);

                    if !stdout.is_empty() {
                        let chunk = a3s_box_core::exec::ExecChunk {
                            stream: a3s_box_core::exec::StreamType::Stdout,
                            data: stdout.to_vec(),
                        };
                        writer
                            .write_data(&serde_json::to_vec(&chunk).unwrap())
                            .await
                            .unwrap();
                    }

                    if !stderr.is_empty() {
                        let chunk = a3s_box_core::exec::ExecChunk {
                            stream: a3s_box_core::exec::StreamType::Stderr,
                            data: stderr.to_vec(),
                        };
                        writer
                            .write_data(&serde_json::to_vec(&chunk).unwrap())
                            .await
                            .unwrap();
                    }

                    sleep(exit_delay).await;

                    let exit = a3s_box_core::exec::ExecExit {
                        exit_code,
                        oom_killed: false,
                    };
                    writer
                        .write_control(&serde_json::to_vec(&exit).unwrap())
                        .await
                        .unwrap();
                    break;
                }
                Some(frame) => {
                    panic!("unexpected frame type: {:?}", frame.frame_type);
                }
            }
        }
    });

    Some(TestExecServer {
        _tmp: tmp,
        socket_path,
    })
}

async fn spawn_exec_stream_server(
    stdout: &'static [u8],
    stderr: &'static [u8],
    exit_code: i32,
    exit_delay: Duration,
) -> Option<TestExecServer> {
    spawn_exec_stream_server_with_assert(stdout, stderr, exit_code, exit_delay, |request| {
        assert_eq!(
            request.rootfs.as_deref(),
            Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs")
        );
    })
    .await
}

async fn spawn_multi_exec_stream_server(
    expected: Vec<(&'static str, &'static [u8], i32, Duration)>,
) -> Option<TestExecServer> {
    let tmp = tempdir_for_unix_socket("a3s-cri-multi-exec-test");
    let socket_path = tmp.path().join("exec.sock");
    let listener = bind_test_exec_listener(&socket_path)?;

    let expected = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from(
        expected,
    )));
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let expected = expected.clone();
            tokio::spawn(async move {
                let (r, w) = tokio::io::split(stream);
                let mut reader = a3s_transport::FrameReader::new(r);
                let mut writer = a3s_transport::FrameWriter::new(w);

                match reader.read_frame().await.unwrap() {
                    None => {}
                    Some(frame) if frame.frame_type == a3s_transport::FrameType::Heartbeat => {
                        let heartbeat = a3s_transport::Frame::heartbeat();
                        let encoded = heartbeat.encode().unwrap();
                        writer.into_inner().write_all(&encoded).await.unwrap();
                    }
                    Some(frame) if frame.frame_type == a3s_transport::FrameType::Data => {
                        let request: a3s_box_core::exec::ExecRequest =
                            serde_json::from_slice(&frame.payload).unwrap();
                        assert!(request.streaming);
                        let Some((expected_cmd, stdout, exit_code, exit_delay)) =
                            expected.lock().await.pop_front()
                        else {
                            panic!("multi exec stream server received unexpected request");
                        };
                        assert_eq!(request.cmd.first().map(String::as_str), Some(expected_cmd));

                        if !stdout.is_empty() {
                            let chunk = a3s_box_core::exec::ExecChunk {
                                stream: a3s_box_core::exec::StreamType::Stdout,
                                data: stdout.to_vec(),
                            };
                            writer
                                .write_data(&serde_json::to_vec(&chunk).unwrap())
                                .await
                                .unwrap();
                        }

                        sleep(exit_delay).await;

                        let exit = a3s_box_core::exec::ExecExit {
                            exit_code,
                            oom_killed: false,
                        };
                        writer
                            .write_control(&serde_json::to_vec(&exit).unwrap())
                            .await
                            .unwrap();
                    }
                    Some(frame) => panic!("unexpected frame type: {:?}", frame.frame_type),
                }
            });
        }
    });

    Some(TestExecServer {
        _tmp: tmp,
        socket_path,
    })
}

async fn spawn_pty_stream_server_with_assert<F>(
    stdout: &'static [u8],
    exit_code: i32,
    exit_delay: Duration,
    assert_request: F,
) -> Option<TestPtyServer>
where
    F: FnOnce(&a3s_box_core::pty::PtyRequest) + Send + 'static,
{
    let tmp = tempdir_for_unix_socket("a3s-cri-pty-test");
    let exec_socket_path = tmp.path().join("exec.sock");
    let pty_socket_path = tmp.path().join("pty.sock");
    let listener = bind_test_exec_listener(&pty_socket_path)?;

    tokio::spawn(async move {
        let mut assert_request = Some(assert_request);
        let (stream, _) = listener.accept().await.unwrap();
        let (r, w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let mut writer = a3s_transport::FrameWriter::new(w);

        let frame = reader.read_frame().await.unwrap().unwrap();
        assert_eq!(frame.frame_type as u8, a3s_box_core::pty::FRAME_PTY_REQUEST);
        let request: a3s_box_core::pty::PtyRequest =
            serde_json::from_slice(&frame.payload).unwrap();
        let Some(assert_request) = assert_request.take() else {
            panic!("PTY stream server received more than one request");
        };
        assert_request(&request);

        if !stdout.is_empty() {
            writer
                .write_frame(&a3s_transport::Frame {
                    frame_type: a3s_transport::FrameType::Control,
                    payload: stdout.to_vec(),
                })
                .await
                .unwrap();
        }

        sleep(exit_delay).await;

        let exit = a3s_box_core::pty::PtyExit { exit_code };
        writer
            .write_frame(&a3s_transport::Frame {
                frame_type: a3s_transport::FrameType::Error,
                payload: serde_json::to_vec(&exit).unwrap(),
            })
            .await
            .unwrap();
    });

    Some(TestPtyServer {
        _tmp: tmp,
        exec_socket_path,
        pty_socket_path,
    })
}

async fn spawn_cancelable_exec_stream_server() -> Option<TestExecServer> {
    let tmp = tempdir_for_unix_socket("a3s-cri-exec-cancel-test");
    let socket_path = tmp.path().join("exec.sock");
    let listener = bind_test_exec_listener(&socket_path)?;

    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            match reader.read_frame().await.unwrap() {
                None => continue,
                Some(frame) if frame.frame_type == a3s_transport::FrameType::Heartbeat => {
                    let heartbeat = a3s_transport::Frame::heartbeat();
                    let encoded = heartbeat.encode().unwrap();
                    writer.into_inner().write_all(&encoded).await.unwrap();
                }
                Some(frame) if frame.frame_type == a3s_transport::FrameType::Data => {
                    let request: a3s_box_core::exec::ExecRequest =
                        serde_json::from_slice(&frame.payload).unwrap();
                    assert!(request.streaming);

                    let cancel = reader.read_frame().await.unwrap().unwrap();
                    assert_eq!(cancel.frame_type, a3s_transport::FrameType::Control);
                    assert_eq!(cancel.payload, b"cancel");

                    let exit = a3s_box_core::exec::ExecExit {
                        exit_code: 137,
                        oom_killed: false,
                    };
                    writer
                        .write_control(&serde_json::to_vec(&exit).unwrap())
                        .await
                        .unwrap();
                    break;
                }
                Some(frame) => {
                    panic!("unexpected frame type: {:?}", frame.frame_type);
                }
            }
        }
    });

    Some(TestExecServer {
        _tmp: tmp,
        socket_path,
    })
}

async fn attach_ready_test_vm(box_id: &str, exec_socket_path: &Path) -> VmManager {
    let mut vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        box_id.to_string(),
    );
    vm.attach_running_process(
        std::process::id(),
        exec_socket_path.to_path_buf(),
        Some(exec_socket_path.with_file_name("pty.sock")),
    )
    .await
    .unwrap();
    vm
}

async fn put_test_oci_image(store: &ImageStore, reference: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let blobs = tmp.path().join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::write(
        tmp.path().join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();

    let config_content = r#"{
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Entrypoint": ["/usr/local/bin/app"],
                "Cmd": ["serve"],
                "Env": ["PATH=/usr/local/bin:/usr/bin:/bin", "ENV=image"],
                "WorkingDir": "/image",
                "User": "2000:2000"
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": []
            },
            "history": []
        }"#;
    let config_hash = "config456";
    std::fs::write(blobs.join(config_hash), config_content).unwrap();

    let manifest_content = format!(
        r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {{
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": "sha256:{}",
                    "size": {}
                }},
                "layers": []
            }}"#,
        config_hash,
        config_content.len()
    );
    let manifest_hash = "manifest789";
    std::fs::write(blobs.join(manifest_hash), &manifest_content).unwrap();

    let index_content = format!(
        r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [
                    {{
                        "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": "sha256:{}",
                        "size": {}
                    }}
                ]
            }}"#,
        manifest_hash,
        manifest_content.len()
    );
    std::fs::write(tmp.path().join("index.json"), index_content).unwrap();

    store
        .put(reference, "sha256:imageconfigtest", tmp.path())
        .await
        .unwrap();
}

fn test_sandbox(id: &str) -> PodSandbox {
    PodSandbox {
        id: id.to_string(),
        name: format!("pod-{}", id),
        namespace: "default".to_string(),
        uid: format!("uid-{}", id),
        state: SandboxState::Ready,
        created_at: 1_000_000_000,
        labels: HashMap::from([("app".to_string(), "test".to_string())]),
        annotations: HashMap::new(),
        log_directory: "/var/log/pods".to_string(),
        runtime_handler: "a3s".to_string(),
        network_ip: String::new(),
        additional_ips: vec![],
        dns: crate::sandbox::SandboxDns::default(),
        container_ports: vec![],
    }
}

fn test_networked_sandbox(id: &str) -> PodSandbox {
    let mut sandbox = test_sandbox(id);
    sandbox
        .annotations
        .insert(ANN_NETWORK.to_string(), "cri-net".to_string());
    sandbox
}

fn add_test_network_endpoint(svc: &BoxRuntimeService, sandbox: &mut PodSandbox) {
    if svc.network_store.get("cri-net").unwrap().is_none() {
        svc.network_store
            .create(a3s_box_core::NetworkConfig::new("cri-net", "10.244.0.0/24").unwrap())
            .unwrap();
    }

    let allocation =
        connect_sandbox_to_network_store(&svc.network_store, "cri-net", &sandbox.id, &sandbox.name)
            .unwrap();
    sandbox.network_ip = allocation.ip;
}

fn test_container(id: &str, sandbox_id: &str) -> Container {
    Container {
        id: id.to_string(),
        sandbox_id: sandbox_id.to_string(),
        name: format!("container-{}", id),
        image_ref: "nginx:latest".to_string(),
        resolved_image_digest: "sha256:test".to_string(),
        resolved_image_path: "/".to_string(),
        command: vec!["nginx".to_string()],
        args: vec!["-g".to_string(), "daemon off;".to_string()],
        env: vec![("ENV".to_string(), "test".to_string())],
        working_dir: "/".to_string(),
        user: Some("1000:1001".to_string()),
        stdin: false,
        stdin_once: false,
        tty: false,
        mounts: vec![],
        state: ContainerState::Created,
        created_at: 1_000_000_000,
        started_at: 0,
        finished_at: 0,
        exit_code: 0,
        oom_killed: false,
        labels: HashMap::from([("app".to_string(), "test".to_string())]),
        annotations: HashMap::new(),
        log_path: String::new(),
        rootfs_path: "/".to_string(),
        rootfs_guest_path: format!("/run/a3s/cri/container-rootfs/{sandbox_id}/{id}/rootfs"),
    }
}

#[tokio::test]
async fn test_cri_log_writer_flushes_partial_lines() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("nested").join("container.log");
    let log_path_string = log_path.to_string_lossy().to_string();
    let mut writer = CriLogWriter::open(&log_path_string).await.unwrap().unwrap();

    writer
        .write_chunk(a3s_box_core::exec::StreamType::Stdout, b"hello ")
        .await
        .unwrap();
    writer
        .write_chunk(a3s_box_core::exec::StreamType::Stdout, b"world")
        .await
        .unwrap();
    writer
        .write_chunk(a3s_box_core::exec::StreamType::Stderr, b"warn\n")
        .await
        .unwrap();
    writer.flush_partials().await.unwrap();

    let log = tokio::fs::read_to_string(&log_path).await.unwrap();
    assert!(log.contains(" stdout F hello world\n"));
    assert!(log.contains(" stderr F warn\n"));
}

#[test]
fn test_runtime_options_resolve_runtime_handler_agent_image() {
    let options = CriRuntimeOptions {
        default_agent_image: "ghcr.io/a3s-box/default:v1".to_string(),
        runtime_handler_agent_images: HashMap::from([(
            "a3s-secure".to_string(),
            "ghcr.io/a3s-box/secure:v1".to_string(),
        )]),
    };

    assert_eq!(
        options.agent_image_for("a3s-secure"),
        "ghcr.io/a3s-box/secure:v1"
    );
    assert_eq!(options.agent_image_for("a3s"), "ghcr.io/a3s-box/default:v1");
}

#[tokio::test]
async fn test_runtime_config_reports_cgroupfs() {
    let svc = make_test_service();
    let resp = svc
        .runtime_config(Request::new(RuntimeConfigRequest {}))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        resp.linux.unwrap().cgroup_driver,
        CgroupDriver::Cgroupfs as i32
    );
}

#[tokio::test]
async fn test_container_stats_returns_container_attributes() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.state = ContainerState::Running;
    svc.store.containers.add(container).await;

    let resp = svc
        .container_stats(Request::new(ContainerStatsRequest {
            container_id: "c-1".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    let stats = resp.stats.unwrap();
    let attrs = stats.attributes.unwrap();
    assert_eq!(attrs.id, "c-1");
    assert_eq!(attrs.metadata.unwrap().name, "container-c-1");
    assert!(stats.cpu.is_some());
    assert!(stats.memory.is_some());
}

#[tokio::test]
async fn test_container_stats_reports_rootfs_writable_layer_usage() {
    let svc = make_test_service();
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(dir.path().join("file.txt"), b"hello").unwrap();
    std::fs::write(nested.join("child.txt"), b"world!").unwrap();

    let mut container = test_container("c-usage", "sb-1");
    container.state = ContainerState::Running;
    container.rootfs_path = dir.path().to_string_lossy().to_string();
    svc.store.containers.add(container).await;

    let resp = svc
        .container_stats(Request::new(ContainerStatsRequest {
            container_id: "c-usage".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    let writable_layer = resp.stats.unwrap().writable_layer.unwrap();
    assert_eq!(
        writable_layer.fs_id.unwrap().mountpoint,
        dir.path().to_string_lossy()
    );
    assert!(writable_layer.used_bytes.unwrap().value >= 11);
    assert!(writable_layer.inodes_used.unwrap().value >= 4);
}

#[tokio::test]
async fn test_list_container_stats_only_reports_running_containers() {
    let svc = make_test_service();
    let mut running = test_container("c-running", "sb-1");
    running.state = ContainerState::Running;
    let mut exited = test_container("c-exited", "sb-1");
    exited.state = ContainerState::Exited;
    svc.store.containers.add(running).await;
    svc.store.containers.add(exited).await;

    let resp = svc
        .list_container_stats(Request::new(ListContainerStatsRequest { filter: None }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.stats.len(), 1);
    assert_eq!(resp.stats[0].attributes.as_ref().unwrap().id, "c-running");
}

fn pod_metric_value(metrics: &PodSandboxMetrics, name: &str) -> f64 {
    metrics
        .metrics
        .iter()
        .find(|metric| metric.name == name)
        .unwrap_or_else(|| panic!("missing pod sandbox metric {name}"))
        .value
}

#[tokio::test]
async fn test_list_metric_descriptors_reports_pod_sandbox_metrics() {
    let svc = make_test_service();
    let resp = svc
        .list_metric_descriptors(Request::new(ListMetricDescriptorsRequest {}))
        .await
        .unwrap()
        .into_inner();

    let names: Vec<_> = resp
        .descriptors
        .iter()
        .map(|descriptor| descriptor.name.as_str())
        .collect();
    assert!(names.contains(&"a3s_box_pod_sandbox_ready"));
    assert!(names.contains(&"a3s_box_pod_sandbox_vm_manager_present"));
    assert!(names.contains(&"a3s_box_pod_sandbox_containers_running"));
}

#[tokio::test]
async fn test_list_pod_sandbox_metrics_returns_lifecycle_snapshot() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let mut running = test_container("c-running", "sb-1");
    running.state = ContainerState::Running;
    let mut exited = test_container("c-exited", "sb-1");
    exited.state = ContainerState::Exited;
    let created = test_container("c-created", "sb-1");
    svc.store.containers.add(running).await;
    svc.store.containers.add(exited).await;
    svc.store.containers.add(created).await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let resp = svc
        .list_pod_sandbox_metrics(Request::new(ListPodSandboxMetricsRequest {
            filter: Some(PodSandboxStatsFilter {
                id: "sb-1".to_string(),
                label_selector: HashMap::new(),
            }),
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.pod_sandbox_metrics.len(), 1);
    let metrics = &resp.pod_sandbox_metrics[0];
    assert_eq!(metrics.pod_sandbox_id, "sb-1");
    assert_eq!(pod_metric_value(metrics, "a3s_box_pod_sandbox_ready"), 1.0);
    assert_eq!(
        pod_metric_value(metrics, "a3s_box_pod_sandbox_vm_manager_present"),
        1.0
    );
    assert_eq!(
        pod_metric_value(metrics, "a3s_box_pod_sandbox_containers_total"),
        3.0
    );
    assert_eq!(
        pod_metric_value(metrics, "a3s_box_pod_sandbox_containers_running"),
        1.0
    );
    assert_eq!(
        pod_metric_value(metrics, "a3s_box_pod_sandbox_containers_exited"),
        1.0
    );
    let first_metric = metrics.metrics.first().unwrap();
    assert_eq!(
        first_metric.labels.get("pod_sandbox_id"),
        Some(&"sb-1".to_string())
    );
    assert_eq!(
        first_metric.labels.get("namespace"),
        Some(&"default".to_string())
    );
}

#[tokio::test]
async fn test_stream_pod_sandbox_metrics_returns_snapshot() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let mut stream = svc
        .stream_pod_sandbox_metrics(Request::new(StreamPodSandboxMetricsRequest {
            filter: None,
        }))
        .await
        .unwrap()
        .into_inner();

    let response = stream.next().await.unwrap().unwrap();
    assert_eq!(response.pod_sandbox_metrics.len(), 1);
    assert_eq!(response.pod_sandbox_metrics[0].pod_sandbox_id, "sb-1");
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn test_stream_containers_returns_snapshot() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let mut stream = svc
        .stream_containers(Request::new(StreamContainersRequest { filter: None }))
        .await
        .unwrap()
        .into_inner();

    let response = stream.next().await.unwrap().unwrap();
    assert_eq!(response.containers.len(), 1);
    assert_eq!(response.containers[0].id, "c-1");
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn test_checkpoint_container_is_explicitly_unsupported() {
    let svc = make_test_service();
    let result = svc
        .checkpoint_container(Request::new(CheckpointContainerRequest {
            container_id: "c-1".to_string(),
            location: "/tmp/checkpoints".to_string(),
            timeout: 0,
        }))
        .await;

    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::Unimplemented);
}

#[tokio::test]
async fn test_get_container_events_streams_lifecycle_events() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let mut events = svc
        .get_container_events(Request::new(GetEventsRequest {}))
        .await
        .unwrap()
        .into_inner();

    let created = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "evented".to_string(),
                    attempt: 0,
                }),
                command: vec!["/bin/true".to_string()],
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner();

    let event = tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .expect("created event should be published")
        .unwrap()
        .unwrap();
    assert_eq!(event.container_id, created.container_id);
    assert_eq!(event.pod_sandbox_id, "sb-1");
    assert_eq!(
        event.container_event_type,
        ContainerEventType::ContainerCreatedEvent as i32
    );
    assert_eq!(event.reason, "ContainerCreated");

    svc.stop_container(Request::new(StopContainerRequest {
        container_id: created.container_id.clone(),
        timeout: 0,
    }))
    .await
    .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .expect("stopped event should be published")
        .unwrap()
        .unwrap();
    assert_eq!(event.container_id, created.container_id);
    assert_eq!(
        event.container_event_type,
        ContainerEventType::ContainerStoppedEvent as i32
    );
    assert_eq!(event.reason, "StopContainer");

    svc.remove_container(Request::new(RemoveContainerRequest {
        container_id: created.container_id.clone(),
    }))
    .await
    .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .expect("deleted event should be published")
        .unwrap()
        .unwrap();
    assert_eq!(event.container_id, created.container_id);
    assert_eq!(
        event.container_event_type,
        ContainerEventType::ContainerDeletedEvent as i32
    );
    assert_eq!(event.reason, "ContainerDeleted");
}

// ── Version ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_version() {
    let svc = make_test_service();
    let resp = svc
        .version(Request::new(VersionRequest {
            version: "0.1.0".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.runtime_name, "a3s-box");
    assert_eq!(resp.runtime_api_version, "v1");
    assert!(!resp.runtime_version.is_empty());
}

// ── Status ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_status() {
    let svc = make_test_service();
    let resp = svc
        .status(Request::new(StatusRequest { verbose: false }))
        .await
        .unwrap()
        .into_inner();

    let status = resp.status.unwrap();
    assert_eq!(status.conditions.len(), 2);
    assert!(status
        .conditions
        .iter()
        .any(|c| c.r#type == "RuntimeReady" && c.status));
    assert!(status
        .conditions
        .iter()
        .any(|c| c.r#type == "NetworkReady" && c.status));
    assert!(resp.info.is_empty());
}

#[tokio::test]
async fn test_status_verbose_info() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-2");
    sandbox.state = SandboxState::NotReady;
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store.sandboxes.add(sandbox).await;

    let mut running = test_container("c-running", "sb-1");
    running.state = ContainerState::Running;
    running.started_at = 2_000_000_000;
    let mut exited = test_container("c-exited", "sb-2");
    exited.state = ContainerState::Exited;
    exited.finished_at = 3_000_000_000;
    exited.exit_code = 42;

    svc.store
        .containers
        .add(test_container("c-created", "sb-1"))
        .await;
    svc.store.containers.add(running).await;
    svc.store.containers.add(exited).await;

    let resp = svc
        .status(Request::new(StatusRequest { verbose: true }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.info.get("sandbox_count"), Some(&"2".to_string()));
    assert_eq!(resp.info.get("sandbox_ready_count"), Some(&"1".to_string()));
    assert_eq!(
        resp.info.get("sandbox_not_ready_count"),
        Some(&"1".to_string())
    );
    assert_eq!(resp.info.get("container_count"), Some(&"3".to_string()));
    assert_eq!(
        resp.info.get("container_created_count"),
        Some(&"1".to_string())
    );
    assert_eq!(
        resp.info.get("container_running_count"),
        Some(&"1".to_string())
    );
    assert_eq!(
        resp.info.get("container_exited_count"),
        Some(&"1".to_string())
    );
    assert_eq!(resp.info.get("vm_manager_count"), Some(&"0".to_string()));
    assert_eq!(
        resp.info.get("warm_pool_enabled"),
        Some(&"false".to_string())
    );
}

// ── UpdateRuntimeConfig ──────────────────────────────────────────

#[tokio::test]
async fn test_update_runtime_config() {
    let svc = make_test_service();
    let result = svc
        .update_runtime_config(Request::new(UpdateRuntimeConfigRequest {
            runtime_config: None,
        }))
        .await;
    assert!(result.is_ok());
}

// ── Pod Sandbox Status / List ────────────────────────────────────

#[tokio::test]
async fn test_pod_sandbox_status_not_found() {
    let svc = make_test_service();
    let result = svc
        .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
            pod_sandbox_id: "nonexistent".to_string(),
            verbose: false,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_pod_sandbox_status_found() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let resp = svc
        .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
            pod_sandbox_id: "sb-1".to_string(),
            verbose: false,
        }))
        .await
        .unwrap()
        .into_inner();

    let status = resp.status.unwrap();
    assert_eq!(status.id, "sb-1");
    assert_eq!(status.state(), PodSandboxState::SandboxReady);
    let meta = status.metadata.unwrap();
    assert_eq!(meta.name, "pod-sb-1");
    assert_eq!(meta.namespace, "default");
}

#[tokio::test]
async fn test_pod_sandbox_status_reports_network_ips() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-1");
    sandbox.network_ip = "10.244.0.12".to_string();
    sandbox.additional_ips = vec!["fd00::12".to_string()];
    svc.store.sandboxes.add(sandbox).await;

    let resp = svc
        .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
            pod_sandbox_id: "sb-1".to_string(),
            verbose: true,
        }))
        .await
        .unwrap()
        .into_inner();

    let network = resp.status.unwrap().network.unwrap();
    assert_eq!(network.ip, "10.244.0.12");
    assert_eq!(network.additional_ips.len(), 1);
    assert_eq!(network.additional_ips[0].ip, "fd00::12");
    assert_eq!(
        resp.info.get("network_ip"),
        Some(&"10.244.0.12".to_string())
    );
    assert_eq!(resp.info.get("additional_ip_count"), Some(&"1".to_string()));
}

#[tokio::test]
async fn test_pod_sandbox_status_verbose_info() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-1");
    sandbox.state = SandboxState::NotReady;
    svc.store.sandboxes.add(sandbox).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let resp = svc
        .pod_sandbox_status(Request::new(PodSandboxStatusRequest {
            pod_sandbox_id: "sb-1".to_string(),
            verbose: true,
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        resp.info.get("sandbox_state"),
        Some(&"not_ready".to_string())
    );
    assert_eq!(resp.info.get("vm_present"), Some(&"false".to_string()));
    assert_eq!(resp.info.get("container_count"), Some(&"1".to_string()));
}

#[tokio::test]
async fn test_list_pod_sandbox_empty() {
    let svc = make_test_service();
    let resp = svc
        .list_pod_sandbox(Request::new(ListPodSandboxRequest { filter: None }))
        .await
        .unwrap()
        .into_inner();
    assert!(resp.items.is_empty());
}

#[tokio::test]
async fn test_list_pod_sandbox_with_entries() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store.sandboxes.add(test_sandbox("sb-2")).await;

    let resp = svc
        .list_pod_sandbox(Request::new(ListPodSandboxRequest { filter: None }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.items.len(), 2);
}

#[tokio::test]
async fn test_list_pod_sandbox_filter_by_id() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store.sandboxes.add(test_sandbox("sb-2")).await;

    let resp = svc
        .list_pod_sandbox(Request::new(ListPodSandboxRequest {
            filter: Some(PodSandboxFilter {
                id: "sb-1".to_string(),
                state: 0,
                label_selector: HashMap::new(),
            }),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.items.len(), 1);
    assert_eq!(resp.items[0].id, "sb-1");
}

// ── Container CRUD ───────────────────────────────────────────────

#[tokio::test]
async fn test_create_container_sandbox_not_found() {
    let svc = make_test_service();
    let result = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "nonexistent".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "test".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                command: vec!["nginx".to_string()],
                args: vec!["-g".to_string(), "daemon off;".to_string()],
                working_dir: "/app".to_string(),
                envs: vec![KeyValue {
                    key: "ENV".to_string(),
                    value: "prod".to_string(),
                }],
                stdin: true,
                stdin_once: true,
                tty: true,
                linux: Some(LinuxContainerConfig {
                    security_context: Some(LinuxContainerSecurityContext {
                        run_as_user: Some(Int64Value { value: 1000 }),
                        run_as_group: Some(Int64Value { value: 1001 }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_create_container_missing_config() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let result = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: None,
            sandbox_config: None,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn test_create_container_missing_metadata() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let result = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: None,
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn test_create_container_requires_ready_sandbox() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-1");
    sandbox.state = SandboxState::NotReady;
    svc.store.sandboxes.add(sandbox).await;

    let result = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "test".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                command: vec!["nginx".to_string()],
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a ready sandbox"));
}

#[tokio::test]
async fn test_create_container_allows_multi_container_pod() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    put_test_oci_image(&svc.image_store, "nginx:latest").await;
    svc.store
        .containers
        .add(test_container("existing", "sb-1"))
        .await;

    let response = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "second-container".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                command: vec!["nginx".to_string()],
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner();

    let containers = svc.store.containers.list(Some("sb-1"), None).await;
    assert_eq!(containers.len(), 2);
    let created = svc
        .store
        .containers
        .get(&response.container_id)
        .await
        .unwrap();
    assert_eq!(created.name, "second-container");
    assert_ne!(created.id, "existing");
    assert!(created.rootfs_guest_path.contains(&created.id));
}

#[tokio::test]
async fn test_create_container_success() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    put_test_oci_image(&svc.image_store, "nginx:latest").await;

    let resp = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "my-container".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                command: vec!["nginx".to_string()],
                args: vec!["-g".to_string(), "daemon off;".to_string()],
                working_dir: "/app".to_string(),
                envs: vec![KeyValue {
                    key: "ENV".to_string(),
                    value: "prod".to_string(),
                }],
                stdin: true,
                stdin_once: true,
                tty: true,
                linux: Some(LinuxContainerConfig {
                    security_context: Some(LinuxContainerSecurityContext {
                        run_as_user: Some(Int64Value { value: 1000 }),
                        run_as_group: Some(Int64Value { value: 1001 }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(!resp.container_id.is_empty());

    // Verify container is in the store
    let c = svc.store.containers.get(&resp.container_id).await.unwrap();
    assert_eq!(c.name, "my-container");
    assert_eq!(c.sandbox_id, "sb-1");
    assert_eq!(c.state, ContainerState::Created);
    assert_eq!(c.resolved_image_digest, "sha256:imageconfigtest");
    assert!(!c.resolved_image_path.is_empty());
    assert!(!c.rootfs_path.is_empty());
    assert!(PathBuf::from(&c.rootfs_path).is_dir());
    assert!(c
        .rootfs_guest_path
        .starts_with(CRI_CONTAINER_ROOTFS_GUEST_BASE));
    assert!(PathBuf::from(&c.rootfs_path).join("tmp").is_dir());
    assert_eq!(c.command, vec!["nginx".to_string()]);
    assert_eq!(c.args, vec!["-g".to_string(), "daemon off;".to_string()]);
    assert_eq!(
        c.env,
        vec![
            (
                "PATH".to_string(),
                "/usr/local/bin:/usr/bin:/bin".to_string()
            ),
            ("ENV".to_string(), "prod".to_string()),
            // Non-privileged container: restricted to the default capability set.
            (
                "A3S_SEC_CAP_KEEP".to_string(),
                "AUDIT_WRITE,CHOWN,DAC_OVERRIDE,FOWNER,FSETID,KILL,MKNOD,\
                 NET_BIND_SERVICE,NET_RAW,SETFCAP,SETGID,SETPCAP,SETUID,SYS_CHROOT"
                    .to_string()
            ),
        ]
    );
    assert_eq!(c.working_dir, "/app");
    assert_eq!(c.user, Some("1000:1001".to_string()));
    assert!(c.stdin);
    assert!(c.stdin_once);
    assert!(c.tty);
}

#[tokio::test]
async fn test_create_container_materializes_readonly_mount() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    put_test_oci_image(&svc.image_store, "nginx:latest").await;

    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("nested")).unwrap();
    std::fs::write(source.path().join("config.txt"), b"mounted config").unwrap();
    std::fs::write(source.path().join("nested").join("extra.txt"), b"extra").unwrap();

    let resp = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "mounted".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                command: vec!["nginx".to_string()],
                mounts: vec![Mount {
                    container_path: "/etc/a3s-config".to_string(),
                    host_path: source.path().to_string_lossy().to_string(),
                    readonly: true,
                    selinux_relabel: false,
                    propagation: crate::cri_api::mount::MountPropagation::PropagationPrivate as i32,
                }],
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner();

    let container = svc.store.containers.get(&resp.container_id).await.unwrap();
    assert_eq!(container.mounts.len(), 1);
    let mounted_file = PathBuf::from(&container.rootfs_path)
        .join("etc")
        .join("a3s-config")
        .join("config.txt");
    let mounted_nested = PathBuf::from(&container.rootfs_path)
        .join("etc")
        .join("a3s-config")
        .join("nested")
        .join("extra.txt");
    assert_eq!(
        std::fs::read_to_string(mounted_file).unwrap(),
        "mounted config"
    );
    assert_eq!(std::fs::read_to_string(mounted_nested).unwrap(), "extra");

    let status = svc
        .container_status(Request::new(ContainerStatusRequest {
            container_id: resp.container_id,
            verbose: true,
        }))
        .await
        .unwrap()
        .into_inner();
    let status = status.status.unwrap();
    assert_eq!(status.mounts.len(), 1);
    assert_eq!(status.mounts[0].container_path, "/etc/a3s-config");
}

#[tokio::test]
async fn test_create_container_materializes_writable_mount() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    put_test_oci_image(&svc.image_store, "nginx:latest").await;

    // Mirrors the CRI volume conformance: a writable mount with selinux_relabel
    // set. Both are now accepted (relabel is a no-op on this non-SELinux
    // runtime); the source is materialized by copy into the rootfs so the
    // host-created file is visible in the container.
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("testVolume.file"), b"data").unwrap();

    let resp = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "mounted".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "nginx:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                command: vec!["nginx".to_string()],
                mounts: vec![Mount {
                    container_path: "/data".to_string(),
                    host_path: source.path().to_string_lossy().to_string(),
                    readonly: false,
                    selinux_relabel: true,
                    propagation: crate::cri_api::mount::MountPropagation::PropagationPrivate as i32,
                }],
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner();

    let container = svc.store.containers.get(&resp.container_id).await.unwrap();
    assert_eq!(container.mounts.len(), 1);
    let mounted_file = PathBuf::from(&container.rootfs_path)
        .join("data")
        .join("testVolume.file");
    assert_eq!(std::fs::read_to_string(mounted_file).unwrap(), "data");
}

#[test]
fn test_parse_localhost_seccomp_deny_allow_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("block-chmod.json");
    std::fs::write(
        &path,
        r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["chmod","fchmodat"],"action":"SCMP_ACT_ERRNO"}]}"#,
    )
    .unwrap();
    let deny = super::parse_localhost_seccomp_deny(path.to_str().unwrap()).unwrap();
    assert_eq!(deny, vec!["chmod".to_string(), "fchmodat".to_string()]);
}

#[test]
fn test_parse_localhost_seccomp_deny_rejects_deny_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deny-default.json");
    std::fs::write(&path, r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[]}"#).unwrap();
    // Deny-by-default profiles need a full allow-list; not supported -> Err so
    // the caller falls back to RuntimeDefault rather than running unconfined.
    assert!(super::parse_localhost_seccomp_deny(path.to_str().unwrap()).is_err());
}

#[tokio::test]
async fn test_create_container_requires_pulled_image() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let result = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "missing-image".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "example.com/missing:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                command: vec!["/bin/true".to_string()],
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
    assert!(err.message().contains("Image not found locally"));
    assert!(svc
        .store
        .containers
        .list(Some("sb-1"), None)
        .await
        .is_empty());
}

#[tokio::test]
async fn test_create_container_uses_image_defaults() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    put_test_oci_image(&svc.image_store, "example.com/app:latest").await;

    let resp = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "my-container".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "example.com/app:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                envs: vec![
                    KeyValue {
                        key: "ENV".to_string(),
                        value: "cri".to_string(),
                    },
                    KeyValue {
                        key: "EXTRA".to_string(),
                        value: "1".to_string(),
                    },
                ],
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner();

    let c = svc.store.containers.get(&resp.container_id).await.unwrap();
    assert_eq!(c.resolved_image_digest, "sha256:imageconfigtest");
    assert!(!c.resolved_image_path.is_empty());
    assert!(!c.rootfs_path.is_empty());
    assert!(PathBuf::from(&c.rootfs_path).is_dir());
    assert!(c
        .rootfs_guest_path
        .starts_with(CRI_CONTAINER_ROOTFS_GUEST_BASE));
    assert_eq!(c.command, vec!["/usr/local/bin/app".to_string()]);
    assert_eq!(c.args, vec!["serve".to_string()]);
    assert_eq!(
        c.env,
        vec![
            (
                "PATH".to_string(),
                "/usr/local/bin:/usr/bin:/bin".to_string()
            ),
            ("ENV".to_string(), "cri".to_string()),
            ("EXTRA".to_string(), "1".to_string()),
        ]
    );
    assert_eq!(c.working_dir, "/image");
    assert_eq!(c.user, Some("2000:2000".to_string()));
}

#[tokio::test]
async fn test_create_then_start_container_uses_image_defaults_and_rootfs() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    put_test_oci_image(&svc.image_store, "example.com/app:latest").await;

    let resp = svc
        .create_container(Request::new(CreateContainerRequest {
            pod_sandbox_id: "sb-1".to_string(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "image-defaults".to_string(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: "example.com/app:latest".to_string(),
                    annotations: HashMap::new(),
                }),
                ..Default::default()
            }),
            sandbox_config: None,
        }))
        .await
        .unwrap()
        .into_inner();

    let container = svc.store.containers.get(&resp.container_id).await.unwrap();
    assert_eq!(container.command, vec!["/usr/local/bin/app".to_string()]);
    assert_eq!(container.args, vec!["serve".to_string()]);
    assert!(PathBuf::from(&container.rootfs_path).is_dir());
    assert!(container
        .rootfs_guest_path
        .starts_with(CRI_CONTAINER_ROOTFS_GUEST_BASE));

    let expected_cmd = vec!["/usr/local/bin/app".to_string(), "serve".to_string()];
    let expected_env = vec![
        "PATH=/usr/local/bin:/usr/bin:/bin".to_string(),
        "ENV=image".to_string(),
    ];
    let expected_rootfs = container.rootfs_guest_path.clone();
    let Some(exec_server) = spawn_exec_stream_server_with_assert(
        b"ready\n",
        b"",
        0,
        Duration::from_millis(100),
        move |request| {
            assert_eq!(request.cmd.as_slice(), expected_cmd.as_slice());
            assert_eq!(request.env.as_slice(), expected_env.as_slice());
            assert_eq!(request.working_dir.as_deref(), Some("/image"));
            assert_eq!(request.user.as_deref(), Some("2000:2000"));
            assert_eq!(request.rootfs.as_deref(), Some(expected_rootfs.as_str()));
        },
    )
    .await
    else {
        return;
    };

    let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.start_container(Request::new(StartContainerRequest {
        container_id: resp.container_id.clone(),
    }))
    .await
    .unwrap();

    let running = svc.store.containers.get(&resp.container_id).await.unwrap();
    assert_eq!(running.state, ContainerState::Running);
    assert!(running.started_at > 0);

    let exited = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let container = svc.store.containers.get(&resp.container_id).await.unwrap();
            if container.state == ContainerState::Exited {
                break container;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("container should exit under background supervision");

    assert_eq!(exited.exit_code, 0);
    assert!(exited.finished_at >= exited.started_at);
}

#[tokio::test]
async fn test_start_container_not_found() {
    let svc = make_test_service();
    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "nonexistent".to_string(),
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_start_container_supports_tty_workload() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    let mut container = test_container("c-1", "sb-1");
    container.command = vec!["/bin/sh".to_string()];
    container.args = vec![];
    container.stdin = true;
    container.tty = true;
    svc.store.containers.add(container).await;

    let Some(pty_server) =
        spawn_pty_stream_server_with_assert(b"tty ready\n", 0, Duration::from_secs(1), |request| {
            assert_eq!(request.cmd, vec!["/bin/sh".to_string()]);
            assert_eq!(
                request.rootfs.as_deref(),
                Some("/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs")
            );
        })
        .await
    else {
        return;
    };
    let vm = attach_ready_test_vm("sb-1", &pty_server.exec_socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.start_container(Request::new(StartContainerRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Running);
    assert!(svc.attach_streams.read().await.contains_key("c-1"));
    assert!(svc.workload_stdins.read().await.contains_key("c-1"));
    assert_eq!(
        pty_server.pty_socket_path,
        pty_server.exec_socket_path.with_file_name("pty.sock")
    );

    let attach = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: false,
            tty: true,
            stdout: true,
            stderr: false,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(attach.url.contains("/attach/"));
}

#[tokio::test]
async fn test_start_container_registers_non_tty_stdin_handle() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    let mut container = test_container("c-1", "sb-1");
    container.command = vec!["cat".to_string()];
    container.args = vec![];
    container.stdin = true;
    container.stdin_once = true;
    svc.store.containers.add(container).await;

    let Some(exec_server) =
        spawn_exec_stream_server_with_assert(b"", b"", 0, Duration::from_secs(1), |request| {
            assert!(request.stdin_streaming);
        })
        .await
    else {
        return;
    };
    let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.start_container(Request::new(StartContainerRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();

    let running = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(running.state, ContainerState::Running);
    assert!(svc.attach_streams.read().await.contains_key("c-1"));
    assert!(svc.workload_stdins.read().await.contains_key("c-1"));
    assert!(svc.workload_stops.read().await.contains_key("c-1"));
}

#[tokio::test]
async fn test_start_container_requires_resolved_image_metadata() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.resolved_image_digest.clear();
    container.resolved_image_path.clear();
    svc.store.containers.add(container).await;

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("without resolved image metadata"));

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Created);
    assert_eq!(c.started_at, 0);
}

#[tokio::test]
async fn test_start_container_requires_resolved_image_path() {
    let svc = make_test_service();
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().join("missing-image");
    let mut container = test_container("c-1", "sb-1");
    container.resolved_image_path = missing_path.to_string_lossy().to_string();
    svc.store.containers.add(container).await;

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("Resolved image path"));

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Created);
    assert_eq!(c.started_at, 0);
}

#[tokio::test]
async fn test_start_container_requires_prepared_rootfs_metadata() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.rootfs_path.clear();
    container.rootfs_guest_path.clear();
    svc.store.containers.add(container).await;

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("without prepared rootfs metadata"));

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Created);
    assert_eq!(c.started_at, 0);
}

#[tokio::test]
async fn test_start_container_requires_prepared_rootfs_path() {
    let svc = make_test_service();
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().join("missing-rootfs");
    let mut container = test_container("c-1", "sb-1");
    container.rootfs_path = missing_path.to_string_lossy().to_string();
    svc.store.containers.add(container).await;

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("Prepared rootfs"));

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Created);
    assert_eq!(c.started_at, 0);
}

#[tokio::test]
async fn test_start_container_rejects_already_running() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.state = ContainerState::Running;
    container.started_at = 2_000_000_000;
    svc.store.containers.add(container).await;

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("already running"));
}

#[tokio::test]
async fn test_start_container_rejects_exited_container() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.state = ContainerState::Exited;
    container.finished_at = 3_000_000_000;
    svc.store.containers.add(container).await;

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("already exited"));
}

#[tokio::test]
async fn test_start_container_requires_running_sandbox_vm() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("VM not found"));

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Created);
    assert_eq!(c.started_at, 0);
}

#[tokio::test]
async fn test_start_container_requires_ready_sandbox_vm() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .start_container(Request::new(StartContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("VM is not ready"));

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Created);
    assert_eq!(c.started_at, 0);
}

#[tokio::test]
async fn test_start_container_transitions_running_then_exited() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    let log_dir = tempfile::tempdir().unwrap();
    let log_path = log_dir.path().join("container.log");
    let mut container = test_container("c-1", "sb-1");
    container.command = vec!["/bin/test-app".to_string()];
    container.args = vec!["serve".to_string()];
    container.log_path = log_path.to_string_lossy().to_string();
    svc.store.containers.add(container).await;

    let Some(exec_server) =
        spawn_exec_stream_server(b"booted\n", b"warn\n", 17, Duration::from_millis(100)).await
    else {
        return;
    };
    let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.start_container(Request::new(StartContainerRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();

    let running = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(running.state, ContainerState::Running);
    assert!(running.started_at > 0);
    assert_eq!(running.finished_at, 0);
    assert!(svc.attach_streams.read().await.contains_key("c-1"));

    let exited = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let container = svc.store.containers.get("c-1").await.unwrap();
            if container.state == ContainerState::Exited {
                break container;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("container should exit under background supervision");

    assert_eq!(exited.exit_code, 17);
    assert!(exited.finished_at >= exited.started_at);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !svc.attach_streams.read().await.contains_key("c-1") {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("attach stream should be removed after workload exit");

    let log = tokio::fs::read_to_string(&log_path).await.unwrap();
    assert!(log.contains(" stdout F booted\n"));
    assert!(log.contains(" stderr F warn\n"));
}

#[tokio::test]
async fn test_start_container_supervises_multiple_containers_in_same_sandbox() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    let log_dir = tempfile::tempdir().unwrap();

    let mut first = test_container("c-1", "sb-1");
    first.command = vec!["app-one".to_string()];
    first.args = vec![];
    first.log_path = log_dir
        .path()
        .join("first.log")
        .to_string_lossy()
        .to_string();
    svc.store.containers.add(first).await;

    let mut second = test_container("c-2", "sb-1");
    second.command = vec!["app-two".to_string()];
    second.args = vec![];
    second.log_path = log_dir
        .path()
        .join("second.log")
        .to_string_lossy()
        .to_string();
    svc.store.containers.add(second).await;

    let Some(exec_server) = spawn_multi_exec_stream_server(vec![
        ("app-one", b"one\n", 0, Duration::from_millis(100)),
        ("app-two", b"two\n", 2, Duration::from_millis(50)),
    ])
    .await
    else {
        return;
    };
    let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.start_container(Request::new(StartContainerRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();
    svc.start_container(Request::new(StartContainerRequest {
        container_id: "c-2".to_string(),
    }))
    .await
    .unwrap();

    assert!(svc.attach_streams.read().await.contains_key("c-1"));
    assert!(svc.attach_streams.read().await.contains_key("c-2"));

    let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let first = svc.store.containers.get("c-1").await.unwrap();
            let second = svc.store.containers.get("c-2").await.unwrap();
            if first.state == ContainerState::Exited && second.state == ContainerState::Exited {
                break (first, second);
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both containers should exit under independent supervision");

    assert_eq!(first.exit_code, 0);
    assert_eq!(second.exit_code, 2);
    assert!(first.finished_at >= first.started_at);
    assert!(second.finished_at >= second.started_at);

    let first_log = tokio::fs::read_to_string(&first.log_path).await.unwrap();
    let second_log = tokio::fs::read_to_string(&second.log_path).await.unwrap();
    assert!(first_log.contains(" stdout F one\n"));
    assert!(second_log.contains(" stdout F two\n"));
}

#[tokio::test]
async fn test_stop_container() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.stop_container(Request::new(StopContainerRequest {
        container_id: "c-1".to_string(),
        timeout: 0,
    }))
    .await
    .unwrap();

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Exited);
    assert!(c.finished_at > 0);
    assert_eq!(c.exit_code, 137);
    assert!(!svc.vm_managers.read().await.contains_key("sb-1"));

    let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sandbox.state, SandboxState::NotReady);
}

#[tokio::test]
async fn test_stop_container_stops_workload_without_tearing_down_sandbox_vm() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    let mut container = test_container("c-1", "sb-1");
    container.command = vec!["sleep".to_string(), "60".to_string()];
    svc.store.containers.add(container).await;

    let Some(exec_server) = spawn_cancelable_exec_stream_server().await else {
        return;
    };
    let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.start_container(Request::new(StartContainerRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();
    assert!(svc.workload_stops.read().await.contains_key("c-1"));

    svc.stop_container(Request::new(StopContainerRequest {
        container_id: "c-1".to_string(),
        timeout: 1,
    }))
    .await
    .unwrap();

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Exited);
    assert_eq!(c.exit_code, 137);
    assert!(c.finished_at > 0);
    assert!(svc.vm_managers.read().await.contains_key("sb-1"));
    assert!(!svc.workload_stops.read().await.contains_key("c-1"));
    assert!(!svc.attach_streams.read().await.contains_key("c-1"));

    let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sandbox.state, SandboxState::Ready);
}

#[tokio::test]
async fn test_stop_container_refuses_vm_teardown_with_other_running_containers() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .add(test_container("c-2", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    svc.store
        .containers
        .mark_started("c-2", 2_000_000_001)
        .await;

    let result = svc
        .stop_container(Request::new(StopContainerRequest {
            container_id: "c-1".to_string(),
            timeout: 0,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("other containers"));
    let first = svc.store.containers.get("c-1").await.unwrap();
    let second = svc.store.containers.get("c-2").await.unwrap();
    assert_eq!(first.state, ContainerState::Running);
    assert_eq!(second.state, ContainerState::Running);
    let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sandbox.state, SandboxState::Ready);
}

#[tokio::test]
async fn test_stop_container_not_found() {
    let svc = make_test_service();

    let result = svc
        .stop_container(Request::new(StopContainerRequest {
            container_id: "missing".to_string(),
            timeout: 0,
        }))
        .await;

    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_stop_container_preserves_exited_state() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.state = ContainerState::Exited;
    container.finished_at = 3_000_000_000;
    container.exit_code = 42;
    svc.store.containers.add(container).await;

    svc.stop_container(Request::new(StopContainerRequest {
        container_id: "c-1".to_string(),
        timeout: 0,
    }))
    .await
    .unwrap();

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Exited);
    assert_eq!(c.finished_at, 3_000_000_000);
    assert_eq!(c.exit_code, 42);
}

#[tokio::test]
async fn test_stop_container_created_does_not_stop_sandbox() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.stop_container(Request::new(StopContainerRequest {
        container_id: "c-1".to_string(),
        timeout: 0,
    }))
    .await
    .unwrap();

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Exited);
    assert_eq!(c.exit_code, 0);

    let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sandbox.state, SandboxState::Ready);
    assert!(svc.vm_managers.read().await.contains_key("sb-1"));
}

#[tokio::test]
async fn test_stop_container_running_without_vm_reconciles_state() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    svc.stop_container(Request::new(StopContainerRequest {
        container_id: "c-1".to_string(),
        timeout: 0,
    }))
    .await
    .unwrap();

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Exited);
    assert_eq!(c.exit_code, 137);

    let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sandbox.state, SandboxState::NotReady);
}

#[tokio::test]
async fn test_stop_container_running_disconnects_network_endpoint() {
    let svc = make_test_service();
    let mut sandbox = test_networked_sandbox("sb-1");
    add_test_network_endpoint(&svc, &mut sandbox);
    svc.store.sandboxes.add(sandbox).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    svc.stop_container(Request::new(StopContainerRequest {
        container_id: "c-1".to_string(),
        timeout: 0,
    }))
    .await
    .unwrap();

    let network = svc.network_store.get("cri-net").unwrap().unwrap();
    assert!(network.endpoints.is_empty());

    let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sandbox.state, SandboxState::NotReady);
}

#[tokio::test]
async fn test_remove_container() {
    let svc = make_test_service();
    let rootfs_path = svc
        .container_rootfs_base()
        .join("sb-1")
        .join("c-1")
        .join("rootfs");
    std::fs::create_dir_all(&rootfs_path).unwrap();
    let mut container = test_container("c-1", "sb-1");
    container.rootfs_path = rootfs_path.to_string_lossy().to_string();
    svc.store.containers.add(container).await;

    svc.remove_container(Request::new(RemoveContainerRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();

    assert!(svc.store.containers.get("c-1").await.is_none());
    assert!(!rootfs_path.exists());
}

#[tokio::test]
async fn test_remove_container_missing_is_idempotent() {
    let svc = make_test_service();

    let result = svc
        .remove_container(Request::new(RemoveContainerRequest {
            container_id: "missing".to_string(),
        }))
        .await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_remove_container_force_stops_running_container() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    // CRI RemoveContainer force-removes: a running container is stopped first,
    // then deleted (no VM manager in the test, so the stop reconciles it).
    let result = svc
        .remove_container(Request::new(RemoveContainerRequest {
            container_id: "c-1".to_string(),
        }))
        .await;

    assert!(result.is_ok());
    assert!(svc.store.containers.get("c-1").await.is_none());
}

// ── Container Status ─────────────────────────────────────────────

#[tokio::test]
async fn test_container_status_not_found() {
    let svc = make_test_service();
    let result = svc
        .container_status(Request::new(ContainerStatusRequest {
            container_id: "nonexistent".to_string(),
            verbose: false,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_container_status_created() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let resp = svc
        .container_status(Request::new(ContainerStatusRequest {
            container_id: "c-1".to_string(),
            verbose: false,
        }))
        .await
        .unwrap()
        .into_inner();

    let status = resp.status.unwrap();
    assert_eq!(status.id, "c-1");
    assert_eq!(
        status.state(),
        crate::cri_api::ContainerState::ContainerCreated
    );
    assert_eq!(status.image_ref, "sha256:test");
    assert!(resp.info.is_empty());
}

#[tokio::test]
async fn test_container_status_running() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    let resp = svc
        .container_status(Request::new(ContainerStatusRequest {
            container_id: "c-1".to_string(),
            verbose: false,
        }))
        .await
        .unwrap()
        .into_inner();

    let status = resp.status.unwrap();
    assert_eq!(
        status.state(),
        crate::cri_api::ContainerState::ContainerRunning
    );
    assert_eq!(status.started_at, 2_000_000_000);
}

#[tokio::test]
async fn test_container_status_exited_success_reason() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_exited("c-1", 3_000_000_000, 0)
        .await;

    let resp = svc
        .container_status(Request::new(ContainerStatusRequest {
            container_id: "c-1".to_string(),
            verbose: false,
        }))
        .await
        .unwrap()
        .into_inner();

    let status = resp.status.unwrap();
    assert_eq!(
        status.state(),
        crate::cri_api::ContainerState::ContainerExited
    );
    assert_eq!(status.reason, "Completed");
    assert_eq!(status.message, "Container exited successfully");
    assert_eq!(status.exit_code, 0);
}

#[tokio::test]
async fn test_container_status_exited_error_reason() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_exited("c-1", 3_000_000_000, 42)
        .await;

    let resp = svc
        .container_status(Request::new(ContainerStatusRequest {
            container_id: "c-1".to_string(),
            verbose: false,
        }))
        .await
        .unwrap()
        .into_inner();

    let status = resp.status.unwrap();
    assert_eq!(
        status.state(),
        crate::cri_api::ContainerState::ContainerExited
    );
    assert_eq!(status.reason, "Error");
    assert_eq!(status.message, "Container exited with code 42");
    assert_eq!(status.exit_code, 42);
}

#[tokio::test]
async fn test_container_status_verbose_info() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.tty = true;
    container.stdin = true;
    container.stdin_once = true;
    svc.store.containers.add(container).await;

    let resp = svc
        .container_status(Request::new(ContainerStatusRequest {
            container_id: "c-1".to_string(),
            verbose: true,
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        resp.info.get("container_state"),
        Some(&"created".to_string())
    );
    assert_eq!(resp.info.get("sandbox_id"), Some(&"sb-1".to_string()));
    assert_eq!(
        resp.info.get("image_ref"),
        Some(&"nginx:latest".to_string())
    );
    assert_eq!(
        resp.info.get("resolved_image_digest"),
        Some(&"sha256:test".to_string())
    );
    assert_eq!(resp.info.get("resolved_image_path"), Some(&"/".to_string()));
    assert_eq!(resp.info.get("rootfs_path"), Some(&"/".to_string()));
    assert_eq!(
        resp.info.get("rootfs_guest_path"),
        Some(&"/run/a3s/cri/container-rootfs/sb-1/c-1/rootfs".to_string())
    );
    assert_eq!(resp.info.get("vm_present"), Some(&"false".to_string()));
    assert_eq!(resp.info.get("command_count"), Some(&"1".to_string()));
    assert_eq!(resp.info.get("arg_count"), Some(&"2".to_string()));
    assert_eq!(resp.info.get("env_count"), Some(&"1".to_string()));
    assert_eq!(resp.info.get("tty"), Some(&"true".to_string()));
    assert_eq!(resp.info.get("stdin"), Some(&"true".to_string()));
    assert_eq!(resp.info.get("stdin_once"), Some(&"true".to_string()));
}

// ── List Containers ──────────────────────────────────────────────

#[tokio::test]
async fn test_list_containers_empty() {
    let svc = make_test_service();
    let resp = svc
        .list_containers(Request::new(ListContainersRequest { filter: None }))
        .await
        .unwrap()
        .into_inner();
    assert!(resp.containers.is_empty());
}

#[tokio::test]
async fn test_list_containers_filter_by_sandbox() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .add(test_container("c-2", "sb-1"))
        .await;
    svc.store
        .containers
        .add(test_container("c-3", "sb-2"))
        .await;

    let resp = svc
        .list_containers(Request::new(ListContainersRequest {
            filter: Some(ContainerFilter {
                id: String::new(),
                state: None,
                pod_sandbox_id: "sb-1".to_string(),
                label_selector: HashMap::new(),
            }),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.containers.len(), 2);
}

#[tokio::test]
async fn test_list_containers_filter_by_id() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .add(test_container("c-2", "sb-1"))
        .await;

    let resp = svc
        .list_containers(Request::new(ListContainersRequest {
            filter: Some(ContainerFilter {
                id: "c-1".to_string(),
                state: None,
                pod_sandbox_id: String::new(),
                label_selector: HashMap::new(),
            }),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.containers.len(), 1);
    assert_eq!(resp.containers[0].id, "c-1");
}

#[tokio::test]
async fn test_list_containers_filter_by_state() {
    let svc = make_test_service();
    let mut running = test_container("c-running", "sb-1");
    running.state = ContainerState::Running;
    running.started_at = 2_000_000_000;

    let mut exited = test_container("c-exited", "sb-1");
    exited.state = ContainerState::Exited;
    exited.finished_at = 3_000_000_000;
    exited.exit_code = 7;

    svc.store
        .containers
        .add(test_container("c-created", "sb-1"))
        .await;
    svc.store.containers.add(running).await;
    svc.store.containers.add(exited).await;

    let resp = svc
        .list_containers(Request::new(ListContainersRequest {
            filter: Some(ContainerFilter {
                id: String::new(),
                state: Some(ContainerStateValue {
                    state: crate::cri_api::ContainerState::ContainerRunning as i32,
                }),
                pod_sandbox_id: String::new(),
                label_selector: HashMap::new(),
            }),
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.containers.len(), 1);
    assert_eq!(resp.containers[0].id, "c-running");
    assert_eq!(
        resp.containers[0].state(),
        crate::cri_api::ContainerState::ContainerRunning
    );
}

#[tokio::test]
async fn test_list_containers_filter_by_label_selector() {
    let svc = make_test_service();
    let mut api = test_container("c-api", "sb-1");
    api.labels.insert("app".to_string(), "api".to_string());
    api.labels.insert("tier".to_string(), "backend".to_string());

    let mut worker = test_container("c-worker", "sb-1");
    worker
        .labels
        .insert("app".to_string(), "worker".to_string());
    worker
        .labels
        .insert("tier".to_string(), "backend".to_string());

    svc.store.containers.add(api).await;
    svc.store.containers.add(worker).await;

    let resp = svc
        .list_containers(Request::new(ListContainersRequest {
            filter: Some(ContainerFilter {
                id: String::new(),
                state: None,
                pod_sandbox_id: String::new(),
                label_selector: HashMap::from([
                    ("app".to_string(), "api".to_string()),
                    ("tier".to_string(), "backend".to_string()),
                ]),
            }),
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.containers.len(), 1);
    assert_eq!(resp.containers[0].id, "c-api");
    assert_eq!(
        resp.containers[0].labels.get("app"),
        Some(&"api".to_string())
    );
}

// ── UpdateContainerResources ─────────────────────────────────────

#[tokio::test]
async fn test_update_container_resources_not_found() {
    let svc = make_test_service();
    let result = svc
        .update_container_resources(Request::new(UpdateContainerResourcesRequest {
            container_id: "nonexistent".to_string(),
            linux: None,
            annotations: HashMap::new(),
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_update_container_resources_no_linux() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let result = svc
        .update_container_resources(Request::new(UpdateContainerResourcesRequest {
            container_id: "c-1".to_string(),
            linux: None,
            annotations: HashMap::new(),
        }))
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_update_container_resources_requires_running_container() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let result = svc
        .update_container_resources(Request::new(UpdateContainerResourcesRequest {
            container_id: "c-1".to_string(),
            linux: Some(LinuxContainerResources {
                cpu_quota: 100_000,
                ..Default::default()
            }),
            annotations: HashMap::new(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a running container"));
}

#[tokio::test]
async fn test_update_container_resources_rejects_exited_container() {
    let svc = make_test_service();
    let mut container = test_container("c-1", "sb-1");
    container.state = ContainerState::Exited;
    container.finished_at = 3_000_000_000;
    container.exit_code = 42;
    svc.store.containers.add(container).await;

    let result = svc
        .update_container_resources(Request::new(UpdateContainerResourcesRequest {
            container_id: "c-1".to_string(),
            linux: Some(LinuxContainerResources {
                cpu_quota: 100_000,
                ..Default::default()
            }),
            annotations: HashMap::new(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a running container"));
}

#[tokio::test]
async fn test_update_container_resources_requires_ready_vm() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .update_container_resources(Request::new(UpdateContainerResourcesRequest {
            container_id: "c-1".to_string(),
            linux: Some(LinuxContainerResources {
                cpu_quota: 100_000,
                ..Default::default()
            }),
            annotations: HashMap::new(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("VM is not ready"));
}

#[tokio::test]
async fn test_update_container_resources_linux_rejected() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    let result = svc
        .update_container_resources(Request::new(UpdateContainerResourcesRequest {
            container_id: "c-1".to_string(),
            linux: Some(LinuxContainerResources {
                cpu_quota: 100_000,
                memory_limit_in_bytes: 1024 * 1024 * 512,
                ..Default::default()
            }),
            annotations: HashMap::new(),
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::Unimplemented);
}

// ── ReopenContainerLog ───────────────────────────────────────────

#[tokio::test]
async fn test_reopen_container_log_not_found() {
    let svc = make_test_service();
    let result = svc
        .reopen_container_log(Request::new(ReopenContainerLogRequest {
            container_id: "nonexistent".to_string(),
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_reopen_container_log_empty_path() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    // Should succeed even with empty log path (no-op)
    let result = svc
        .reopen_container_log(Request::new(ReopenContainerLogRequest {
            container_id: "c-1".to_string(),
        }))
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_reopen_container_log_signals_supervisor() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    // Register a reopen handle as StartContainer does for a running container.
    // The actual file reopen happens in the exit supervisor (integration-tested
    // via critest); here we verify the RPC fires the per-container request and
    // returns once the supervisor acks. Pre-arm `done` so the now-synchronous
    // RPC does not block waiting for a supervisor that isn't running here.
    let request = std::sync::Arc::new(tokio::sync::Notify::new());
    let done = std::sync::Arc::new(tokio::sync::Notify::new());
    done.notify_one();
    svc.log_reopens.write().await.insert(
        "c-1".to_string(),
        super::LogReopenHandle {
            request: request.clone(),
            done: done.clone(),
        },
    );

    svc.reopen_container_log(Request::new(ReopenContainerLogRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();

    // The RPC fired the per-container reopen request (notify_one stores a
    // permit, so notified() resolves immediately).
    tokio::time::timeout(std::time::Duration::from_secs(1), request.notified())
        .await
        .expect("ReopenContainerLog should signal the container's reopen request");
}

// ── Stop/Remove Pod Sandbox (store-only, no VM) ──────────────────

#[tokio::test]
async fn test_stop_pod_sandbox_no_vm() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
        pod_sandbox_id: "sb-1".to_string(),
    }))
    .await
    .unwrap();

    // Sandbox should be NotReady
    let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sb.state, SandboxState::NotReady);

    // Container should be Exited
    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Exited);
    assert_eq!(c.exit_code, 137);
}

#[tokio::test]
async fn test_stop_pod_sandbox_uses_workload_stop_controls_for_running_containers() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .add(test_container("c-2", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    svc.store
        .containers
        .mark_started("c-2", 2_000_000_001)
        .await;

    let (first_stop_tx, first_stop_rx) = tokio::sync::oneshot::channel();
    let (second_stop_tx, second_stop_rx) = tokio::sync::oneshot::channel();
    {
        let mut stops = svc.workload_stops.write().await;
        stops.insert("c-1".to_string(), first_stop_tx);
        stops.insert("c-2".to_string(), second_stop_tx);
    }

    let store = svc.store.clone();
    tokio::spawn(async move {
        first_stop_rx.await.unwrap();
        sleep(Duration::from_millis(25)).await;
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        store
            .mark_container_exited_if_running("c-1", now_ns, 143, false)
            .await;
    });

    let store = svc.store.clone();
    tokio::spawn(async move {
        second_stop_rx.await.unwrap();
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        store
            .mark_container_exited_if_running("c-2", now_ns, 144, false)
            .await;
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await
    })
    .await
    .expect("StopPodSandbox should wait for workload stop controls")
    .unwrap();

    let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sb.state, SandboxState::NotReady);

    let first = svc.store.containers.get("c-1").await.unwrap();
    let second = svc.store.containers.get("c-2").await.unwrap();
    assert_eq!(first.state, ContainerState::Exited);
    assert_eq!(second.state, ContainerState::Exited);
    assert_eq!(first.exit_code, 143);
    assert_eq!(second.exit_code, 144);
    assert!(!svc.workload_stops.read().await.contains_key("c-1"));
    assert!(!svc.workload_stops.read().await.contains_key("c-2"));
}

#[tokio::test]
async fn test_stop_pod_sandbox_disconnects_network_endpoint() {
    let svc = make_test_service();
    let mut sandbox = test_networked_sandbox("sb-1");
    add_test_network_endpoint(&svc, &mut sandbox);
    svc.store.sandboxes.add(sandbox).await;

    svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
        pod_sandbox_id: "sb-1".to_string(),
    }))
    .await
    .unwrap();

    let network = svc.network_store.get("cri-net").unwrap().unwrap();
    assert!(network.endpoints.is_empty());

    let sandbox = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sandbox.state, SandboxState::NotReady);
}

#[tokio::test]
async fn test_stop_pod_sandbox_removes_vm_manager() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.stop_pod_sandbox(Request::new(StopPodSandboxRequest {
        pod_sandbox_id: "sb-1".to_string(),
    }))
    .await
    .unwrap();

    assert!(!svc.vm_managers.read().await.contains_key("sb-1"));

    let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sb.state, SandboxState::NotReady);

    let c = svc.store.containers.get("c-1").await.unwrap();
    assert_eq!(c.state, ContainerState::Exited);
    assert_eq!(c.exit_code, 137);
}

#[tokio::test]
async fn test_stop_pod_sandbox_not_found() {
    let svc = make_test_service();

    let result = svc
        .stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: "missing".to_string(),
        }))
        .await;

    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_stop_pod_sandbox_not_ready_is_idempotent() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-1");
    sandbox.state = SandboxState::NotReady;
    svc.store.sandboxes.add(sandbox).await;

    let result = svc
        .stop_pod_sandbox(Request::new(StopPodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await;

    assert!(result.is_ok());
    let sb = svc.store.sandboxes.get("sb-1").await.unwrap();
    assert_eq!(sb.state, SandboxState::NotReady);
}

#[tokio::test]
async fn test_remove_pod_sandbox_no_vm() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-1");
    sandbox.state = SandboxState::NotReady;
    svc.store.sandboxes.add(sandbox).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
        pod_sandbox_id: "sb-1".to_string(),
    }))
    .await
    .unwrap();

    // Sandbox and containers should be gone
    assert!(svc.store.sandboxes.get("sb-1").await.is_none());
    assert!(svc.store.containers.get("c-1").await.is_none());
}

#[tokio::test]
async fn test_remove_pod_sandbox_disconnects_network_endpoint() {
    let svc = make_test_service();
    let mut sandbox = test_networked_sandbox("sb-1");
    sandbox.state = SandboxState::NotReady;
    add_test_network_endpoint(&svc, &mut sandbox);
    svc.store.sandboxes.add(sandbox).await;

    svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
        pod_sandbox_id: "sb-1".to_string(),
    }))
    .await
    .unwrap();

    let network = svc.network_store.get("cri-net").unwrap().unwrap();
    assert!(network.endpoints.is_empty());
    assert!(svc.store.sandboxes.get("sb-1").await.is_none());
}

#[tokio::test]
async fn test_remove_pod_sandbox_removes_lingering_vm_manager() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-1");
    sandbox.state = SandboxState::NotReady;
    svc.store.sandboxes.add(sandbox).await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
        pod_sandbox_id: "sb-1".to_string(),
    }))
    .await
    .unwrap();

    assert!(!svc.vm_managers.read().await.contains_key("sb-1"));
    assert!(svc.store.sandboxes.get("sb-1").await.is_none());
}

#[tokio::test]
async fn test_remove_pod_sandbox_missing_is_idempotent() {
    let svc = make_test_service();

    let result = svc
        .remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
            pod_sandbox_id: "missing".to_string(),
        }))
        .await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_remove_pod_sandbox_rejects_ready_sandbox() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let result = svc
        .remove_pod_sandbox(Request::new(RemovePodSandboxRequest {
            pod_sandbox_id: "sb-1".to_string(),
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a stopped sandbox"));
    assert!(svc.store.sandboxes.get("sb-1").await.is_some());
    assert!(svc.store.containers.get("c-1").await.is_some());
}

// ── Exec/Attach/PortForward error paths ──────────────────────────

#[tokio::test]
async fn test_exec_sync_container_not_found() {
    let svc = make_test_service();
    let result = svc
        .exec_sync(Request::new(ExecSyncRequest {
            container_id: "nonexistent".to_string(),
            cmd: vec!["ls".to_string()],
            timeout: 0,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_exec_sync_sandbox_not_found() {
    let svc = make_test_service();
    // Container exists but no VM for its sandbox
    svc.store
        .containers
        .add(test_container("c-1", "sb-missing"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    let result = svc
        .exec_sync(Request::new(ExecSyncRequest {
            container_id: "c-1".to_string(),
            cmd: vec!["ls".to_string()],
            timeout: 0,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_exec_sync_rejects_empty_command() {
    let svc = make_test_service();
    let result = svc
        .exec_sync(Request::new(ExecSyncRequest {
            container_id: "c-1".to_string(),
            cmd: vec![],
            timeout: 0,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("at least one argument"));
}

#[tokio::test]
async fn test_exec_sync_requires_running_container() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let result = svc
        .exec_sync(Request::new(ExecSyncRequest {
            container_id: "c-1".to_string(),
            cmd: vec!["ls".to_string()],
            timeout: 0,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a running container"));
}

#[tokio::test]
async fn test_exec_sync_requires_ready_vm() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .exec_sync(Request::new(ExecSyncRequest {
            container_id: "c-1".to_string(),
            cmd: vec!["ls".to_string()],
            timeout: 0,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("VM is not ready"));
}

#[tokio::test]
async fn test_exec_container_not_found() {
    let svc = make_test_service();
    let result = svc
        .exec(Request::new(ExecRequest {
            container_id: "nonexistent".to_string(),
            cmd: vec!["sh".to_string()],
            tty: false,
            stdin: false,
            stdout: true,
            stderr: true,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_exec_rejects_empty_command() {
    let svc = make_test_service();
    let result = svc
        .exec(Request::new(ExecRequest {
            container_id: "c-1".to_string(),
            cmd: vec![],
            tty: false,
            stdin: false,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("at least one argument"));
}

#[tokio::test]
async fn test_exec_requires_running_container() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let result = svc
        .exec(Request::new(ExecRequest {
            container_id: "c-1".to_string(),
            cmd: vec!["sh".to_string()],
            tty: false,
            stdin: false,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a running container"));
}

#[tokio::test]
async fn test_exec_requires_ready_vm() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .exec(Request::new(ExecRequest {
            container_id: "c-1".to_string(),
            cmd: vec!["sh".to_string()],
            tty: false,
            stdin: false,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("VM is not ready"));
}

#[tokio::test]
async fn test_attach_container_not_found() {
    let svc = make_test_service();
    let result = svc
        .attach(Request::new(AttachRequest {
            container_id: "nonexistent".to_string(),
            stdin: false,
            tty: false,
            stdout: true,
            stderr: true,
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_attach_rejects_without_streams() {
    let svc = make_test_service();
    let result = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: false,
            tty: false,
            stdout: false,
            stderr: false,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("at least one stream"));
}

#[tokio::test]
async fn test_attach_stdin_requires_container_stdin_enabled() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    let result = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: true,
            tty: false,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("stdin enabled"));
}

#[tokio::test]
async fn test_attach_rejects_tty_mismatch() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;

    let result = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: false,
            tty: true,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("TTY flag must match"));
}

#[tokio::test]
async fn test_attach_requires_running_container() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;

    let result = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: false,
            tty: false,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a running container"));
}

#[tokio::test]
async fn test_attach_requires_ready_vm() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: false,
            tty: false,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("VM is not ready"));
}

#[tokio::test]
async fn test_attach_requires_active_workload_stream() {
    let svc = make_test_service();
    svc.store
        .containers
        .add(test_container("c-1", "sb-1"))
        .await;
    svc.store
        .containers
        .mark_started("c-1", 2_000_000_000)
        .await;
    let Some(exec_server) = spawn_exec_stream_server(b"", b"", 0, Duration::from_secs(1)).await
    else {
        return;
    };
    let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: false,
            tty: false,
            stdout: true,
            stderr: true,
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("active workload stream"));
}

#[tokio::test]
async fn test_attach_stdin_once_consumes_workload_stdin_handle() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    let mut container = test_container("c-1", "sb-1");
    container.command = vec!["cat".to_string()];
    container.args = vec![];
    container.stdin = true;
    container.stdin_once = true;
    svc.store.containers.add(container).await;

    let Some(exec_server) =
        spawn_exec_stream_server_with_assert(b"", b"", 0, Duration::from_secs(1), |request| {
            assert!(request.stdin_streaming);
        })
        .await
    else {
        return;
    };
    let vm = attach_ready_test_vm("sb-1", &exec_server.socket_path).await;
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    svc.start_container(Request::new(StartContainerRequest {
        container_id: "c-1".to_string(),
    }))
    .await
    .unwrap();
    assert!(svc.workload_stdins.read().await.contains_key("c-1"));

    let response = svc
        .attach(Request::new(AttachRequest {
            container_id: "c-1".to_string(),
            stdin: true,
            tty: false,
            stdout: true,
            stderr: true,
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(response.url.contains("/attach/"));
    assert!(!svc.workload_stdins.read().await.contains_key("c-1"));
}

#[tokio::test]
async fn test_port_forward_sandbox_not_found() {
    let svc = make_test_service();
    let result = svc
        .port_forward(Request::new(PortForwardRequest {
            pod_sandbox_id: "nonexistent".to_string(),
            port: vec![8080],
        }))
        .await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn test_port_forward_empty_ports_rejected_when_sandbox_declares_none() {
    // critest passes the port in the RPC; `crictl port-forward` sends none and
    // we fall back to the sandbox's declared ports. A ready sandbox that
    // declares no ports therefore has nothing to forward to.
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let result = svc
        .port_forward(Request::new(PortForwardRequest {
            pod_sandbox_id: "sb-1".to_string(),
            port: vec![],
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("declares none"));
}

#[tokio::test]
async fn test_port_forward_rejects_multiple_ports() {
    let svc = make_test_service();
    let result = svc
        .port_forward(Request::new(PortForwardRequest {
            pod_sandbox_id: "sb-1".to_string(),
            port: vec![8080, 9090],
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unimplemented);
    assert!(err.message().contains("exactly one port"));
}

#[tokio::test]
async fn test_port_forward_requires_ready_sandbox() {
    let svc = make_test_service();
    let mut sandbox = test_sandbox("sb-1");
    sandbox.state = SandboxState::NotReady;
    svc.store.sandboxes.add(sandbox).await;

    let result = svc
        .port_forward(Request::new(PortForwardRequest {
            pod_sandbox_id: "sb-1".to_string(),
            port: vec![8080],
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("requires a ready sandbox"));
}

#[tokio::test]
async fn test_port_forward_requires_ready_vm() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;
    let vm = VmManager::with_box_id(
        a3s_box_core::config::BoxConfig::default(),
        EventEmitter::new(16),
        "sb-1".to_string(),
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .port_forward(Request::new(PortForwardRequest {
            pod_sandbox_id: "sb-1".to_string(),
            port: vec![8080],
        }))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("VM is not ready"));
}

#[tokio::test]
async fn test_port_forward_registers_session_for_recovered_ready_vm() {
    let svc = make_test_service();
    svc.store.sandboxes.add(test_sandbox("sb-1")).await;

    let tmp = tempfile::tempdir().unwrap();
    let exec_socket_path = tmp.path().join("exec.sock");
    let vm = attach_ready_test_vm("sb-1", &exec_socket_path).await;
    assert_eq!(
        vm.port_forward_socket_path(),
        Some(exec_socket_path.with_file_name("portfwd.sock").as_path())
    );
    svc.vm_managers.write().await.insert("sb-1".to_string(), vm);

    let result = svc
        .port_forward(Request::new(PortForwardRequest {
            pod_sandbox_id: "sb-1".to_string(),
            port: vec![8080],
        }))
        .await
        .unwrap();

    let url = result.into_inner().url;
    assert!(url.starts_with("http://127.0.0.1:0/portforward/"));
}

// ── Warm Pool ────────────────────────────────────────────────────

#[test]
fn test_service_without_warm_pool_has_none() {
    let svc = make_test_service();
    assert!(svc.warm_pool.is_none());
}

#[tokio::test]
async fn test_with_warm_pool_attaches_pool() {
    use a3s_box_core::config::{BoxConfig, PoolConfig};
    use a3s_box_core::event::EventEmitter;
    use a3s_box_runtime::pool::WarmPool;

    let pool_config = PoolConfig {
        enabled: true,
        min_idle: 0, // no pre-boot in tests
        max_size: 2,
        idle_ttl_secs: 300,
        ..Default::default()
    };

    let result = WarmPool::start(pool_config, BoxConfig::default(), EventEmitter::new(64)).await;

    if let Ok(pool) = result {
        let svc = make_test_service().with_warm_pool(pool);

        assert!(svc.warm_pool.is_some());
        // Drain pool to clean up
        if let Some(p) = svc.warm_pool {
            let mut pool = p.write().await;
            let _ = pool.drain().await;
        }
    }
    // If WarmPool::start fails (no shim), test is skipped — acceptable in unit test env
}

#[tokio::test]
async fn test_acquire_vm_without_pool_fails_without_shim() {
    // Without a warm pool, CRI sandbox acquisition cold-boots and fails in unit test env.
    let svc = make_test_service();
    let config = a3s_box_core::config::BoxConfig::default();
    let result = svc
        .acquire_vm_with_box_id(config, "test-acquire".to_string())
        .await;
    // Expected: error because no shim binary available
    assert!(result.is_err());
}
