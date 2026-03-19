//! Guest network configuration for passt-based virtio-net interfaces.
//!
//! When a VM is started with `--network`, libkrun adds a virtio-net device
//! connected to passt. The guest init detects the interface and configures
//! it with the IP/gateway/DNS passed via environment variables.
//!
//! Environment variables:
//! - `A3S_NET_IP`: IPv4 address with prefix (e.g., "10.88.0.2/24")
//! - `A3S_NET_GATEWAY`: Gateway IPv4 address (e.g., "10.88.0.1")
//! - `A3S_NET_DNS`: Comma-separated DNS servers (e.g., "8.8.8.8,8.8.4.4")

use std::fmt;
use tracing::info;

/// Network configuration parsed from environment variables.
#[derive(Debug, Clone)]
pub struct GuestNetConfig {
    /// IP address with prefix length (e.g., "10.88.0.2/24").
    pub ip_cidr: String,
    /// Gateway address.
    pub gateway: String,
    /// DNS servers.
    pub dns_servers: Vec<String>,
}

/// Errors during guest network setup.
#[derive(Debug)]
pub enum NetError {
    /// Missing required environment variable.
    MissingEnv(String),
    /// Failed to execute ip command.
    CommandFailed(String),
    /// Failed to write resolv.conf.
    ResolvConf(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::MissingEnv(var) => write!(f, "missing env var: {}", var),
            NetError::CommandFailed(msg) => write!(f, "network command failed: {}", msg),
            NetError::ResolvConf(msg) => write!(f, "resolv.conf error: {}", msg),
        }
    }
}

impl std::error::Error for NetError {}

impl GuestNetConfig {
    /// Parse network configuration from environment variables.
    /// Returns None if A3S_NET_IP is not set (TSI mode, no network setup needed).
    pub fn from_env() -> Option<Self> {
        let ip_cidr = std::env::var("A3S_NET_IP").ok()?;
        let gateway = std::env::var("A3S_NET_GATEWAY").unwrap_or_default();
        let dns_servers: Vec<String> = std::env::var("A3S_NET_DNS")
            .map(|s| s.split(',').map(|d| d.trim().to_string()).collect())
            .unwrap_or_else(|_| vec!["8.8.8.8".to_string()]);

        Some(Self {
            ip_cidr,
            gateway,
            dns_servers,
        })
    }
}

/// Configure the guest network interface.
///
/// This function:
/// 1. Always brings up lo (loopback) — required for listen() even in TSI mode
/// 2. If A3S_NET_IP is set (passt mode):
///    a. Assigns IP to eth0
///    b. Brings up eth0
///    c. Adds default route via gateway
///    d. Writes /etc/resolv.conf
pub fn configure_guest_network() -> Result<(), Box<dyn std::error::Error>> {
    // Always bring up loopback — needed for listen() on 0.0.0.0 even in TSI mode
    #[cfg(target_os = "linux")]
    {
        info!("Bringing up loopback interface");
        if let Err(e) = set_interface_up("lo") {
            tracing::warn!("Failed to bring up loopback: {}", e);
        }
    }

    let config = match GuestNetConfig::from_env() {
        Some(c) => c,
        None => {
            info!("No A3S_NET_IP set, using TSI networking");
            return Ok(());
        }
    };

    info!(
        ip = %config.ip_cidr,
        gateway = %config.gateway,
        dns = ?config.dns_servers,
        "Configuring guest network"
    );

    #[cfg(target_os = "linux")]
    {
        configure_interfaces(&config)?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Skipping network configuration on non-Linux platform (development mode)");
        let _ = config;
    }

    Ok(())
}

/// Configure network interfaces using ip commands via /proc/sys/net.
///
/// We use direct syscalls via libc/nix instead of shelling out to `ip`,
/// since the guest rootfs may not have iproute2 installed.
#[cfg(target_os = "linux")]
fn configure_interfaces(config: &GuestNetConfig) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Bring up loopback
    info!("Bringing up loopback interface");
    set_interface_up("lo")?;

    // Step 2: Wait for eth0 to appear (virtio-net driver may need a moment to probe).
    // On macOS/krun the device activation is asynchronous, so we retry for up to 10s.
    let eth0_ready = {
        let mut found = false;
        for attempt in 0..100 {
            // Log what interfaces exist every 10 attempts (1s)
            if attempt % 10 == 0 {
                if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
                    let names: Vec<String> = entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                    info!(interfaces = ?names, attempt, "Waiting for eth0 — current interfaces");
                }
            }
            if interface_exists("eth0") {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        found
    };
    if !eth0_ready {
        // Log final state of interfaces before failing
        if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
            let names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            tracing::error!(interfaces = ?names, "Final interface list when eth0 not found");
        }
        return Err(Box::new(NetError::CommandFailed(
            "eth0 not found after 10s — expected virtio-net interface from network backend"
                .to_string(),
        )));
    }

    // Step 3: Assign IP address to eth0
    info!(ip = %config.ip_cidr, "Assigning IP to eth0");
    add_address("eth0", &config.ip_cidr)?;

    // Step 4: Bring up eth0
    info!("Bringing up eth0");
    set_interface_up("eth0")?;

    // Step 5: Add default route via gateway
    if !config.gateway.is_empty() {
        info!(gateway = %config.gateway, "Adding default route");
        add_default_route(&config.gateway)?;
    }

    // Step 6: Write /etc/resolv.conf
    info!(dns = ?config.dns_servers, "Writing /etc/resolv.conf");
    write_resolv_conf(&config.dns_servers)?;

    info!("Guest network configuration complete");
    Ok(())
}

/// Check if a network interface exists by reading /sys/class/net/.
#[cfg(target_os = "linux")]
fn interface_exists(name: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/net/{}", name)).exists()
}

/// Bring a network interface up using ioctl SIOCSIFFLAGS.
#[cfg(target_os = "linux")]
fn set_interface_up(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::CString;

    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(Box::new(NetError::CommandFailed(
            "failed to create socket for ioctl".to_string(),
        )));
    }

    let ifname = CString::new(name)?;
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };

    // Copy interface name
    let name_bytes = ifname.as_bytes();
    let copy_len = name_bytes.len().min(libc::IFNAMSIZ - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr(),
            ifr.ifr_name.as_mut_ptr() as *mut u8,
            copy_len,
        );
    }

    // Get current flags
    if unsafe { libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) } < 0 {
        unsafe { libc::close(sock) };
        return Err(Box::new(NetError::CommandFailed(format!(
            "SIOCGIFFLAGS failed for {}",
            name
        ))));
    }

    // Set IFF_UP flag
    unsafe {
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16;
    }

    if unsafe { libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) } < 0 {
        unsafe { libc::close(sock) };
        return Err(Box::new(NetError::CommandFailed(format!(
            "SIOCSIFFLAGS failed for {}",
            name
        ))));
    }

    unsafe { libc::close(sock) };
    Ok(())
}

/// Add an IPv4 address to an interface using ioctl SIOCSIFADDR + SIOCSIFNETMASK.
#[cfg(target_os = "linux")]
fn add_address(ifname: &str, ip_cidr: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::CString;
    use std::net::Ipv4Addr;

    // Parse "10.88.0.2/24"
    let parts: Vec<&str> = ip_cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(Box::new(NetError::CommandFailed(format!(
            "invalid IP CIDR: {}",
            ip_cidr
        ))));
    }
    let ip: Ipv4Addr = parts[0].parse()?;
    let prefix: u8 = parts[1].parse()?;

    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(Box::new(NetError::CommandFailed(
            "failed to create socket".to_string(),
        )));
    }

    let if_cstr = CString::new(ifname)?;
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    let name_bytes = if_cstr.as_bytes();
    let copy_len = name_bytes.len().min(libc::IFNAMSIZ - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr(),
            ifr.ifr_name.as_mut_ptr() as *mut u8,
            copy_len,
        );
    }

    // Set IP address
    let addr = sockaddr_in(ip);
    unsafe {
        std::ptr::copy_nonoverlapping(
            &addr as *const libc::sockaddr_in as *const u8,
            &mut ifr.ifr_ifru as *mut _ as *mut u8,
            std::mem::size_of::<libc::sockaddr_in>(),
        );
    }
    if unsafe { libc::ioctl(sock, libc::SIOCSIFADDR as _, &ifr) } < 0 {
        unsafe { libc::close(sock) };
        return Err(Box::new(NetError::CommandFailed(format!(
            "SIOCSIFADDR failed for {}: {}",
            ifname, ip
        ))));
    }

    // Set netmask
    let mask = prefix_to_netmask(prefix);
    let mask_addr = sockaddr_in(mask);
    unsafe {
        std::ptr::copy_nonoverlapping(
            &mask_addr as *const libc::sockaddr_in as *const u8,
            &mut ifr.ifr_ifru as *mut _ as *mut u8,
            std::mem::size_of::<libc::sockaddr_in>(),
        );
    }
    if unsafe { libc::ioctl(sock, libc::SIOCSIFNETMASK as _, &ifr) } < 0 {
        unsafe { libc::close(sock) };
        return Err(Box::new(NetError::CommandFailed(format!(
            "SIOCSIFNETMASK failed for {}: /{}",
            ifname, prefix
        ))));
    }

    unsafe { libc::close(sock) };
    Ok(())
}

/// Add a default route via the given gateway using netlink (rtnetlink).
/// Falls back to writing /proc/sys/net if netlink is unavailable.
#[cfg(target_os = "linux")]
fn add_default_route(gateway: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::net::Ipv4Addr;

    let gw: Ipv4Addr = gateway.parse()?;

    // Use raw socket + rtnetlink to add default route
    let sock = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE) };
    if sock < 0 {
        return Err(Box::new(NetError::CommandFailed(
            "failed to create netlink socket".to_string(),
        )));
    }

    // Bind netlink socket
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    sa.nl_pid = 0;
    sa.nl_groups = 0;

    if unsafe {
        libc::bind(
            sock,
            &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    } < 0
    {
        unsafe { libc::close(sock) };
        return Err(Box::new(NetError::CommandFailed(
            "failed to bind netlink socket".to_string(),
        )));
    }

    // Build RTM_NEWROUTE message
    let gw_octets = gw.octets();

    // nlmsghdr + rtmsg + RTA_GATEWAY attr
    let rta_len = 4 + 4; // rta_len(2) + rta_type(2) + 4 bytes IPv4
    let msg_len = std::mem::size_of::<libc::nlmsghdr>() + std::mem::size_of::<RtMsg>() + rta_len;

    let mut buf = vec![0u8; msg_len];

    // nlmsghdr
    let nlh = unsafe { &mut *(buf.as_mut_ptr() as *mut libc::nlmsghdr) };
    nlh.nlmsg_len = msg_len as u32;
    nlh.nlmsg_type = libc::RTM_NEWROUTE;
    nlh.nlmsg_flags = (libc::NLM_F_REQUEST | libc::NLM_F_CREATE | libc::NLM_F_EXCL) as u16;
    nlh.nlmsg_seq = 1;
    nlh.nlmsg_pid = 0;

    // rtmsg
    let rtm_offset = std::mem::size_of::<libc::nlmsghdr>();
    let rtm = unsafe { &mut *(buf.as_mut_ptr().add(rtm_offset) as *mut RtMsg) };
    rtm.rtm_family = libc::AF_INET as u8;
    rtm.rtm_dst_len = 0; // default route
    rtm.rtm_src_len = 0;
    #[allow(clippy::unnecessary_cast)]
    {
        rtm.rtm_table = libc::RT_TABLE_MAIN as u8;
        rtm.rtm_protocol = libc::RTPROT_BOOT as u8;
    }
    rtm.rtm_scope = libc::RT_SCOPE_UNIVERSE;
    #[allow(clippy::unnecessary_cast)]
    {
        rtm.rtm_type = libc::RTN_UNICAST as u8;
    }

    // RTA_GATEWAY attribute
    let rta_offset = rtm_offset + std::mem::size_of::<RtMsg>();
    let rta = unsafe { &mut *(buf.as_mut_ptr().add(rta_offset) as *mut RtAttr) };
    rta.rta_len = rta_len as u16;
    #[allow(clippy::unnecessary_cast)]
    {
        rta.rta_type = libc::RTA_GATEWAY as u16;
    }
    buf[rta_offset + 4..rta_offset + 8].copy_from_slice(&gw_octets);

    // Send
    let sent = unsafe { libc::send(sock, buf.as_ptr() as *const _, buf.len(), 0) };

    unsafe { libc::close(sock) };

    if sent < 0 {
        return Err(Box::new(NetError::CommandFailed(format!(
            "failed to send RTM_NEWROUTE for gateway {}",
            gateway
        ))));
    }

    Ok(())
}

/// Minimal rtmsg struct for netlink route messages.
#[cfg(target_os = "linux")]
#[repr(C)]
struct RtMsg {
    rtm_family: u8,
    rtm_dst_len: u8,
    rtm_src_len: u8,
    rtm_tos: u8,
    rtm_table: u8,
    rtm_protocol: u8,
    rtm_scope: u8,
    rtm_type: u8,
    rtm_flags: u32,
}

/// Minimal rtattr struct for netlink attributes.
#[cfg(target_os = "linux")]
#[repr(C)]
struct RtAttr {
    rta_len: u16,
    rta_type: u16,
}

/// Create a sockaddr_in from an Ipv4Addr.
#[cfg(target_os = "linux")]
fn sockaddr_in(ip: std::net::Ipv4Addr) -> libc::sockaddr_in {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr.sin_family = libc::AF_INET as u16;
    addr.sin_addr.s_addr = u32::from(ip).to_be();
    addr
}

/// Convert a prefix length to a netmask Ipv4Addr.
#[cfg(target_os = "linux")]
fn prefix_to_netmask(prefix: u8) -> std::net::Ipv4Addr {
    if prefix == 0 {
        return std::net::Ipv4Addr::new(0, 0, 0, 0);
    }
    let mask = !((1u32 << (32 - prefix)) - 1);
    std::net::Ipv4Addr::from(mask)
}

/// Write /etc/resolv.conf with the given DNS servers.
#[cfg(target_os = "linux")]
fn write_resolv_conf(dns_servers: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut content = String::from("# Generated by a3s-box guest init\n");
    for server in dns_servers {
        content.push_str(&format!("nameserver {}\n", server));
    }

    std::fs::write("/etc/resolv.conf", &content).map_err(|e| {
        Box::new(NetError::ResolvConf(format!(
            "failed to write /etc/resolv.conf: {}",
            e
        ))) as Box<dyn std::error::Error>
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_guest_net_config_from_env_none() {
        // Without A3S_NET_IP, should return None
        std::env::remove_var("A3S_NET_IP");
        assert!(GuestNetConfig::from_env().is_none());
    }

    #[test]
    #[serial]
    fn test_guest_net_config_from_env_with_ip() {
        std::env::set_var("A3S_NET_IP", "10.88.0.2/24");
        std::env::set_var("A3S_NET_GATEWAY", "10.88.0.1");
        std::env::set_var("A3S_NET_DNS", "8.8.8.8,1.1.1.1");

        let config = GuestNetConfig::from_env().unwrap();
        assert_eq!(config.ip_cidr, "10.88.0.2/24");
        assert_eq!(config.gateway, "10.88.0.1");
        assert_eq!(config.dns_servers, vec!["8.8.8.8", "1.1.1.1"]);

        // Cleanup
        std::env::remove_var("A3S_NET_IP");
        std::env::remove_var("A3S_NET_GATEWAY");
        std::env::remove_var("A3S_NET_DNS");
    }

    #[test]
    #[serial]
    fn test_guest_net_config_default_dns() {
        std::env::set_var("A3S_NET_IP", "10.88.0.2/24");
        std::env::remove_var("A3S_NET_DNS");

        let config = GuestNetConfig::from_env().unwrap();
        assert_eq!(config.dns_servers, vec!["8.8.8.8"]);

        std::env::remove_var("A3S_NET_IP");
    }

    #[test]
    fn test_net_error_display() {
        let e = NetError::MissingEnv("A3S_NET_IP".to_string());
        assert_eq!(e.to_string(), "missing env var: A3S_NET_IP");

        let e = NetError::CommandFailed("ioctl failed".to_string());
        assert!(e.to_string().contains("ioctl failed"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_prefix_to_netmask() {
        use std::net::Ipv4Addr;
        assert_eq!(prefix_to_netmask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(prefix_to_netmask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(prefix_to_netmask(8), Ipv4Addr::new(255, 0, 0, 0));
        assert_eq!(prefix_to_netmask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(prefix_to_netmask(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(prefix_to_netmask(28), Ipv4Addr::new(255, 255, 255, 240));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_sockaddr_in() {
        use std::net::Ipv4Addr;
        let addr = sockaddr_in(Ipv4Addr::new(10, 88, 0, 1));
        assert_eq!(addr.sin_family, libc::AF_INET as u16);
        // 10.88.0.1 in network byte order
        assert_eq!(
            addr.sin_addr.s_addr,
            u32::from(Ipv4Addr::new(10, 88, 0, 1)).to_be()
        );
    }
}
