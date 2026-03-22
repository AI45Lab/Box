//! Pure-Rust userspace network proxy for libkrun on macOS.
//!
//! Uses a Unix datagram `socketpair()` to connect libkrun's virtio-net backend
//! to the userspace gateway and provides the gateway services needed by the guest:
//!
//! - **ARP**: handled automatically by smoltcp's interface layer.
//! - **DNS**: UDP/53 queries forwarded to the host's configured DNS servers.
//! - **Inbound TCP port-forwarding**: `host_port → guest_ip:guest_port` pairs
//!   parsed from the box's `port_map` config (e.g. `"8088:80"`).
//!
//! Outbound NAT (VM → internet) is Phase 2 and not included here.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::Duration;

use a3s_box_core::error::{BoxError, Result};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the current time as a smoltcp `Instant` (microseconds since Unix epoch).
fn smoltcp_now() -> Instant {
    use std::time::{SystemTime, UNIX_EPOCH};
    let us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    Instant::from_micros(us)
}

/// Convert `std::net::Ipv4Addr` to `smoltcp::wire::Ipv4Address`.
fn to_smoltcp_ipv4(ip: Ipv4Addr) -> Ipv4Address {
    Ipv4Address::from(ip)
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// MAC address we assign to the virtual gateway interface.
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Maximum Ethernet frame size (header + MTU).
const MAX_FRAME: usize = 1514;
/// Ephemeral port range start for outbound TCP connections from the gateway.
const EPHEMERAL_BASE: u16 = 49152;

// ── smoltcp phy::Device ───────────────────────────────────────────────────────

/// smoltcp physical-layer device backed by a connected Unix datagram socket.
///
/// Frames from the VM arrive via `recv()` and are queued in `rx_queue`.
/// smoltcp reads them through `receive()`. Frames smoltcp wants to transmit
/// are sent directly to the peer via `transmit()` / `TxToken::consume()`.
///
/// The socket MUST be connected to the peer (via `UnixDatagram::connect`) before
/// use so that `send()` works without a destination address. On macOS, using
/// `send_to()` on a socket whose peer has called `connect()` to us causes
/// ECONNRESET / EDESTADDRREQ in the peer's receive path.
struct UnixgramDevice {
    socket: UnixDatagram,
    rx_queue: VecDeque<Vec<u8>>,
}

impl UnixgramDevice {
    /// Drain the socket into `rx_queue` (non-blocking, batch up to 64 frames).
    fn drain(&mut self) {
        let mut buf = vec![0u8; MAX_FRAME];
        for _ in 0..64 {
            match self.socket.recv(&mut buf) {
                Ok(n) => {
                    tracing::trace!(
                        bytes = n,
                        "NetProxy received ethernet frame from guest/libkrun"
                    );
                    self.rx_queue.push_back(buf[..n].to_vec())
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(error = %e, "NetProxy: recv from libkrun failed");
                    break;
                }
            }
        }
    }
}

/// Owned received frame — consumed by smoltcp's interface layer.
struct OwnedRxToken(Vec<u8>);

impl smoltcp::phy::RxToken for OwnedRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.0)
    }
}

/// Transmit token — smoltcp writes a frame into `buf`, which we then send.
///
/// The socket must already be connected to the peer so `send()` works without
/// an explicit destination address.
struct TxToken {
    socket: UnixDatagram,
}

impl smoltcp::phy::TxToken for TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        tracing::trace!(
            bytes = len,
            "NetProxy sending ethernet frame to guest/libkrun"
        );
        if let Err(e) = self.socket.send(&buf) {
            tracing::warn!(error = %e, len, "NetProxy: send to libkrun failed");
        }
        result
    }
}

impl smoltcp::phy::Device for UnixgramDevice {
    type RxToken<'a>
        = OwnedRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken
    where
        Self: 'a;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx_queue.pop_front()?;
        let tx = TxToken {
            socket: self.socket.try_clone().ok()?,
        };
        Some((OwnedRxToken(frame), tx))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            socket: self.socket.try_clone().ok()?,
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        caps
    }
}

// ── Port-forward state ────────────────────────────────────────────────────────

/// Parsed port-forward rule: `host_port → guest_ip:guest_port`.
struct PortForward {
    listener: TcpListener,
    guest_ip: Ipv4Addr,
    guest_port: u16,
    /// TCP handshake in progress: (smoltcp handle, host TcpStream).
    pending: Vec<(smoltcp::iface::SocketHandle, TcpStream)>,
    /// Fully established connections ready for data proxying.
    active: Vec<(smoltcp::iface::SocketHandle, TcpStream)>,
}

// ── Proxy engine ──────────────────────────────────────────────────────────────

struct ProxyEngine {
    device: UnixgramDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dns_handle: smoltcp::iface::SocketHandle,
    dns_servers: Vec<Ipv4Addr>,
    port_forwards: Vec<PortForward>,
    next_ephemeral: u16,
    shutdown: Arc<AtomicBool>,
}

impl ProxyEngine {
    fn new(
        socket: UnixDatagram,
        gateway_ip: Ipv4Addr,
        prefix_len: u8,
        dns_servers: Vec<Ipv4Addr>,
        port_forwards: Vec<PortForward>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let mut device = UnixgramDevice {
            socket,
            rx_queue: VecDeque::new(),
        };

        // Configure smoltcp interface as the gateway.
        let config = Config::new(GATEWAY_MAC.into());
        let mut iface = Interface::new(config, &mut device, smoltcp_now());
        iface.update_ip_addrs(|addrs| {
            let cidr = IpCidr::new(IpAddress::Ipv4(to_smoltcp_ipv4(gateway_ip)), prefix_len);
            addrs.push(cidr).ok();
        });

        let mut sockets = SocketSet::new(vec![]);

        // DNS socket: listens on UDP/53 on the gateway IP.
        let dns_rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 8192]);
        let dns_tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 8192]);
        let mut dns_socket = udp::Socket::new(dns_rx, dns_tx);
        dns_socket.bind(53).ok();
        let dns_handle = sockets.add(dns_socket);

        Self {
            device,
            iface,
            sockets,
            dns_handle,
            dns_servers,
            port_forwards,
            next_ephemeral: EPHEMERAL_BASE,
            shutdown,
        }
    }

    fn run(&mut self) {
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let now = smoltcp_now();

            // 1. Drain UnixGram socket into rx_queue.
            self.device.drain();

            // 2. Accept new host connections on port-forward listeners.
            self.accept_connections(now);

            // 3. Poll smoltcp (processes ARP, TCP, UDP frames).
            self.iface.poll(now, &mut self.device, &mut self.sockets);

            // 4. Promote pending TCP connections to active once established.
            self.promote_established();

            // 5. Proxy data for active TCP connections.
            self.proxy_data();

            // 6. Forward DNS queries to real DNS servers.
            self.forward_dns();

            // 7. Remove closed connections and release their smoltcp sockets.
            self.cleanup();

            // 8. Sleep until the next smoltcp event or at most 5 ms.
            let delay = self
                .iface
                .poll_delay(now, &self.sockets)
                .unwrap_or(smoltcp::time::Duration::from_millis(1));
            std::thread::sleep(Duration::from_micros(delay.micros().min(5_000)));
        }
    }

    // ── Accept new host connections ───────────────────────────────────────────

    fn accept_connections(&mut self, now: Instant) {
        // First pass: accept connections, collect (forward_index, stream, guest_ip, guest_port).
        // We can't call open_guest_tcp while mutably borrowing port_forwards.
        let mut new_conns: Vec<(usize, TcpStream, Ipv4Addr, u16)> = Vec::new();
        for (i, pf) in self.port_forwards.iter_mut().enumerate() {
            loop {
                match pf.listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(true).ok();
                        stream.set_nodelay(true).ok();
                        new_conns.push((i, stream, pf.guest_ip, pf.guest_port));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "Port-forward listener error");
                        break;
                    }
                }
            }
        }

        // Second pass: open smoltcp TCP sockets and push to pending.
        for (i, stream, guest_ip, guest_port) in new_conns {
            let handle = self.open_guest_tcp(guest_ip, guest_port, now);
            self.port_forwards[i].pending.push((handle, stream));
            tracing::debug!(
                guest = %guest_ip,
                port = guest_port,
                handle = ?handle,
                "NetProxy accepted host connection and initiated guest TCP connect"
            );
        }
    }

    /// Create a smoltcp TCP socket and initiate a connection to the guest.
    fn open_guest_tcp(
        &mut self,
        guest_ip: Ipv4Addr,
        guest_port: u16,
        _now: Instant,
    ) -> smoltcp::iface::SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let mut socket = tcp::Socket::new(rx, tx);

        let local_port = self.next_ephemeral;
        self.next_ephemeral = self.next_ephemeral.wrapping_add(1);
        if self.next_ephemeral < EPHEMERAL_BASE {
            self.next_ephemeral = EPHEMERAL_BASE;
        }

        let remote = IpEndpoint::new(IpAddress::Ipv4(to_smoltcp_ipv4(guest_ip)), guest_port);
        socket
            .connect(self.iface.context(), remote, local_port)
            .ok();
        socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));

        self.sockets.add(socket)
    }

    // ── Promote pending → active ──────────────────────────────────────────────

    fn promote_established(&mut self) {
        for pf in &mut self.port_forwards {
            let mut still_pending = Vec::new();
            for (handle, stream) in pf.pending.drain(..) {
                let socket = self.sockets.get::<tcp::Socket>(handle);
                use smoltcp::socket::tcp::State;
                match socket.state() {
                    State::Established => {
                        tracing::debug!(handle = ?handle, "NetProxy guest TCP connection established");
                        pf.active.push((handle, stream));
                    }
                    State::Closed | State::TimeWait | State::CloseWait => {
                        tracing::debug!(handle = ?handle, state = ?socket.state(), "NetProxy guest TCP connection closed before establishment");
                        // Connection failed; close host side
                        drop(stream);
                        self.sockets.remove(handle);
                    }
                    _ => {
                        still_pending.push((handle, stream));
                    }
                }
            }
            pf.pending = still_pending;
        }
    }

    // ── Bidirectional data proxy ──────────────────────────────────────────────

    fn proxy_data(&mut self) {
        for pf in &mut self.port_forwards {
            for (handle, host_stream) in &mut pf.active {
                let socket = self.sockets.get_mut::<tcp::Socket>(*handle);

                // smoltcp → host (data received from guest)
                if socket.can_recv() {
                    socket
                        .recv(|data| {
                            tracing::trace!(handle = ?*handle, bytes = data.len(), "NetProxy forwarding guest -> host bytes");
                            let _ = host_stream.write_all(data);
                            (data.len(), ())
                        })
                        .ok();
                }

                // host → smoltcp (data from host curl/client)
                if socket.can_send() {
                    let mut buf = [0u8; 8192];
                    match host_stream.read(&mut buf) {
                        Ok(0) => {
                            tracing::debug!(handle = ?*handle, "NetProxy host side closed connection");
                            socket.close();
                        }
                        Ok(n) => {
                            tracing::trace!(handle = ?*handle, bytes = n, "NetProxy forwarding host -> guest bytes");
                            socket.send_slice(&buf[..n]).ok();
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            socket.close();
                        }
                    }
                }
            }
        }
    }

    // ── DNS forwarding ────────────────────────────────────────────────────────

    fn forward_dns(&mut self) {
        let dns_server = match self.dns_servers.first() {
            Some(s) => *s,
            None => return,
        };

        let socket = self.sockets.get_mut::<udp::Socket>(self.dns_handle);
        if !socket.can_recv() {
            return;
        }
        let (query, src_endpoint) = match socket.recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        let query = query.to_vec();
        let src = src_endpoint;

        // Forward query to the real DNS server via a host UDP socket.
        match UdpSocket::bind("0.0.0.0:0") {
            Ok(udp) => {
                udp.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let dest = SocketAddrV4::new(dns_server, 53);
                if udp.send_to(&query, dest).is_ok() {
                    let mut resp = vec![0u8; 4096];
                    if let Ok((n, _)) = udp.recv_from(&mut resp) {
                        let socket = self.sockets.get_mut::<udp::Socket>(self.dns_handle);
                        socket.send_slice(&resp[..n], src).ok();
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "DNS forward: failed to bind host UDP socket");
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────

    fn cleanup(&mut self) {
        use smoltcp::socket::tcp::State;
        for pf in &mut self.port_forwards {
            // Collect handles that need to be removed first, then remove outside retain.
            let mut to_remove = Vec::new();
            pf.active.retain(|(handle, _stream)| {
                let state = self.sockets.get::<tcp::Socket>(*handle).state();
                match state {
                    State::Closed | State::TimeWait | State::CloseWait => {
                        to_remove.push(*handle);
                        false
                    }
                    _ => true,
                }
            });
            for h in to_remove {
                self.sockets.remove(h);
            }
        }
    }
}

// ── NetProxyManager lifecycle ─────────────────────────────────────────────────

/// Manages the lifecycle of the pure-Rust vfkit network proxy thread.
///
/// Drop calls `stop()` automatically.
pub struct NetProxyManager {
    socket_path: PathBuf,
    net_socket_fd: Option<RawFd>,
    net_proxy_fd: Option<RawFd>,
    shutdown: Option<Arc<AtomicBool>>,
    worker: Option<JoinHandle<()>>,
}

impl NetProxyManager {
    /// Create a new manager. Socket will be placed at
    /// `~/.a3s/boxes/<box_id>/sockets/net.sock`.
    pub fn new(box_dir: &Path) -> Self {
        Self {
            socket_path: box_dir.join("sockets").join("net.sock"),
            net_socket_fd: None,
            net_proxy_fd: None,
            shutdown: None,
            worker: None,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn net_socket_fd(&self) -> Option<RawFd> {
        self.net_socket_fd
    }

    pub fn net_proxy_fd(&self) -> Option<RawFd> {
        self.net_proxy_fd
    }

    /// Spawn the proxy thread.
    ///
    /// `port_map` entries have the Docker format `"host_port:guest_port"`
    /// (e.g. `"8088:80"`). Guest IP is taken from `ip`.
    pub fn spawn(
        &mut self,
        ip: Ipv4Addr,
        gateway: Ipv4Addr,
        prefix_len: u8,
        dns_servers: &[Ipv4Addr],
        port_map: &[String],
    ) -> Result<()> {
        let (proxy_socket, krun_fd) = socketpair_unixgram()?;
        let port_forwards = parse_port_forwards(port_map, ip)
            .map_err(|e| BoxError::NetworkError(format!("invalid port_map: {}", e)))?;
        proxy_socket
            .set_nonblocking(true)
            .map_err(|e| BoxError::NetworkError(format!("failed to configure netproxy socket: {}", e)))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let dns_servers = dns_servers.to_vec();
        let worker_shutdown = shutdown.clone();
        let worker = std::thread::Builder::new()
            .name("a3s-netproxy".to_string())
            .spawn(move || {
                tracing::info!(gateway = %gateway, guest_ip = %ip, "NetProxy thread started");
                let mut engine = ProxyEngine::new(
                    proxy_socket,
                    gateway,
                    prefix_len,
                    dns_servers,
                    port_forwards,
                    worker_shutdown,
                );
                engine.run();
                tracing::info!("NetProxy thread exiting");
            })
            .map_err(|e| BoxError::NetworkError(format!("failed to spawn netproxy thread: {}", e)))?;
        self.net_socket_fd = Some(krun_fd);
        self.net_proxy_fd = None;
        self.shutdown = Some(shutdown);
        self.worker = Some(worker);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.store(true, Ordering::Relaxed);
        }
        if let Some(fd) = self.net_socket_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
        if let Some(fd) = self.net_proxy_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        std::fs::remove_file(&self.socket_path).ok();
    }

    pub fn is_running(&mut self) -> bool {
        self.net_socket_fd.is_some() || self.net_proxy_fd.is_some()
    }
}

impl super::NetworkBackend for NetProxyManager {
    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn stop(&mut self) {
        self.stop();
    }
}

impl Drop for NetProxyManager {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn spawn_inherited_netproxy(
    fd: RawFd,
    guest_ip: Ipv4Addr,
    gateway: Ipv4Addr,
    prefix_len: u8,
    dns_servers: &[Ipv4Addr],
    port_map: &[String],
) -> Result<()> {
    let socket = unsafe { UnixDatagram::from_raw_fd(fd) };
    let port_forwards = parse_port_forwards(port_map, guest_ip)
        .map_err(|e| BoxError::NetworkError(format!("invalid port_map: {}", e)))?;
    let dns_servers = dns_servers.to_vec();
    let shutdown = Arc::new(AtomicBool::new(false));

    std::thread::Builder::new()
        .name("a3s-netproxy".to_string())
        .spawn(move || {
            tracing::info!(fd, gateway = %gateway, guest_ip = %guest_ip, "NetProxy thread started");
            if let Err(e) = socket.set_nonblocking(true) {
                tracing::error!(error = %e, "NetProxy: set_nonblocking failed");
                return;
            }

            let mut engine = ProxyEngine::new(
                socket,
                gateway,
                prefix_len,
                dns_servers,
                port_forwards,
                shutdown,
            );
            engine.run();
            tracing::info!("NetProxy thread exiting");
        })
        .map_err(|e| BoxError::NetworkError(format!("failed to spawn netproxy thread: {}", e)))?;

    Ok(())
}

fn socketpair_unixgram() -> Result<(UnixDatagram, RawFd)> {
    let mut fds = [-1; 2];
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(BoxError::NetworkError(format!(
            "failed to create unix datagram socketpair: {}",
            io::Error::last_os_error()
        )));
    }

    let proxy_socket = unsafe { UnixDatagram::from_raw_fd(fds[0]) };
    Ok((proxy_socket, fds[1]))
}

/// Parse `["8088:80", "443:443"]` into `Vec<PortForward>`.
///
/// Each rule maps `host_port → guest_ip:guest_port`. Guest IP is always the
/// IPAM-assigned `guest_ip`.
fn parse_port_forwards(
    port_map: &[String],
    guest_ip: Ipv4Addr,
) -> std::result::Result<Vec<PortForward>, String> {
    let mut forwards = Vec::new();
    for entry in port_map {
        // Strip protocol suffix: "8088:80/tcp" → "8088:80"
        let entry = entry.split('/').next().unwrap_or(entry);
        let (host_str, guest_str) = entry
            .split_once(':')
            .ok_or_else(|| format!("expected 'host_port:guest_port', got '{}'", entry))?;
        let host_port: u16 = host_str
            .parse()
            .map_err(|_| format!("invalid host port '{}'", host_str))?;
        let guest_port: u16 = guest_str
            .parse()
            .map_err(|_| format!("invalid guest port '{}'", guest_str))?;

        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, host_port))
            .map_err(|e| format!("cannot bind 0.0.0.0:{}: {}", host_port, e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking on listener: {}", e))?;

        tracing::info!(
            host_port,
            guest_port,
            guest_ip = %guest_ip,
            "Port-forward listener ready"
        );
        forwards.push(PortForward {
            listener,
            guest_ip,
            guest_port,
            pending: Vec::new(),
            active: Vec::new(),
        });
    }
    Ok(forwards)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn port_is_bindable(port: u16) -> bool {
        TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).is_ok()
    }

    #[test]
    fn test_netproxy_manager_new() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = NetProxyManager::new(dir.path());
        assert_eq!(
            mgr.socket_path(),
            dir.path().join("sockets").join("net.sock")
        );
        assert_eq!(mgr.net_socket_fd(), None);
    }

    #[test]
    fn test_netproxy_manager_not_running_initially() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = NetProxyManager::new(dir.path());
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_netproxy_manager_stop_when_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = NetProxyManager::new(dir.path());
        mgr.stop(); // must not panic
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_netproxy_manager_drop_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sockets").join("net.sock");
        std::fs::create_dir_all(dir.path().join("sockets")).unwrap();
        std::fs::write(&socket_path, "fake").unwrap();
        {
            let _mgr = NetProxyManager::new(dir.path());
            // Drop triggers cleanup
        }
        assert!(!socket_path.exists());
    }

    #[test]
    fn test_parse_port_forwards_valid() {
        let guest = Ipv4Addr::new(10, 89, 0, 2);
        // Use a random high port to avoid conflicts
        let rules = vec!["19988:80".to_string(), "19443:443".to_string()];
        let fwds = parse_port_forwards(&rules, guest).unwrap();
        assert_eq!(fwds.len(), 2);
        assert_eq!(fwds[0].guest_port, 80);
        assert_eq!(fwds[1].guest_port, 443);
    }

    #[test]
    fn test_parse_port_forwards_with_protocol_suffix() {
        let guest = Ipv4Addr::new(10, 89, 0, 2);
        let rules = vec!["19989:80/tcp".to_string()];
        let fwds = parse_port_forwards(&rules, guest).unwrap();
        assert_eq!(fwds[0].guest_port, 80);
    }

    #[test]
    fn test_parse_port_forwards_invalid_format() {
        let guest = Ipv4Addr::new(10, 89, 0, 2);
        assert!(parse_port_forwards(&["notaport".to_string()], guest).is_err());
        assert!(parse_port_forwards(&["abc:80".to_string()], guest).is_err());
        assert!(parse_port_forwards(&["80:xyz".to_string()], guest).is_err());
    }

    #[test]
    fn test_netproxy_manager_spawn_binds_and_releases_host_ports() {
        let dir = tempfile::tempdir().unwrap();
        let probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let host_port = probe.local_addr().unwrap().port();
        drop(probe);

        let mut mgr = NetProxyManager::new(dir.path());
        mgr.spawn(
            Ipv4Addr::new(10, 89, 0, 2),
            Ipv4Addr::new(10, 89, 0, 1),
            24,
            &[Ipv4Addr::new(8, 8, 8, 8)],
            &[format!("{host_port}:80")],
        )
        .unwrap();

        assert!(!port_is_bindable(host_port));

        mgr.stop();

        assert!(port_is_bindable(host_port));
    }
}
