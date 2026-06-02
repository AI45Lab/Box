use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use a3s_box_core::PORT_FWD_VSOCK_PORT;
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use tracing::{debug, info, warn};

const HOST_CID: u32 = 2;
const ENV_WINDOWS_ENABLED: &str = "BOX_WINDOWS_PORT_FWD";
const ENV_CRI_ENABLED: &str = "BOX_CRI_PORT_FWD";

const FRAME_OPEN: u8 = 1;
const FRAME_OPEN_ACK: u8 = 2;
const FRAME_DATA: u8 = 3;
const FRAME_CLOSE: u8 = 4;

type SharedWriter = Arc<Mutex<std::fs::File>>;
type StreamMap = Arc<Mutex<HashMap<u32, TcpStream>>>;

pub fn run_port_forward_client() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var(ENV_WINDOWS_ENABLED).as_deref() == Ok("1") {
        return run_windows_port_forward_client();
    }

    if std::env::var(ENV_CRI_ENABLED).as_deref() == Ok("1") {
        return run_cri_port_forward_server();
    }

    Ok(())
}

fn run_windows_port_forward_client() -> Result<(), Box<dyn std::error::Error>> {
    let mut backoff = Duration::from_millis(250);
    loop {
        match connect_control() {
            Ok(control) => {
                info!(
                    host_cid = HOST_CID,
                    host_port = PORT_FWD_VSOCK_PORT,
                    "Windows port-forward control channel connected"
                );
                backoff = Duration::from_millis(250);
                if let Err(err) = serve_control(control) {
                    warn!(error = %err, "Windows port-forward control channel dropped");
                }
            }
            Err(err) => {
                warn!(
                    error = %err,
                    retry_ms = backoff.as_millis(),
                    "Windows port-forward control connect failed"
                );
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_cri_port_forward_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    info!(
        guest_port = PORT_FWD_VSOCK_PORT,
        "Starting CRI port-forward server"
    );

    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;

    unsafe {
        libc::fcntl(sock_fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, PORT_FWD_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(4)?)?;

    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                std::thread::spawn(move || {
                    let file = std::fs::File::from(client);
                    if let Err(err) = serve_control(file) {
                        warn!(error = %err, "CRI port-forward control connection dropped");
                    }
                });
            }
            Err(err) => {
                warn!(error = %err, "CRI port-forward accept failed");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run_cri_port_forward_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("CRI port-forward server not available on non-Linux guest platform");
    Ok(())
}

fn connect_control() -> io::Result<std::fs::File> {
    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(io::Error::other)?;

    // Set CLOEXEC manually since SOCK_CLOEXEC isn't available in nix 0.29 on macOS
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let addr = VsockAddr::new(HOST_CID, PORT_FWD_VSOCK_PORT);
    connect(fd.as_raw_fd(), &addr).map_err(io::Error::other)?;

    let owned: OwnedFd = fd;
    Ok(std::fs::File::from(owned))
}

fn serve_control(control: std::fs::File) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(control.try_clone()?));
    let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
    let mut reader = control;

    loop {
        if !wait_readable(reader.as_raw_fd(), Duration::from_secs(1))? {
            continue;
        }

        let frame = match read_frame(&mut reader)? {
            Some(frame) => frame,
            None => {
                debug!("pf: serve_control read EOF, connection closing");
                return Ok(());
            }
        };
        debug!(
            kind = frame.kind,
            stream_id = frame.stream_id,
            payload_len = frame.payload.len(),
            "pf: serve_control received frame"
        );

        match frame.kind {
            FRAME_OPEN => {
                if frame.payload.len() != 2 {
                    write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    continue;
                }

                let guest_port = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                match TcpStream::connect(("127.0.0.1", guest_port)) {
                    Ok(stream) => {
                        let _ = stream.set_nodelay(true);
                        let read_stream = stream.try_clone()?;
                        debug!(
                            stream_id = frame.stream_id,
                            guest_port,
                            peer = ?stream.peer_addr().ok(),
                            local = ?stream.local_addr().ok(),
                            "pf: connected guest target, spawned reader"
                        );
                        streams.lock().unwrap().insert(frame.stream_id, stream);
                        spawn_guest_reader(
                            frame.stream_id,
                            read_stream,
                            writer.clone(),
                            streams.clone(),
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[0])?;
                    }
                    Err(err) => {
                        debug!(
                            error = %err,
                            stream_id = frame.stream_id,
                            guest_port,
                            "Failed to connect guest TCP target"
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    }
                }
            }
            FRAME_DATA => {
                let mut remove = false;
                {
                    let mut guard = streams.lock().unwrap();
                    if let Some(stream) = guard.get_mut(&frame.stream_id) {
                        match stream
                            .write_all(&frame.payload)
                            .and_then(|_| stream.flush())
                        {
                            Ok(()) => debug!(
                                stream_id = frame.stream_id,
                                len = frame.payload.len(),
                                "pf: wrote client data to guest target"
                            ),
                            Err(err) => {
                                warn!(stream_id = frame.stream_id, error = %err, "pf: write to guest target failed");
                                remove = true;
                            }
                        }
                    } else {
                        warn!(stream_id = frame.stream_id, "pf: DATA for unknown stream");
                        continue;
                    }
                }
                if remove {
                    close_stream(frame.stream_id, &streams);
                    let _ = write_frame(&writer, FRAME_CLOSE, frame.stream_id, &[]);
                }
            }
            FRAME_CLOSE => {
                close_stream(frame.stream_id, &streams);
            }
            _ => {
                debug!(kind = frame.kind, "Ignoring unknown port-forward frame");
            }
        }
    }
}

fn wait_readable(fd: i32, timeout: Duration) -> io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().clamp(0, i32::MAX as u128) as i32;
    let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc > 0 && (pollfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "vsock connection closed",
        ));
    }
    Ok(rc > 0 && (pollfd.revents & libc::POLLIN) != 0)
}

fn spawn_guest_reader(
    stream_id: u32,
    mut stream: TcpStream,
    writer: SharedWriter,
    streams: StreamMap,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        debug!(stream_id, "pf: guest reader thread started");
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    debug!(stream_id, "pf: guest target read EOF");
                    break;
                }
                Ok(n) => {
                    debug!(stream_id, n, "pf: read from guest target -> FRAME_DATA");
                    if write_frame(&writer, FRAME_DATA, stream_id, &buf[..n]).is_err() {
                        warn!(stream_id, "pf: relay FRAME_DATA to host failed");
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    warn!(stream_id, error = %err, "pf: guest target read error");
                    break;
                }
            }
        }

        debug!(stream_id, "pf: guest reader thread exiting");
        close_stream(stream_id, &streams);
        let _ = write_frame(&writer, FRAME_CLOSE, stream_id, &[]);
    });
}

fn close_stream(stream_id: u32, streams: &StreamMap) {
    if let Some(stream) = streams.lock().unwrap().remove(&stream_id) {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

struct Frame {
    kind: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

fn read_frame(reader: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; 9];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }

    let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload)?;
    }

    Ok(Some(Frame {
        kind: header[0],
        stream_id: u32::from_be_bytes([header[1], header[2], header[3], header[4]]),
        payload,
    }))
}

fn write_frame(writer: &SharedWriter, kind: u8, stream_id: u32, payload: &[u8]) -> io::Result<()> {
    let mut guard = writer.lock().unwrap();
    guard.write_all(&[kind])?;
    guard.write_all(&stream_id.to_be_bytes())?;
    guard.write_all(&(payload.len() as u32).to_be_bytes())?;
    if !payload.is_empty() {
        guard.write_all(payload)?;
    }
    guard.flush()
}
