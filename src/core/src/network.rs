//! Network types for container-to-container communication.
//!
//! Provides network configuration, endpoint tracking, and IP address
//! management (IPAM) for connecting boxes via passt-based virtio-net.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;

/// Network mode for a box.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// TSI mode (default) — no network interfaces, socket syscalls proxied via vsock.
    #[default]
    Tsi,

    /// Bridge mode — real eth0 via passt, container-to-container communication.
    Bridge {
        /// Network name to join.
        network: String,
    },

    /// No networking at all.
    None,
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkMode::Tsi => write!(f, "tsi"),
            NetworkMode::Bridge { network } => write!(f, "bridge:{}", network),
            NetworkMode::None => write!(f, "none"),
        }
    }
}

/// Configuration for a user-defined network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Network name (unique identifier).
    pub name: String,

    /// Subnet in CIDR notation (e.g., "10.88.0.0/24").
    pub subnet: String,

    /// Gateway IP address (e.g., "10.88.0.1").
    pub gateway: Ipv4Addr,

    /// Network driver (currently only "bridge" is supported).
    #[serde(default = "default_driver")]
    pub driver: String,

    /// User-defined labels.
    #[serde(default)]
    pub labels: HashMap<String, String>,

    /// Connected endpoints (box_id → endpoint).
    #[serde(default)]
    pub endpoints: HashMap<String, NetworkEndpoint>,

    /// Creation timestamp (RFC 3339).
    pub created_at: String,

    /// Network isolation policy.
    #[serde(default)]
    pub policy: NetworkPolicy,
}

fn default_driver() -> String {
    "bridge".to_string()
}

/// A box's connection to a network.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkEndpoint {
    /// Box ID.
    pub box_id: String,

    /// Box name (for DNS resolution).
    pub box_name: String,

    /// Assigned IPv4 address.
    pub ip_address: Ipv4Addr,

    /// Assigned MAC address (hex string, e.g., "02:42:0a:58:00:02").
    pub mac_address: String,
}

/// Network isolation policy.
///
/// Controls which boxes can communicate with each other on a network.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkPolicy {
    /// Isolation mode (default: None — all boxes can communicate).
    #[serde(default)]
    pub isolation: IsolationMode,

    /// Ingress rules (who can receive traffic from whom).
    /// Only used when isolation is `Custom`.
    #[serde(default)]
    pub ingress: Vec<PolicyRule>,

    /// Egress rules (who can send traffic to whom).
    /// Only used when isolation is `Custom`.
    #[serde(default)]
    pub egress: Vec<PolicyRule>,
}

/// Network isolation mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// No isolation — all boxes on the network can communicate freely (default).
    #[default]
    None,
    /// Strict isolation — no box-to-box communication allowed (only gateway/external).
    Strict,
    /// Custom rules — use ingress/egress rules to control traffic.
    Custom,
}

/// A network policy rule that allows traffic between specific boxes or ports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Source box name pattern (e.g., "web", "*" for any).
    #[serde(default = "wildcard")]
    pub from: String,

    /// Destination box name pattern (e.g., "db", "*" for any).
    #[serde(default = "wildcard")]
    pub to: String,

    /// Allowed ports (empty = all ports).
    #[serde(default)]
    pub ports: Vec<u16>,

    /// Protocol: "tcp", "udp", or "any" (default).
    #[serde(default = "default_protocol")]
    pub protocol: String,

    /// Rule action: allow or deny.
    #[serde(default)]
    pub action: PolicyAction,
}

/// Policy rule action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Allow the traffic (default).
    #[default]
    Allow,
    /// Deny the traffic.
    Deny,
}

fn wildcard() -> String {
    "*".to_string()
}

fn default_protocol() -> String {
    "any".to_string()
}

impl NetworkPolicy {
    /// Validate that the policy can be enforced at runtime.
    ///
    /// Currently only `IsolationMode::None` is supported. Strict and Custom
    /// modes require iptables/nftables integration which is not yet implemented.
    /// Rejecting early prevents a false sense of security.
    pub fn validate(&self) -> Result<(), String> {
        match self.isolation {
            IsolationMode::None => Ok(()),
            IsolationMode::Strict => Err(
                "network policy isolation mode 'strict' is not yet enforced at runtime; \
                 packets will NOT be filtered. Remove the policy or use isolation=none"
                    .to_string(),
            ),
            IsolationMode::Custom => Err(
                "network policy isolation mode 'custom' is not yet enforced at runtime; \
                 ingress/egress rules will NOT be applied. Remove the policy or use isolation=none"
                    .to_string(),
            ),
        }
    }

    /// Check if a box is allowed to communicate with a peer.
    pub fn is_peer_allowed(&self, box_name: &str, peer_name: &str) -> bool {
        match self.isolation {
            IsolationMode::None => true,
            IsolationMode::Strict => false,
            IsolationMode::Custom => {
                // Check egress rules (from box_name to peer_name)
                self.evaluate_rules(&self.egress, box_name, peer_name)
            }
        }
    }

    /// Evaluate a set of rules. Default-deny: if no rule matches, deny.
    fn evaluate_rules(&self, rules: &[PolicyRule], from: &str, to: &str) -> bool {
        for rule in rules {
            if matches_pattern(&rule.from, from) && matches_pattern(&rule.to, to) {
                return rule.action == PolicyAction::Allow;
            }
        }
        // No matching rule → deny in Custom mode
        false
    }

    /// Get the list of allowed peers for a box, given all peer names.
    pub fn allowed_peers<'a>(
        &self,
        box_name: &str,
        peers: &'a [(String, String)],
    ) -> Vec<&'a (String, String)> {
        peers
            .iter()
            .filter(|(_, peer_name)| self.is_peer_allowed(box_name, peer_name))
            .collect()
    }
}

/// Simple wildcard pattern matching: "*" matches anything, otherwise exact match.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    pattern == "*" || pattern == name
}

/// Simple sequential IPAM (IP Address Management) for a subnet.
#[derive(Debug)]
pub struct Ipam {
    /// Network address (e.g., 10.88.0.0).
    network: Ipv4Addr,
    /// Prefix length (e.g., 24).
    prefix_len: u8,
    /// Gateway (first usable, e.g., 10.88.0.1).
    gateway: Ipv4Addr,
}

impl Ipam {
    /// Create a new IPAM from a CIDR string (e.g., "10.88.0.0/24").
    pub fn new(cidr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = cidr.split('/').collect();
        if parts.len() != 2 {
            return Err(format!("invalid CIDR notation: {}", cidr));
        }

        let network: Ipv4Addr = parts[0]
            .parse()
            .map_err(|e| format!("invalid network address '{}': {}", parts[0], e))?;
        let prefix_len: u8 = parts[1]
            .parse()
            .map_err(|e| format!("invalid prefix length '{}': {}", parts[1], e))?;

        if prefix_len > 30 {
            return Err(format!(
                "prefix length {} too large (max 30 for usable hosts)",
                prefix_len
            ));
        }

        // Gateway is network + 1
        let net_u32 = u32::from(network);
        let gateway = Ipv4Addr::from(net_u32 + 1);

        Ok(Self {
            network,
            prefix_len,
            gateway,
        })
    }

    /// Get the gateway address.
    pub fn gateway(&self) -> Ipv4Addr {
        self.gateway
    }

    /// Get the subnet CIDR string.
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.network, self.prefix_len)
    }

    /// Calculate the broadcast address.
    pub fn broadcast(&self) -> Ipv4Addr {
        let net_u32 = u32::from(self.network);
        let host_bits = 32 - self.prefix_len as u32;
        let broadcast = net_u32 | ((1u32 << host_bits) - 1);
        Ipv4Addr::from(broadcast)
    }

    /// Total number of usable host addresses (excluding network, gateway, broadcast).
    pub fn capacity(&self) -> u32 {
        let host_bits = 32 - self.prefix_len as u32;
        let total = (1u32 << host_bits) - 1; // exclude network address
        total.saturating_sub(2) // exclude gateway and broadcast
    }

    /// Allocate the next available IP, given a set of already-used IPs.
    pub fn allocate(&self, used: &[Ipv4Addr]) -> Result<Ipv4Addr, String> {
        let net_u32 = u32::from(self.network);
        let broadcast_u32 = u32::from(self.broadcast());
        let gateway_u32 = u32::from(self.gateway);

        // Start from network + 2 (skip network and gateway)
        let mut candidate = net_u32 + 2;
        while candidate < broadcast_u32 {
            if candidate != gateway_u32 {
                let ip = Ipv4Addr::from(candidate);
                if !used.contains(&ip) {
                    return Ok(ip);
                }
            }
            candidate += 1;
        }

        Err("no available IP addresses in subnet".to_string())
    }

    /// Generate a deterministic MAC address from an IPv4 address.
    /// Uses the locally-administered prefix 02:42 (same as Docker).
    pub fn mac_from_ip(ip: &Ipv4Addr) -> String {
        let octets = ip.octets();
        format!(
            "02:42:{:02x}:{:02x}:{:02x}:{:02x}",
            octets[0], octets[1], octets[2], octets[3]
        )
    }
}

/// Simple sequential IPAM for IPv6 subnets.
///
/// Supports /64 to /120 prefix lengths. Allocates addresses sequentially
/// starting from network::1 (gateway) + 1.
#[derive(Debug)]
pub struct Ipam6 {
    /// Network address (e.g., fd00::0).
    network: std::net::Ipv6Addr,
    /// Prefix length (e.g., 64).
    prefix_len: u8,
    /// Gateway (network::1).
    gateway: std::net::Ipv6Addr,
}

impl Ipam6 {
    /// Create a new IPv6 IPAM from a CIDR string (e.g., "fd00::/64").
    pub fn new(cidr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = cidr.split('/').collect();
        if parts.len() != 2 {
            return Err(format!("invalid IPv6 CIDR notation: {}", cidr));
        }

        let network: std::net::Ipv6Addr = parts[0]
            .parse()
            .map_err(|e| format!("invalid IPv6 network address '{}': {}", parts[0], e))?;
        let prefix_len: u8 = parts[1]
            .parse()
            .map_err(|e| format!("invalid prefix length '{}': {}", parts[1], e))?;

        if prefix_len > 120 || prefix_len < 64 {
            return Err(format!(
                "IPv6 prefix length {} out of range (64..=120)",
                prefix_len
            ));
        }

        // Gateway is network + 1
        let net_u128 = u128::from(network);
        let gateway = std::net::Ipv6Addr::from(net_u128 + 1);

        Ok(Self {
            network,
            prefix_len,
            gateway,
        })
    }

    /// Get the gateway address.
    pub fn gateway(&self) -> std::net::Ipv6Addr {
        self.gateway
    }

    /// Get the subnet CIDR string.
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.network, self.prefix_len)
    }

    /// Allocate the next available IPv6 address, given a set of already-used IPs.
    pub fn allocate(&self, used: &[std::net::Ipv6Addr]) -> Result<std::net::Ipv6Addr, String> {
        let net_u128 = u128::from(self.network);
        let host_bits = 128 - self.prefix_len as u32;
        let max_host = (1u128 << host_bits) - 1; // broadcast equivalent
        let gateway_u128 = u128::from(self.gateway);

        // Start from network + 2 (skip network and gateway)
        let mut offset = 2u128;
        while offset < max_host {
            let candidate_u128 = net_u128 + offset;
            if candidate_u128 != gateway_u128 {
                let ip = std::net::Ipv6Addr::from(candidate_u128);
                if !used.contains(&ip) {
                    return Ok(ip);
                }
            }
            offset += 1;
        }

        Err("no available IPv6 addresses in subnet".to_string())
    }
}

impl NetworkConfig {
    /// Create a new network with the given name and subnet.
    pub fn new(name: &str, subnet: &str) -> Result<Self, String> {
        let ipam = Ipam::new(subnet)?;

        Ok(Self {
            name: name.to_string(),
            subnet: ipam.cidr(),
            gateway: ipam.gateway(),
            driver: "bridge".to_string(),
            labels: HashMap::new(),
            endpoints: HashMap::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            policy: NetworkPolicy::default(),
        })
    }

    /// Allocate an IP and register a new endpoint for a box.
    pub fn connect(&mut self, box_id: &str, box_name: &str) -> Result<NetworkEndpoint, String> {
        if self.endpoints.contains_key(box_id) {
            return Err(format!(
                "box '{}' is already connected to network '{}'",
                box_id, self.name
            ));
        }

        let ipam = Ipam::new(&self.subnet)?;
        let used: Vec<Ipv4Addr> = self.endpoints.values().map(|e| e.ip_address).collect();
        let ip = ipam.allocate(&used)?;
        let mac = Ipam::mac_from_ip(&ip);

        let endpoint = NetworkEndpoint {
            box_id: box_id.to_string(),
            box_name: box_name.to_string(),
            ip_address: ip,
            mac_address: mac,
        };

        self.endpoints.insert(box_id.to_string(), endpoint.clone());
        Ok(endpoint)
    }

    /// Remove a box from this network.
    pub fn disconnect(&mut self, box_id: &str) -> Result<NetworkEndpoint, String> {
        self.endpoints.remove(box_id).ok_or_else(|| {
            format!(
                "box '{}' is not connected to network '{}'",
                box_id, self.name
            )
        })
    }

    /// Set the network policy, validating that it can be enforced.
    ///
    /// Returns an error if the policy uses an isolation mode that is not
    /// yet implemented at the packet-filtering level.
    pub fn set_policy(&mut self, policy: NetworkPolicy) -> Result<(), String> {
        policy.validate()?;
        self.policy = policy;
        Ok(())
    }

    /// Get all connected endpoints.
    pub fn connected_boxes(&self) -> Vec<&NetworkEndpoint> {
        self.endpoints.values().collect()
    }

    /// Get peer endpoints for DNS discovery (all endpoints except the given box).
    ///
    /// Returns `(ip_address, box_name)` pairs for all endpoints other than `exclude_box_id`.
    pub fn peer_endpoints(&self, exclude_box_id: &str) -> Vec<(String, String)> {
        self.endpoints
            .values()
            .filter(|ep| ep.box_id != exclude_box_id)
            .map(|ep| (ep.ip_address.to_string(), ep.box_name.clone()))
            .collect()
    }

    /// Get peer endpoints filtered by the network's isolation policy.
    ///
    /// Like `peer_endpoints`, but only returns peers that the given box
    /// is allowed to communicate with according to the network policy.
    pub fn allowed_peer_endpoints(&self, exclude_box_id: &str) -> Vec<(String, String)> {
        let box_name = self
            .endpoints
            .get(exclude_box_id)
            .map(|ep| ep.box_name.as_str())
            .unwrap_or("");

        let all_peers = self.peer_endpoints(exclude_box_id);
        self.policy
            .allowed_peers(box_name, &all_peers)
            .into_iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- NetworkMode tests ---

    #[test]
    fn test_network_mode_default_is_tsi() {
        let mode = NetworkMode::default();
        assert_eq!(mode, NetworkMode::Tsi);
    }

    #[test]
    fn test_network_mode_display() {
        assert_eq!(NetworkMode::Tsi.to_string(), "tsi");
        assert_eq!(NetworkMode::None.to_string(), "none");
        assert_eq!(
            NetworkMode::Bridge {
                network: "mynet".to_string()
            }
            .to_string(),
            "bridge:mynet"
        );
    }

    #[test]
    fn test_network_mode_serialization() {
        let mode = NetworkMode::Bridge {
            network: "test-net".to_string(),
        };
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: NetworkMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mode);
    }

    #[test]
    fn test_network_mode_tsi_serialization() {
        let mode = NetworkMode::Tsi;
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: NetworkMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, NetworkMode::Tsi);
    }

    // --- IPAM tests ---

    #[test]
    fn test_ipam_new_valid() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        assert_eq!(ipam.gateway(), Ipv4Addr::new(10, 88, 0, 1));
        assert_eq!(ipam.cidr(), "10.88.0.0/24");
    }

    #[test]
    fn test_ipam_new_slash16() {
        let ipam = Ipam::new("172.20.0.0/16").unwrap();
        assert_eq!(ipam.gateway(), Ipv4Addr::new(172, 20, 0, 1));
    }

    #[test]
    fn test_ipam_invalid_cidr() {
        assert!(Ipam::new("10.88.0.0").is_err());
        assert!(Ipam::new("not-an-ip/24").is_err());
        assert!(Ipam::new("10.88.0.0/33").is_err());
        assert!(Ipam::new("10.88.0.0/31").is_err());
    }

    #[test]
    fn test_ipam_broadcast() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        assert_eq!(ipam.broadcast(), Ipv4Addr::new(10, 88, 0, 255));

        let ipam16 = Ipam::new("172.20.0.0/16").unwrap();
        assert_eq!(ipam16.broadcast(), Ipv4Addr::new(172, 20, 255, 255));
    }

    #[test]
    fn test_ipam_capacity() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        // /24 = 256 total, minus network(1) minus gateway(1) minus broadcast(1) = 253
        assert_eq!(ipam.capacity(), 253);

        let ipam28 = Ipam::new("10.88.0.0/28").unwrap();
        // /28 = 16 total, minus network(1) = 15, minus gateway(1) minus broadcast(1) = 13
        assert_eq!(ipam28.capacity(), 13);
    }

    #[test]
    fn test_ipam_allocate_first() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        let ip = ipam.allocate(&[]).unwrap();
        // First allocation: network+2 (skip network and gateway)
        assert_eq!(ip, Ipv4Addr::new(10, 88, 0, 2));
    }

    #[test]
    fn test_ipam_allocate_sequential() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        let ip1 = ipam.allocate(&[]).unwrap();
        let ip2 = ipam.allocate(&[ip1]).unwrap();
        let ip3 = ipam.allocate(&[ip1, ip2]).unwrap();

        assert_eq!(ip1, Ipv4Addr::new(10, 88, 0, 2));
        assert_eq!(ip2, Ipv4Addr::new(10, 88, 0, 3));
        assert_eq!(ip3, Ipv4Addr::new(10, 88, 0, 4));
    }

    #[test]
    fn test_ipam_allocate_skips_gateway() {
        let ipam = Ipam::new("10.88.0.0/24").unwrap();
        // Gateway is 10.88.0.1, first alloc should be .2
        let ip = ipam.allocate(&[]).unwrap();
        assert_ne!(ip, ipam.gateway());
    }

    #[test]
    fn test_ipam_allocate_exhausted() {
        let ipam = Ipam::new("10.88.0.0/30").unwrap();
        // /30 = 4 total: .0 (network), .1 (gateway), .2 (host), .3 (broadcast)
        // Only 1 usable host
        let ip1 = ipam.allocate(&[]).unwrap();
        assert_eq!(ip1, Ipv4Addr::new(10, 88, 0, 2));

        let result = ipam.allocate(&[ip1]);
        assert!(result.is_err());
    }

    #[test]
    fn test_ipam_mac_from_ip() {
        let ip = Ipv4Addr::new(10, 88, 0, 2);
        assert_eq!(Ipam::mac_from_ip(&ip), "02:42:0a:58:00:02");

        let ip2 = Ipv4Addr::new(192, 168, 1, 100);
        assert_eq!(Ipam::mac_from_ip(&ip2), "02:42:c0:a8:01:64");
    }

    // --- NetworkConfig tests ---

    #[test]
    fn test_network_config_new() {
        let net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        assert_eq!(net.name, "mynet");
        assert_eq!(net.subnet, "10.88.0.0/24");
        assert_eq!(net.gateway, Ipv4Addr::new(10, 88, 0, 1));
        assert_eq!(net.driver, "bridge");
        assert!(net.endpoints.is_empty());
    }

    #[test]
    fn test_network_config_invalid_subnet() {
        assert!(NetworkConfig::new("bad", "invalid").is_err());
    }

    #[test]
    fn test_network_config_connect() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep = net.connect("box-1", "web").unwrap();

        assert_eq!(ep.box_id, "box-1");
        assert_eq!(ep.box_name, "web");
        assert_eq!(ep.ip_address, Ipv4Addr::new(10, 88, 0, 2));
        assert_eq!(ep.mac_address, "02:42:0a:58:00:02");
        assert_eq!(net.endpoints.len(), 1);
    }

    #[test]
    fn test_network_config_connect_multiple() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep1 = net.connect("box-1", "web").unwrap();
        let ep2 = net.connect("box-2", "api").unwrap();

        assert_eq!(ep1.ip_address, Ipv4Addr::new(10, 88, 0, 2));
        assert_eq!(ep2.ip_address, Ipv4Addr::new(10, 88, 0, 3));
        assert_eq!(net.endpoints.len(), 2);
    }

    #[test]
    fn test_network_config_connect_duplicate() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        let result = net.connect("box-1", "web");
        assert!(result.is_err());
    }

    #[test]
    fn test_network_config_disconnect() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();

        let ep = net.disconnect("box-1").unwrap();
        assert_eq!(ep.box_id, "box-1");
        assert!(net.endpoints.is_empty());
    }

    #[test]
    fn test_network_config_disconnect_not_connected() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let result = net.disconnect("box-1");
        assert!(result.is_err());
    }

    #[test]
    fn test_network_config_connected_boxes() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "api").unwrap();

        let boxes = net.connected_boxes();
        assert_eq!(boxes.len(), 2);
    }

    #[test]
    fn test_network_config_ip_reuse_after_disconnect() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let ep1 = net.connect("box-1", "web").unwrap();
        assert_eq!(ep1.ip_address, Ipv4Addr::new(10, 88, 0, 2));

        net.disconnect("box-1").unwrap();

        // After disconnect, the IP should be reusable
        let ep2 = net.connect("box-2", "api").unwrap();
        assert_eq!(ep2.ip_address, Ipv4Addr::new(10, 88, 0, 2));
    }

    #[test]
    fn test_network_config_serialization() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();

        let json = serde_json::to_string(&net).unwrap();
        let parsed: NetworkConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, "mynet");
        assert_eq!(parsed.subnet, "10.88.0.0/24");
        assert_eq!(parsed.endpoints.len(), 1);
        assert!(parsed.endpoints.contains_key("box-1"));
    }

    // --- NetworkEndpoint tests ---

    // --- peer_endpoints tests ---

    #[test]
    fn test_peer_endpoints_excludes_self() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "api").unwrap();
        net.connect("box-3", "db").unwrap();

        let peers = net.peer_endpoints("box-1");
        assert_eq!(peers.len(), 2);
        assert!(peers.iter().all(|(_, name)| name != "web"));
        assert!(peers.iter().any(|(_, name)| name == "api"));
        assert!(peers.iter().any(|(_, name)| name == "db"));
    }

    #[test]
    fn test_peer_endpoints_empty_when_alone() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();

        let peers = net.peer_endpoints("box-1");
        assert!(peers.is_empty());
    }

    #[test]
    fn test_peer_endpoints_returns_all_others() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "api").unwrap();

        let peers = net.peer_endpoints("box-1");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, "10.88.0.3");
        assert_eq!(peers[0].1, "api");
    }

    #[test]
    fn test_peer_endpoints_nonexistent_excludes_nothing() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "api").unwrap();

        let peers = net.peer_endpoints("nonexistent");
        assert_eq!(peers.len(), 2);
    }

    // --- NetworkEndpoint tests ---

    #[test]
    fn test_network_endpoint_serialization() {
        let ep = NetworkEndpoint {
            box_id: "abc123".to_string(),
            box_name: "web".to_string(),
            ip_address: Ipv4Addr::new(10, 88, 0, 2),
            mac_address: "02:42:0a:58:00:02".to_string(),
        };

        let json = serde_json::to_string(&ep).unwrap();
        let parsed: NetworkEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ep);
    }

    // --- NetworkPolicy tests ---

    #[test]
    fn test_network_policy_default_allows_all() {
        let policy = NetworkPolicy::default();
        assert_eq!(policy.isolation, IsolationMode::None);
        assert!(policy.is_peer_allowed("web", "db"));
        assert!(policy.is_peer_allowed("any", "any"));
    }

    #[test]
    fn test_network_policy_strict_denies_all() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Strict,
            ..Default::default()
        };
        assert!(!policy.is_peer_allowed("web", "db"));
        assert!(!policy.is_peer_allowed("any", "any"));
    }

    #[test]
    fn test_network_policy_custom_allow_rule() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![PolicyRule {
                from: "web".to_string(),
                to: "db".to_string(),
                ports: vec![],
                protocol: "any".to_string(),
                action: PolicyAction::Allow,
            }],
            ..Default::default()
        };
        assert!(policy.is_peer_allowed("web", "db"));
        assert!(!policy.is_peer_allowed("web", "redis")); // no rule → deny
        assert!(!policy.is_peer_allowed("api", "db")); // from doesn't match
    }

    #[test]
    fn test_network_policy_custom_wildcard_from() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![PolicyRule {
                from: "*".to_string(),
                to: "db".to_string(),
                ports: vec![],
                protocol: "any".to_string(),
                action: PolicyAction::Allow,
            }],
            ..Default::default()
        };
        assert!(policy.is_peer_allowed("web", "db"));
        assert!(policy.is_peer_allowed("api", "db"));
        assert!(!policy.is_peer_allowed("web", "redis"));
    }

    #[test]
    fn test_network_policy_custom_wildcard_to() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![PolicyRule {
                from: "web".to_string(),
                to: "*".to_string(),
                ports: vec![],
                protocol: "any".to_string(),
                action: PolicyAction::Allow,
            }],
            ..Default::default()
        };
        assert!(policy.is_peer_allowed("web", "db"));
        assert!(policy.is_peer_allowed("web", "redis"));
        assert!(!policy.is_peer_allowed("api", "db"));
    }

    #[test]
    fn test_network_policy_custom_deny_rule() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![
                PolicyRule {
                    from: "web".to_string(),
                    to: "db".to_string(),
                    ports: vec![],
                    protocol: "any".to_string(),
                    action: PolicyAction::Deny,
                },
                PolicyRule {
                    from: "web".to_string(),
                    to: "*".to_string(),
                    ports: vec![],
                    protocol: "any".to_string(),
                    action: PolicyAction::Allow,
                },
            ],
            ..Default::default()
        };
        // First matching rule wins: web→db is denied
        assert!(!policy.is_peer_allowed("web", "db"));
        // web→redis matches the wildcard allow
        assert!(policy.is_peer_allowed("web", "redis"));
    }

    #[test]
    fn test_network_policy_custom_no_rules_denies() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![],
            ..Default::default()
        };
        assert!(!policy.is_peer_allowed("web", "db"));
    }

    #[test]
    fn test_network_policy_allowed_peers() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![PolicyRule {
                from: "web".to_string(),
                to: "db".to_string(),
                ports: vec![],
                protocol: "any".to_string(),
                action: PolicyAction::Allow,
            }],
            ..Default::default()
        };

        let peers = vec![
            ("10.88.0.3".to_string(), "db".to_string()),
            ("10.88.0.4".to_string(), "redis".to_string()),
        ];

        let allowed = policy.allowed_peers("web", &peers);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].1, "db");
    }

    #[test]
    fn test_network_policy_serde_roundtrip() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![PolicyRule {
                from: "web".to_string(),
                to: "db".to_string(),
                ports: vec![5432],
                protocol: "tcp".to_string(),
                action: PolicyAction::Allow,
            }],
            ingress: vec![],
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: NetworkPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.isolation, IsolationMode::Custom);
        assert_eq!(parsed.egress.len(), 1);
        assert_eq!(parsed.egress[0].ports, vec![5432]);
    }

    #[test]
    fn test_isolation_mode_serde() {
        let modes = vec![
            (IsolationMode::None, "\"none\""),
            (IsolationMode::Strict, "\"strict\""),
            (IsolationMode::Custom, "\"custom\""),
        ];
        for (mode, expected) in modes {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, expected);
            let parsed: IsolationMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn test_allowed_peer_endpoints_none_policy() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "db").unwrap();
        net.connect("box-3", "redis").unwrap();

        // Default policy (None) → all peers visible
        let peers = net.allowed_peer_endpoints("box-1");
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn test_allowed_peer_endpoints_strict_policy() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.policy = NetworkPolicy {
            isolation: IsolationMode::Strict,
            ..Default::default()
        };
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "db").unwrap();

        let peers = net.allowed_peer_endpoints("box-1");
        assert!(peers.is_empty());
    }

    #[test]
    fn test_allowed_peer_endpoints_custom_policy() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        net.policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![PolicyRule {
                from: "web".to_string(),
                to: "db".to_string(),
                ports: vec![],
                protocol: "any".to_string(),
                action: PolicyAction::Allow,
            }],
            ..Default::default()
        };
        net.connect("box-1", "web").unwrap();
        net.connect("box-2", "db").unwrap();
        net.connect("box-3", "redis").unwrap();

        let peers = net.allowed_peer_endpoints("box-1");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].1, "db");
    }

    #[test]
    fn test_matches_pattern() {
        assert!(matches_pattern("*", "anything"));
        assert!(matches_pattern("web", "web"));
        assert!(!matches_pattern("web", "api"));
    }

    #[test]
    fn test_policy_action_default() {
        assert_eq!(PolicyAction::default(), PolicyAction::Allow);
    }

    // --- NetworkPolicy::validate tests ---

    #[test]
    fn test_policy_validate_none_ok() {
        let policy = NetworkPolicy::default();
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn test_policy_validate_strict_rejected() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Strict,
            ..Default::default()
        };
        let err = policy.validate().unwrap_err();
        assert!(err.contains("strict"));
        assert!(err.contains("not yet enforced"));
    }

    #[test]
    fn test_policy_validate_custom_rejected() {
        let policy = NetworkPolicy {
            isolation: IsolationMode::Custom,
            egress: vec![PolicyRule {
                from: "web".to_string(),
                to: "db".to_string(),
                ports: vec![],
                protocol: "any".to_string(),
                action: PolicyAction::Allow,
            }],
            ..Default::default()
        };
        let err = policy.validate().unwrap_err();
        assert!(err.contains("custom"));
        assert!(err.contains("not yet enforced"));
    }

    // --- NetworkConfig::set_policy tests ---

    #[test]
    fn test_set_policy_none_ok() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        assert!(net.set_policy(NetworkPolicy::default()).is_ok());
    }

    #[test]
    fn test_set_policy_strict_rejected() {
        let mut net = NetworkConfig::new("mynet", "10.88.0.0/24").unwrap();
        let result = net.set_policy(NetworkPolicy {
            isolation: IsolationMode::Strict,
            ..Default::default()
        });
        assert!(result.is_err());
    }

    // --- Ipam6 tests ---

    #[test]
    fn test_ipam6_new_valid() {
        let ipam = Ipam6::new("fd00::/64").unwrap();
        assert_eq!(ipam.gateway(), "fd00::1".parse::<std::net::Ipv6Addr>().unwrap());
        assert_eq!(ipam.cidr(), "fd00::/64");
    }

    #[test]
    fn test_ipam6_invalid_cidr() {
        assert!(Ipam6::new("fd00::").is_err());
        assert!(Ipam6::new("not-an-ip/64").is_err());
        assert!(Ipam6::new("fd00::/63").is_err()); // below 64
        assert!(Ipam6::new("fd00::/121").is_err()); // above 120
    }

    #[test]
    fn test_ipam6_allocate_first() {
        let ipam = Ipam6::new("fd00::/64").unwrap();
        let ip = ipam.allocate(&[]).unwrap();
        assert_eq!(ip, "fd00::2".parse::<std::net::Ipv6Addr>().unwrap());
    }

    #[test]
    fn test_ipam6_allocate_sequential() {
        let ipam = Ipam6::new("fd00::/64").unwrap();
        let ip1 = ipam.allocate(&[]).unwrap();
        let ip2 = ipam.allocate(&[ip1]).unwrap();
        let ip3 = ipam.allocate(&[ip1, ip2]).unwrap();

        assert_eq!(ip1, "fd00::2".parse::<std::net::Ipv6Addr>().unwrap());
        assert_eq!(ip2, "fd00::3".parse::<std::net::Ipv6Addr>().unwrap());
        assert_eq!(ip3, "fd00::4".parse::<std::net::Ipv6Addr>().unwrap());
    }

    #[test]
    fn test_ipam6_allocate_skips_gateway() {
        let ipam = Ipam6::new("fd00::/64").unwrap();
        let ip = ipam.allocate(&[]).unwrap();
        assert_ne!(ip, ipam.gateway());
    }

    #[test]
    fn test_ipam6_slash120() {
        let ipam = Ipam6::new("fd00::/120").unwrap();
        let ip = ipam.allocate(&[]).unwrap();
        assert_eq!(ip, "fd00::2".parse::<std::net::Ipv6Addr>().unwrap());
    }
}
