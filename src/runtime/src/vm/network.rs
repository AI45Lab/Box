//! Network setup for VM instances (bridge networking and DNS).

use a3s_box_core::dns::HostEntry;
use a3s_box_core::error::{BoxError, Result};

use crate::vmm::NetworkInstanceConfig;

use super::VmManager;

impl VmManager {
    /// Write `/etc/hostname` when a hostname override is configured.
    pub(crate) fn write_hostname_file(&self, layout: &super::BoxLayout) -> Result<()> {
        let Some(hostname) = self.config.hostname.as_deref() else {
            return Ok(());
        };
        a3s_box_core::dns::validate_hostname(hostname).map_err(BoxError::ConfigError)?;

        let hostname_path = layout.rootfs_path.join("etc/hostname");
        if let Some(parent) = hostname_path.parent() {
            std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
        }
        std::fs::write(&hostname_path, format!("{hostname}\n")).map_err(|e| {
            BoxError::NetworkError(format!(
                "Failed to write {}: {}",
                hostname_path.display(),
                e
            ))
        })?;
        Ok(())
    }

    /// Write `/etc/hosts` for boxes without bridge network peer discovery.
    pub(crate) fn write_standalone_hosts_file(&self, layout: &super::BoxLayout) -> Result<()> {
        let add_hosts = self.parse_add_hosts()?;
        let aliases = self.hostname_aliases(None);
        if aliases.is_empty() && add_hosts.is_empty() {
            return Ok(());
        }

        self.write_hosts_content(layout, None, &aliases, &[], &add_hosts)
    }

    /// Set up bridge networking by looking up the network, spawning passt,
    /// and building the NetworkInstanceConfig for the VM spec.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn setup_bridge_network(
        &mut self,
        network_name: &str,
    ) -> Result<NetworkInstanceConfig> {
        use crate::network::NetworkStore;

        let store = NetworkStore::default_path()?;
        let net_config = store.get(network_name)?.ok_or_else(|| {
            BoxError::NetworkError(format!("network '{}' not found", network_name))
        })?;

        // Find this box's endpoint in the network
        let endpoint = net_config
            .endpoints
            .get(&self.box_id)
            .ok_or_else(|| BoxError::NetworkError(format!(
                "box '{}' is not connected to network '{}'; run with --network or use 'network connect'",
                self.box_id, network_name
            )))?;

        let ip = endpoint.ip_address;
        let gateway = net_config.gateway;

        // Parse prefix length from subnet CIDR
        let prefix_len: u8 = net_config
            .subnet
            .split('/')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        // Parse MAC address from hex string "02:42:0a:58:00:02" into [u8; 6]
        let mac_address = parse_mac(&endpoint.mac_address).map_err(|e| {
            BoxError::NetworkError(format!(
                "invalid MAC address '{}': {}",
                endpoint.mac_address, e
            ))
        })?;

        // Determine DNS servers
        let dns_servers: Vec<std::net::Ipv4Addr> = if !self.config.dns.is_empty() {
            self.config
                .dns
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect()
        } else {
            vec![std::net::Ipv4Addr::new(8, 8, 8, 8)]
        };

        // Spawn platform-specific network backend
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);

        #[cfg(target_os = "linux")]
        let (socket_path, _net_socket_fd, _net_proxy_fd) = {
            let mut passt = crate::network::PasstManager::new(&box_dir);
            passt.spawn(ip, gateway, prefix_len, &dns_servers)?;
            let path = passt.socket_path().to_path_buf();
            self.net_manager = Some(Box::new(passt));
            tracing::info!(network = network_name, ip = %ip, gateway = %gateway, "Bridge networking configured via passt");
            (path, None::<i32>, None::<i32>)
        };

        #[cfg(target_os = "macos")]
        let (socket_path, net_socket_fd, net_proxy_fd) = {
            let mut netproxy = crate::network::NetProxyManager::new(&box_dir);
            netproxy.spawn(ip, gateway, prefix_len, &dns_servers, &self.config.port_map)?;
            let fd = netproxy.net_socket_fd();
            let proxy_fd = netproxy.net_proxy_fd();
            let path = netproxy.socket_path().to_path_buf();
            self.net_manager = Some(Box::new(netproxy));
            tracing::info!(network = network_name, ip = %ip, gateway = %gateway, "Bridge networking configured via built-in netproxy");
            (path, fd, proxy_fd)
        };

        Ok(NetworkInstanceConfig {
            net_socket_path: socket_path,
            #[cfg(target_os = "macos")]
            net_socket_fd,
            #[cfg(target_os = "macos")]
            net_proxy_fd,
            ip_address: ip,
            gateway,
            prefix_len,
            mac_address,
            dns_servers,
        })
    }

    /// Set up bridge networking by looking up the network, spawning passt,
    /// and building the NetworkInstanceConfig for the VM spec.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn setup_bridge_network(
        &mut self,
        _network_name: &str,
    ) -> Result<NetworkInstanceConfig> {
        Err(BoxError::NetworkError(
            "Bridge networking (--network) is not supported on this platform".to_string(),
        ))
    }

    /// Write /etc/hosts to the guest rootfs for DNS service discovery.
    ///
    /// Looks up the box's own endpoint and all peer endpoints in the network,
    /// then generates a hosts file mapping IPs to box names.
    pub(crate) fn write_hosts_file(
        &self,
        layout: &super::BoxLayout,
        network_name: &str,
    ) -> Result<()> {
        use crate::network::NetworkStore;

        let store = NetworkStore::default_path()?;
        let net_config = store.get(network_name)?.ok_or_else(|| {
            BoxError::NetworkError(format!("network '{}' not found", network_name))
        })?;

        let endpoint = net_config.endpoints.get(&self.box_id).ok_or_else(|| {
            BoxError::NetworkError(format!(
                "box '{}' not connected to network '{}'",
                self.box_id, network_name
            ))
        })?;

        let own_ip = endpoint.ip_address.to_string();
        let own_names = self.hostname_aliases(Some(&endpoint.box_name));
        let peers = net_config.peer_endpoints(&self.box_id);
        let add_hosts = self.parse_add_hosts()?;

        self.write_hosts_content(layout, Some(&own_ip), &own_names, &peers, &add_hosts)?;

        Ok(())
    }

    fn parse_add_hosts(&self) -> Result<Vec<HostEntry>> {
        a3s_box_core::dns::parse_add_host_entries(&self.config.add_hosts)
            .map_err(BoxError::ConfigError)
    }

    fn hostname_aliases(&self, default_name: Option<&str>) -> Vec<String> {
        let mut aliases = Vec::new();
        if let Some(default_name) = default_name.filter(|name| !name.is_empty()) {
            aliases.push(default_name.to_string());
        }
        if let Some(hostname) = self
            .config
            .hostname
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            if !aliases.iter().any(|alias| alias == hostname) {
                aliases.push(hostname.to_string());
            }
        }
        aliases
    }

    fn write_hosts_content(
        &self,
        layout: &super::BoxLayout,
        own_ip: Option<&str>,
        own_names: &[String],
        peers: &[(String, String)],
        add_hosts: &[HostEntry],
    ) -> Result<()> {
        let hosts_content = a3s_box_core::dns::generate_hosts_file_with_entries(
            own_ip, own_names, peers, add_hosts,
        );
        let hosts_path = layout.rootfs_path.join("etc/hosts");
        if let Some(parent) = hosts_path.parent() {
            std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
        }
        std::fs::write(&hosts_path, &hosts_content).map_err(|e| {
            BoxError::NetworkError(format!("Failed to write {}: {}", hosts_path.display(), e))
        })?;
        tracing::debug!(hosts = %hosts_content.trim(), "Configured guest /etc/hosts for DNS discovery");
        Ok(())
    }
}

/// Parse a MAC address string "02:42:0a:58:00:02" into [u8; 6].
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) fn parse_mac(mac_str: &str) -> std::result::Result<[u8; 6], String> {
    let parts: Vec<&str> = mac_str.split(':').collect();
    if parts.len() != 6 {
        return Err(format!("expected 6 octets, got {}", parts.len()));
    }

    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] =
            u8::from_str_radix(part, 16).map_err(|e| format!("invalid octet '{}': {}", part, e))?;
    }
    Ok(mac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_layout(rootfs_path: std::path::PathBuf) -> super::super::BoxLayout {
        super::super::BoxLayout {
            rootfs_path,
            exec_socket_path: std::path::PathBuf::new(),
            pty_socket_path: std::path::PathBuf::new(),
            attest_socket_path: std::path::PathBuf::new(),
            port_forward_socket_path: std::path::PathBuf::new(),
            workspace_path: std::path::PathBuf::new(),
            console_output: None,
            oci_config: None,
            tee_instance_config: None,
        }
    }

    #[test]
    fn test_parse_mac_valid() {
        let mac = parse_mac("02:42:0a:58:00:02").unwrap();
        assert_eq!(mac, [0x02, 0x42, 0x0a, 0x58, 0x00, 0x02]);
    }

    #[test]
    fn test_parse_mac_all_zeros() {
        let mac = parse_mac("00:00:00:00:00:00").unwrap();
        assert_eq!(mac, [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_parse_mac_all_ff() {
        let mac = parse_mac("ff:ff:ff:ff:ff:ff").unwrap();
        assert_eq!(mac, [0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn test_parse_mac_too_few_octets() {
        assert!(parse_mac("02:42:0a").is_err());
    }

    #[test]
    fn test_parse_mac_too_many_octets() {
        assert!(parse_mac("02:42:0a:58:00:02:ff").is_err());
    }

    #[test]
    fn test_parse_mac_invalid_hex() {
        assert!(parse_mac("02:42:zz:58:00:02").is_err());
    }

    #[test]
    fn test_hostname_aliases_include_default_and_override() {
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig {
                hostname: Some("web".to_string()),
                ..Default::default()
            },
            a3s_box_core::event::EventEmitter::new(16),
            "box-id".to_string(),
        );

        assert_eq!(
            vm.hostname_aliases(Some("box-name")),
            vec!["box-name", "web"]
        );
    }

    #[test]
    fn test_write_standalone_hosts_file_with_hostname_and_add_host() {
        let dir = TempDir::new().unwrap();
        let layout = test_layout(dir.path().join("rootfs"));
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig {
                hostname: Some("web".to_string()),
                add_hosts: vec!["db.local:10.88.0.10".to_string()],
                ..Default::default()
            },
            a3s_box_core::event::EventEmitter::new(16),
            "box-id".to_string(),
        );

        vm.write_standalone_hosts_file(&layout).unwrap();

        let hosts = std::fs::read_to_string(layout.rootfs_path.join("etc/hosts")).unwrap();
        assert!(hosts.contains("127.0.1.1 web"));
        assert!(hosts.contains("10.88.0.10 db.local"));
    }

    #[test]
    fn test_write_hostname_file() {
        let dir = TempDir::new().unwrap();
        let layout = test_layout(dir.path().join("rootfs"));
        let vm = VmManager::with_box_id(
            a3s_box_core::config::BoxConfig {
                hostname: Some("web".to_string()),
                ..Default::default()
            },
            a3s_box_core::event::EventEmitter::new(16),
            "box-id".to_string(),
        );

        vm.write_hostname_file(&layout).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.rootfs_path.join("etc/hostname")).unwrap(),
            "web\n"
        );
    }
}
